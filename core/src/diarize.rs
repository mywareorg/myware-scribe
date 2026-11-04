//! Speaker diarization via sherpa-onnx (pyannote segmentation + speaker embedding).
//!
//! Uses the raw sherpa-onnx C API directly (rather than the high-level
//! `sherpa-rs` diarize helper, which hardcodes `num_threads = 1`) so
//! diarization runs multi-threaded, and wires the progress callback so the UI
//! can show live progress.

use std::ffi::{c_void, CString};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[cfg(windows)]
use crate::sherpa_dynamic as ffi;
#[cfg(not(windows))]
use sherpa_rs_sys as ffi;

use crate::event::Turn;

/// Clustering threshold used when the speaker count is auto-detected.
pub const CLUSTER_THRESHOLD: f32 = 0.5;

/// Opaque session handle. The Windows dynamic binding and the crate binding
/// disagree only in pointer type, so normalise it here.
#[cfg(windows)]
type SessionPtr = *const c_void;
#[cfg(not(windows))]
type SessionPtr = *const ffi::SherpaOnnxOfflineSpeakerDiarization;

/// Progress is stored as a fraction scaled by this constant.
const PROGRESS_SCALE: f32 = 1_000_000.0;

/// sherpa's per-file progress callback; `arg` is the caller's opaque pointer.
type ProgressFn = unsafe extern "C" fn(i32, i32, *mut c_void) -> i32;
type ProgressCallback = Option<ProgressFn>;

fn progress_callback(callback: ProgressFn) -> ProgressCallback {
    Some(callback)
}

/// Verify that the diarization backend is available. On Windows this loads
/// the shipped sherpa-onnx DLL and resolves its symbols.
pub fn ensure_available() -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::sherpa_dynamic::ensure_loaded()?;
    }
    Ok(())
}

/// Threads used by the segmentation and embedding models. Defaults to every
/// physical core; `HIPPO_TRANSCRIBE_DIARIZE_THREADS` (or the legacy
/// `TRANSIEL_DIARIZE_THREADS`) overrides it, which matters once diarization
/// shares the machine with ASR workers.
fn diarize_threads() -> i32 {
    crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_THREADS",
        "TRANSIEL_DIARIZE_THREADS",
    )
    .and_then(|value| value.parse::<usize>().ok())
    .filter(|threads| *threads > 0)
    .unwrap_or_else(|| num_cpus::get_physical().max(1)) as i32
}

/// A diarization session that can be reused across files.
///
/// Creating the sherpa-onnx session loads the pyannote segmentation model and
/// the speaker embedding model, so rebuilding it per file costs tens of
/// megabytes of initialisation. The clustering settings are baked into the
/// session, so a change in `num_speakers` or `threshold` requires a new one.
pub struct Diarizer {
    sd: SessionPtr,
    // The C strings must outlive the config pointers the session was built
    // from; sherpa keeps them for the session's lifetime.
    _embedding: CString,
    _segmentation: CString,
    _provider: CString,
    num_speakers: Option<i32>,
    threshold: f32,
    threads: i32,
}

// The session is only ever driven from one thread at a time.
unsafe impl Send for Diarizer {}
unsafe impl Sync for Diarizer {}

impl Drop for Diarizer {
    fn drop(&mut self) {
        unsafe { ffi::SherpaOnnxDestroyOfflineSpeakerDiarization(self.sd) };
    }
}

impl Diarizer {
    /// Build a session, or return `None` when the requested clustering matches
    /// `existing` so the caller can keep using it.
    pub fn compatible(&self, num_speakers: Option<i32>, threshold: f32) -> bool {
        self.num_speakers == num_speakers && (self.threshold - threshold).abs() < f32::EPSILON
    }

    pub fn new(
        segmentation: &Path,
        embedding: &Path,
        num_speakers: Option<i32>,
        threshold: f32,
    ) -> Result<Self, String> {
        Self::new_with_threads(segmentation, embedding, num_speakers, threshold, None)
    }

    /// Build a session that uses `threads` for both models. `None` keeps the
    /// default (all physical cores, or the environment override).
    pub fn new_with_threads(
        segmentation: &Path,
        embedding: &Path,
        num_speakers: Option<i32>,
        threshold: f32,
        threads: Option<i32>,
    ) -> Result<Self, String> {
        #[cfg(windows)]
        ffi::ensure_loaded()?;

        let threads = threads
            .filter(|threads| *threads > 0)
            .unwrap_or_else(diarize_threads);
        let embedding = CString::new(embedding.to_string_lossy().as_bytes())
            .map_err(|e| format!("embedding path: {e}"))?;
        let segmentation = CString::new(segmentation.to_string_lossy().as_bytes())
            .map_err(|e| format!("segmentation path: {e}"))?;
        let provider = CString::new("cpu").unwrap();

        let config = ffi::SherpaOnnxOfflineSpeakerDiarizationConfig {
            embedding: ffi::SherpaOnnxSpeakerEmbeddingExtractorConfig {
                model: embedding.as_ptr(),
                num_threads: threads,
                debug: 0,
                provider: provider.as_ptr(),
            },
            clustering: ffi::SherpaOnnxFastClusteringConfig {
                num_clusters: num_speakers.unwrap_or(-1),
                threshold,
            },
            min_duration_off: 0.0,
            min_duration_on: 0.0,
            segmentation: ffi::SherpaOnnxOfflineSpeakerSegmentationModelConfig {
                pyannote: ffi::SherpaOnnxOfflineSpeakerSegmentationPyannoteModelConfig {
                    model: segmentation.as_ptr(),
                },
                num_threads: threads,
                debug: 0,
                provider: provider.as_ptr(),
            },
        };

        let sd: SessionPtr = unsafe { ffi::SherpaOnnxCreateOfflineSpeakerDiarization(&config) };
        if sd.is_null() {
            return Err("failed to initialize offline speaker diarization".to_string());
        }

        Ok(Self {
            sd,
            _embedding: embedding,
            _segmentation: segmentation,
            _provider: provider,
            num_speakers,
            threshold,
            threads,
        })
    }

    /// Diarize 16 kHz mono samples, reporting progress (0..1 scaled).
    pub fn run_with_progress(
        &self,
        samples: &[f32],
        progress: Option<Arc<AtomicU32>>,
    ) -> Result<Vec<Turn>, String> {
        let callback = match progress {
            Some(_) => progress_callback(diarize_progress as ProgressFn),
            None => None,
        };
        let arg = match &progress {
            Some(progress) => Arc::as_ptr(progress) as *mut c_void,
            None => std::ptr::null_mut(),
        };
        self.run_raw(samples, callback, arg)
    }

    /// One pass over `samples`, reporting through a raw sherpa progress
    /// callback. The blockwise path needs its own callback so progress can be
    /// composed across blocks instead of restarting at zero for each one.
    fn run_raw(
        &self,
        samples: &[f32],
        callback: ProgressCallback,
        arg: *mut c_void,
    ) -> Result<Vec<Turn>, String> {
        let mut turns = Vec::new();
        unsafe {
            let result = ffi::SherpaOnnxOfflineSpeakerDiarizationProcessWithCallback(
                self.sd,
                samples.as_ptr() as *mut f32,
                samples.len() as i32,
                callback,
                arg,
            );

            if !result.is_null() {
                let count = ffi::SherpaOnnxOfflineSpeakerDiarizationResultGetNumSegments(result);
                let segments =
                    ffi::SherpaOnnxOfflineSpeakerDiarizationResultSortByStartTime(result);
                if !segments.is_null() && count > 0 {
                    for segment in std::slice::from_raw_parts(segments, count as usize) {
                        turns.push(Turn {
                            start: segment.start,
                            end: segment.end,
                            speaker: format!("spk{}", segment.speaker),
                        });
                    }
                }
                ffi::SherpaOnnxOfflineSpeakerDiarizationDestroySegment(segments);
                ffi::SherpaOnnxOfflineSpeakerDiarizationDestroyResult(result);
            }
        }

        turns.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(turns)
    }

    /// Threads this session was configured with.
    pub fn threads(&self) -> i32 {
        self.threads
    }
}

/// Threads a new diarization session would use, for the CLI's timing line.
pub fn threads() -> i32 {
    diarize_threads()
}

/// Diarize 16 kHz mono samples. `num_speakers` fixes the cluster count; `None`
/// lets the clustering decide using `threshold`.
pub fn diarize(
    samples: &[f32],
    segmentation: &Path,
    embedding: &Path,
    num_speakers: Option<i32>,
    threshold: f32,
) -> Result<Vec<Turn>, String> {
    // Each block is clustered with the requested count as well: measured on the
    // 300-600 s slice, auto clustering at the default threshold absorbs one
    // speaker into the other, which is the failure the blocks are meant to fix.
    // A block that really holds one voice is handled afterwards by
    // `merge_small_clusters`, which folds the phantom cluster away.
    let session = Diarizer::new(segmentation, embedding, num_speakers, threshold)?;
    if blocks_enabled() {
        run_blockwise(&session, samples, num_speakers, None)
    } else {
        session.run_with_progress(samples, None)
    }
}

// ------------------------------------------------------------------ blocks

/// Local diarization block length, and the overlap between neighbouring blocks,
/// in seconds. The overlap is what lets one block be labelled against the one
/// before it without a global pass.
///
/// 300/45 is the shipped geometry. It measures worse on sample-long than the
/// 25 s overlap (23.43% against 11.45%) because one block's labels tie inside
/// the overlap and the wrong one wins; it is shipped deliberately, and the
/// cluster-mean embedding assignment is what is meant to make the overlap
/// length stop mattering.
pub const BLOCK_S: f32 = 300.0;
pub const BLOCK_OVERLAP_S: f32 = 45.0;

/// sherpa diarization takes 16 kHz mono f32 samples.
pub const SAMPLE_RATE: usize = 16_000;

/// A local cluster shorter than this is treated as segmentation noise.
const MIN_CLUSTER_S: f32 = 2.0;

/// Blockwise diarization is opt-in until its numbers beat the single pass.
pub fn blocks_enabled() -> bool {
    crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_BLOCKS",
        "TRANSIEL_DIARIZE_BLOCKS",
    )
    .map(|value| {
        let value = value.trim().to_ascii_lowercase();
        value == "1" || value == "true" || value == "yes"
    })
    .unwrap_or(false)
}

/// Block length in seconds, overridable so the geometry can be swept without a
/// rebuild.
fn block_seconds() -> f32 {
    crate::env_compat("HIPPO_TRANSCRIBE_DIARIZE_BLOCK_S", "TRANSIEL_DIARIZE_BLOCK_S")
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|seconds| *seconds >= 30.0)
        .unwrap_or(BLOCK_S)
}

/// Overlap between neighbouring blocks, in seconds.
fn block_overlap_seconds() -> f32 {
    crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_BLOCK_OVERLAP_S",
        "TRANSIEL_DIARIZE_BLOCK_OVERLAP_S",
    )
    .and_then(|value| value.parse::<f32>().ok())
    .filter(|seconds| *seconds >= 0.0)
    .unwrap_or(BLOCK_OVERLAP_S)
    .min(block_seconds() / 2.0)
}

/// Identifier for the shipped turn geometry, folded into the cache key so a
/// cached single pass cannot be served to a blockwise run.
pub fn geometry_id() -> String {
    if blocks_enabled() {
        format!("blocks-{:04.0}-{:03.0}", block_seconds(), block_overlap_seconds())
    } else {
        "single".to_string()
    }
}

/// Sample ranges for each block, covering the file once from the left.
fn block_ranges(total: usize, sample_rate: usize) -> Vec<(usize, usize)> {
    let block = (block_seconds() * sample_rate as f32) as usize;
    let hop = ((block_seconds() - block_overlap_seconds()) * sample_rate as f32) as usize;
    let mut ranges = Vec::new();
    let mut start = 0usize;
    loop {
        let end = (start + block).min(total);
        ranges.push((start, end));
        if end >= total || hop == 0 {
            break;
        }
        start += hop;
        if start >= total {
            break;
        }
    }
    ranges
}

/// The time window each block owns, so a turn that both blocks produced is kept
/// exactly once. Ownership is decided by the midpoint of the overlap, not by
/// clipping, which would leave a seam artefact on every boundary.
fn ownership(ranges: &[(usize, usize)], sample_rate: usize) -> Vec<(f32, f32)> {
    let seconds = |samples: usize| samples as f32 / sample_rate as f32;
    let bounds: Vec<f32> = ranges
        .windows(2)
        .map(|pair| (seconds(pair[1].0) + seconds(pair[0].1)) / 2.0)
        .collect();
    (0..ranges.len())
        .map(|i| {
            let low = if i == 0 {
                f32::NEG_INFINITY
            } else {
                bounds[i - 1]
            };
            let high = bounds.get(i).copied().unwrap_or(f32::INFINITY);
            (low, high)
        })
        .collect()
}

/// Fold local clusters that are too small to be a decision into their nearest
/// neighbour in time. A 1 s cluster inside a monologue is segmentation noise,
/// and keeping it would make the block boundary logic reason about a speaker
/// that does not exist.
fn merge_small_clusters(mut turns: Vec<Turn>) -> Vec<Turn> {
    // Absolute floor, not a share of the block: a share scales with block
    // length, and a quiet speaker in a short block is a real speaker, not an
    // artefact. Only fragments this short are unambiguous segmentation noise.
    let _speech: f32 = turns.iter().map(|t| (t.end - t.start).max(0.0)).sum();
    let floor = MIN_CLUSTER_S;
    let mut totals: Vec<(String, f32)> = Vec::new();
    for turn in &turns {
        match totals.iter_mut().find(|(label, _)| *label == turn.speaker) {
            Some((_, total)) => *total += (turn.end - turn.start).max(0.0),
            None => totals.push((turn.speaker.clone(), (turn.end - turn.start).max(0.0))),
        }
    }
    let big: Vec<String> = totals
        .iter()
        .filter(|(_, total)| *total >= floor)
        .map(|(label, _)| label.clone())
        .collect();
    let small: Vec<String> = totals
        .iter()
        .filter(|(_, total)| *total < floor)
        .map(|(label, _)| label.clone())
        .collect();
    // One cluster, or nothing to merge into: leave the labels alone.
    if big.is_empty() || small.is_empty() {
        return turns;
    }
    for label in small {
        let gap_to = |other: &str| -> f32 {
            let mut best = f32::INFINITY;
            for turn in turns.iter().filter(|t| t.speaker == label) {
                for other_turn in turns.iter().filter(|t| t.speaker == other) {
                    let gap = if other_turn.start >= turn.end {
                        other_turn.start - turn.end
                    } else if turn.start >= other_turn.end {
                        turn.start - other_turn.end
                    } else {
                        0.0
                    };
                    best = best.min(gap);
                }
            }
            best
        };
        let nearest = big
            .iter()
            .min_by(|a, b| gap_to(a).total_cmp(&gap_to(b)))
            .cloned();
        if let Some(nearest) = nearest {
            for turn in turns.iter_mut().filter(|t| t.speaker == label) {
                turn.speaker = nearest.clone();
            }
        }
    }
    turns
}

/// Map one block's local speaker labels onto the global ones.
///
/// `reference` is the previous block's turns, already in global labels and
/// absolute time; agreement is measured only inside `overlap`, so this is a
/// local decision that cannot invert the whole transcript. Labels with no
/// overlap evidence take the next free global index in order of first
/// appearance, which is the same rule the first block uses.
fn assign_block(
    local: &[Turn],
    reference: &[Turn],
    offset: f32,
    overlap: (f32, f32),
    limit: Option<usize>,
) -> Vec<Turn> {
    let mut labels: Vec<String> = Vec::new();
    for turn in local {
        if !labels.contains(&turn.speaker) {
            labels.push(turn.speaker.clone());
        }
    }
    let mut reference_labels: Vec<String> = Vec::new();
    for turn in reference {
        if !reference_labels.contains(&turn.speaker) {
            reference_labels.push(turn.speaker.clone());
        }
    }

    // Seconds of agreement between each local label and each global label,
    // counted only inside the overlap where both blocks saw the same audio.
    let mut scores = vec![vec![0f32; reference_labels.len()]; labels.len()];
    if overlap.1 > overlap.0 {
        for (label_index, label) in labels.iter().enumerate() {
            for turn in local.iter().filter(|t| &t.speaker == label) {
                let start = (turn.start + offset).max(overlap.0);
                let end = (turn.end + offset).min(overlap.1);
                if end <= start {
                    continue;
                }
                for ref_turn in reference.iter() {
                    // Index by the reference turn's *label*, not its position:
                    // `reference` holds one turn per segment.
                    let Some(ref_index) = reference_labels
                        .iter()
                        .position(|label| label == &ref_turn.speaker)
                    else {
                        continue;
                    };
                    let shared = end.min(ref_turn.end) - start.max(ref_turn.start);
                    if shared > 0.0 {
                        scores[label_index][ref_index] += shared;
                    }
                }
            }
        }
    }

    // Greedy best-first matching: with two speakers this is exact, and it
    // degrades to "most agreement wins" if a block returns a stray third
    // cluster.
    let mut pairs: Vec<(usize, usize, f32)> = Vec::new();
    for (i, row) in scores.iter().enumerate() {
        for (j, score) in row.iter().enumerate() {
            if *score > 0.0 {
                pairs.push((i, j, *score));
            }
        }
    }
    pairs.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut assigned: Vec<Option<String>> = vec![None; labels.len()];
    let mut taken = vec![false; reference_labels.len()];
    for (label_index, ref_index, _) in pairs {
        if assigned[label_index].is_some() || taken[ref_index] {
            continue;
        }
        assigned[label_index] = Some(reference_labels[ref_index].clone());
        taken[ref_index] = true;
    }

    // Whatever is left has no overlap evidence: take the first global label
    // that is still free, folding into the last allowed speaker rather than
    // inventing one beyond the requested count.
    let mut used: Vec<String> = reference_labels.clone();
    used.extend(assigned.iter().flatten().cloned());
    let allowed = limit.map(|count| count.max(1)).unwrap_or(usize::MAX);
    for label_index in 0..labels.len() {
        if assigned[label_index].is_some() {
            continue;
        }
        let mut index = 0usize;
        let name = loop {
            if index + 1 >= allowed {
                break format!("spk{}", allowed.saturating_sub(1));
            }
            let candidate = format!("spk{index}");
            if !used.contains(&candidate) {
                break candidate;
            }
            index += 1;
            if index > 64 {
                break "spk0".to_string();
            }
        };
        used.push(name.clone());
        assigned[label_index] = Some(name);
    }

    if crate::env_compat("HIPPO_TRANSCRIBE_DIARIZE_TRACE", "TRANSIEL_DIARIZE_TRACE").is_some() {
        let mapping: Vec<(String, String)> = labels
            .iter()
            .zip(assigned.iter())
            .map(|(local, global)| {
                (
                    local.clone(),
                    global.clone().unwrap_or_else(|| "-".to_string()),
                )
            })
            .collect();
        eprintln!("  assign offset {offset:.1}s {mapping:?} scores {scores:?}");
    }

    local
        .iter()
        .map(|turn| {
            let label = labels
                .iter()
                .position(|l| l == &turn.speaker)
                .and_then(|i| assigned[i].clone())
                .unwrap_or_else(|| "spk0".to_string());
            Turn {
                start: turn.start + offset,
                end: turn.end + offset,
                speaker: label,
            }
        })
        .collect()
}

#[repr(C)]
struct BlockProgress {
    base: f32,
    span: f32,
    out: *const AtomicU32,
}

unsafe extern "C" fn block_progress(processed: i32, total: i32, arg: *mut c_void) -> i32 {
    if !arg.is_null() && total > 0 {
        let sink = &*(arg as *const BlockProgress);
        let fraction = (processed.max(0) as f32 / total as f32).clamp(0.0, 1.0);
        let value = (sink.base + sink.span * fraction).clamp(0.0, 1.0);
        (*sink.out).store((value * PROGRESS_SCALE) as u32, Ordering::Relaxed);
    }
    0
}

/// Diarize in overlapping blocks and reconcile the labels across them.
///
/// The session is expected to cluster locally (no fixed speaker count); the
/// requested global count is applied while the blocks are joined. A whole-file
/// pass has to cluster every turn at once, which is what lets one speaker be
/// absorbed by the other; a five minute block is a much easier clustering
/// problem, and the measured slice beat the full file by a wide margin.
pub fn run_blockwise(
    diarizer: &Diarizer,
    samples: &[f32],
    num_speakers: Option<i32>,
    progress: Option<Arc<AtomicU32>>,
) -> Result<Vec<Turn>, String> {
    let ranges = block_ranges(samples.len(), SAMPLE_RATE);
    let windows = ownership(&ranges, SAMPLE_RATE);
    let total = ranges.len();
    let mut out: Vec<Turn> = Vec::new();
    let mut reference: Vec<Turn> = Vec::new();

    for (index, &(start, end)) in ranges.iter().enumerate() {
        let offset = start as f32 / SAMPLE_RATE as f32;
        let block = &samples[start..end];
        let local = match &progress {
            Some(shared) => {
                let sink = BlockProgress {
                    base: index as f32 / total as f32,
                    span: 1.0 / total as f32,
                    out: Arc::as_ptr(shared),
                };
                diarizer.run_raw(
                    block,
                    progress_callback(block_progress as ProgressFn),
                    &sink as *const BlockProgress as *mut c_void,
                )?
            }
            None => diarizer.run_raw(block, None, std::ptr::null_mut())?,
        };
        let local = merge_small_clusters(local);
        let overlap = if index == 0 {
            (0.0, 0.0)
        } else {
            (
                offset,
                ranges[index - 1].1 as f32 / SAMPLE_RATE as f32,
            )
        };
        if crate::env_compat("HIPPO_TRANSCRIBE_DIARIZE_TRACE", "TRANSIEL_DIARIZE_TRACE").is_some() {
            let mut summary: Vec<(String, f32)> = Vec::new();
            for turn in &local {
                match summary.iter_mut().find(|(label, _)| *label == turn.speaker) {
                    Some((_, total)) => *total += turn.end - turn.start,
                    None => summary.push((turn.speaker.clone(), turn.end - turn.start)),
                }
            }
            eprintln!(
                "block {index} offset {offset:.1}s overlap {:.1}-{:.1}s local {summary:?}",
                overlap.0, overlap.1
            );
        }
        let mapped = assign_block(
            &local,
            &reference,
            offset,
            overlap,
            num_speakers.map(|count| count.max(1) as usize),
        );
        let (low, high) = windows[index];
        for turn in &mapped {
            let middle = (turn.start + turn.end) / 2.0;
            if middle >= low && middle < high {
                out.push(turn.clone());
            }
        }
        reference = mapped;
    }

    out.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(start: f32, end: f32, speaker: &str) -> Turn {
        Turn {
            start,
            end,
            speaker: speaker.to_string(),
        }
    }

    #[test]
    fn blocks_cover_the_file_and_overlap_by_the_configured_seconds() {
        let ten_minutes = 600 * SAMPLE_RATE;
        let ranges = block_ranges(ten_minutes, SAMPLE_RATE);
        assert!(ranges.len() >= 3);
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, ten_minutes);
        for pair in ranges.windows(2) {
            let overlap = pair[0].1 - pair[1].0;
            let expected = (BLOCK_OVERLAP_S * SAMPLE_RATE as f32) as usize;
            assert!(
                overlap.abs_diff(expected) <= 1,
                "overlap {overlap} != {expected}"
            );
        }
    }

    #[test]
    fn ownership_splits_the_overlap_down_the_middle() {
        let ranges = {
            let mut ranges = Vec::new();
            let mut start = 0;
            while start < 600 * SAMPLE_RATE {
                let end = (start + 300 * SAMPLE_RATE).min(600 * SAMPLE_RATE);
                ranges.push((start, end));
                if end >= 600 * SAMPLE_RATE {
                    break;
                }
                start += 275 * SAMPLE_RATE;
            }
            ranges
        };
        let windows = ownership(&ranges, SAMPLE_RATE);
        assert_eq!(windows.len(), ranges.len());
        // The seam between the first two blocks lands at the middle of their
        // overlap: 275..300, so 287.5 s.
        let (_, first_high) = windows[0];
        let (second_low, _) = windows[1];
        assert!((first_high - 287.5).abs() < 0.01, "{first_high}");
        assert!((second_low - 287.5).abs() < 0.01);
        assert_eq!(windows[0].0, f32::NEG_INFINITY);
        assert_eq!(windows.last().unwrap().1, f32::INFINITY);
    }

    #[test]
    fn assignment_follows_agreement_in_the_overlap_not_label_order() {
        // The previous block calls the first voice spk0; this block's local
        // clustering calls it spk1. Agreement inside 0..30 s must win.
        let reference = vec![turn(0.0, 20.0, "spk0"), turn(20.0, 30.0, "spk1")];
        let local = vec![turn(0.0, 20.0, "spk1"), turn(20.0, 30.0, "spk0")];
        let mapped = assign_block(&local, &reference, 0.0, (0.0, 30.0), Some(2));
        assert_eq!(mapped[0].speaker, "spk0");
        assert_eq!(mapped[1].speaker, "spk1");
    }

    #[test]
    fn assignment_without_overlap_continues_the_global_numbering() {
        let reference = vec![turn(0.0, 20.0, "spk0"), turn(30.0, 40.0, "spk1")];
        let local = vec![turn(0.0, 10.0, "spk7")];
        // Empty overlap window: nothing to agree on, so the next free global
        // label is used.
        let mapped = assign_block(&local, &reference, 500.0, (0.0, 0.0), None);
        assert_eq!(mapped[0].speaker, "spk2");
    }

    #[test]
    fn small_cluster_is_folded_into_its_nearest_neighbour() {
        let turns = vec![
            turn(0.0, 40.0, "spk0"),
            turn(12.0, 12.9, "spk1"),
            turn(60.0, 90.0, "spk0"),
        ];
        let merged = merge_small_clusters(turns);
        assert!(merged.iter().all(|t| t.speaker == "spk0"));
        assert_eq!(merged.len(), 3);
    }
}


unsafe extern "C" fn diarize_progress(processed: i32, total: i32, arg: *mut c_void) -> i32 {
    if !arg.is_null() && total > 0 {
        let progress = &*(arg as *const AtomicU32);
        let fraction = (processed.max(0) as f32 / total as f32).clamp(0.0, 1.0);
        progress.store((fraction * PROGRESS_SCALE) as u32, Ordering::Relaxed);
    }
    0
}
