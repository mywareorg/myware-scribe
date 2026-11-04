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

/// Opaque speaker-embedding-extractor handle, normalised the same way.
#[cfg(windows)]
type EmbedderPtr = *const c_void;
#[cfg(not(windows))]
type EmbedderPtr = *const ffi::SherpaOnnxSpeakerEmbeddingExtractor;

/// Opaque online stream used to feed samples to the extractor.
#[cfg(windows)]
type StreamPtr = *const c_void;
#[cfg(not(windows))]
type StreamPtr = *const ffi::SherpaOnnxOnlineStream;

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

// ---------------------------------------------------------------- embeddings

/// Speaker-embedding extractor, loaded once and reused across windows.
///
/// Sherpa's diarizer computes embeddings internally but never exposes them, and
/// the block/cluster work needs the vectors themselves: to compare a turn
/// against a centroid, or to notice that one long turn is really two speakers.
pub struct Embedder {
    handle: EmbedderPtr,
    dim: usize,
    // The C strings must outlive the config the handle was built from.
    _model: CString,
    _provider: CString,
}

// Like the diarizer, the extractor is only driven from one thread at a time.
unsafe impl Send for Embedder {}
unsafe impl Sync for Embedder {}

impl Drop for Embedder {
    fn drop(&mut self) {
        unsafe { ffi::SherpaOnnxDestroySpeakerEmbeddingExtractor(self.handle) };
    }
}

impl Embedder {
    /// Build an extractor for `model`. `threads` defaults to the diarization
    /// thread budget, since the two never run at the same moment.
    pub fn new(model: &Path, threads: Option<i32>) -> Result<Self, String> {
        #[cfg(windows)]
        ffi::ensure_loaded()?;

        let threads = threads
            .filter(|threads| *threads > 0)
            .unwrap_or_else(diarize_threads);
        let model = CString::new(model.to_string_lossy().as_bytes())
            .map_err(|e| format!("embedding path: {e}"))?;
        let provider = CString::new("cpu").unwrap();
        let config = ffi::SherpaOnnxSpeakerEmbeddingExtractorConfig {
            model: model.as_ptr(),
            num_threads: threads,
            debug: 0,
            provider: provider.as_ptr(),
        };
        let handle: EmbedderPtr = unsafe { ffi::SherpaOnnxCreateSpeakerEmbeddingExtractor(&config) };
        if handle.is_null() {
            return Err("failed to initialize speaker embedding extractor".to_string());
        }
        let dim = unsafe { ffi::SherpaOnnxSpeakerEmbeddingExtractorDim(handle) };
        if dim <= 0 {
            unsafe { ffi::SherpaOnnxDestroySpeakerEmbeddingExtractor(handle) };
            return Err("speaker embedding extractor reported no dimensions".to_string());
        }
        Ok(Self {
            handle,
            dim: dim as usize,
            _model: model,
            _provider: provider,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// One L2-normalised embedding for `samples` (16 kHz mono).
    pub fn embed(&self, samples: &[f32]) -> Result<Vec<f32>, String> {
        if samples.len() < SAMPLE_RATE / 4 {
            // Under ~250 ms there is not enough signal for a stable vector.
            return Err("sample too short for an embedding".to_string());
        }
        unsafe {
            let stream: StreamPtr = ffi::SherpaOnnxSpeakerEmbeddingExtractorCreateStream(self.handle);
            if stream.is_null() {
                return Err("failed to create embedding stream".to_string());
            }
            ffi::SherpaOnnxOnlineStreamAcceptWaveform(
                stream,
                SAMPLE_RATE as i32,
                samples.as_ptr(),
                samples.len() as i32,
            );
            ffi::SherpaOnnxOnlineStreamInputFinished(stream);
            if ffi::SherpaOnnxSpeakerEmbeddingExtractorIsReady(self.handle, stream) == 0 {
                ffi::SherpaOnnxDestroyOnlineStream(stream);
                return Err("embedding stream was not ready".to_string());
            }
            let data = ffi::SherpaOnnxSpeakerEmbeddingExtractorComputeEmbedding(self.handle, stream);
            if data.is_null() {
                ffi::SherpaOnnxDestroyOnlineStream(stream);
                return Err("embedding computation failed".to_string());
            }
            let mut vector = std::slice::from_raw_parts(data, self.dim).to_vec();
            ffi::SherpaOnnxSpeakerEmbeddingExtractorDestroyEmbedding(data);
            ffi::SherpaOnnxDestroyOnlineStream(stream);
            normalize(&mut vector);
            Ok(vector)
        }
    }
}

/// Scale a vector to unit length in place. Cosine similarity then equals a dot
/// product, which is all the centroid code needs.
fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

// ------------------------------------------------------------ window track

/// Cost, in cosine similarity, of switching speaker between two windows.
/// Without it a single noisy window flips the label; the value is small enough
/// that a real turn change (much larger in similarity) still wins.
const TRACK_SWITCH_PENALTY: f32 = 0.06;

/// Switch penalty for this run, overridable for sweeps.
fn track_penalty() -> f32 {
    crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_TRACK_PENALTY",
        "TRANSIEL_DIARIZE_TRACK_PENALTY",
    )
    .and_then(|value| value.parse::<f32>().ok())
    .unwrap_or(TRACK_SWITCH_PENALTY)
}
/// Windows shorter than this are dropped when the track is turned into turns.
const TRACK_MIN_TURN_S: f32 = 0.30;

/// Off by default until it is measured against the eight-file table.
pub fn track_enabled() -> bool {
    crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_TRACK",
        "TRANSIEL_DIARIZE_TRACK",
    )
    .map(|value| {
        let value = value.trim().to_ascii_lowercase();
        value == "1" || value == "true" || value == "yes"
    })
    .unwrap_or(false)
}

/// Replace the diarizer's turn geometry with a dense window track.
///
/// The built-in pass decides segmentation and clustering together; when it
/// under-segments, an entire exchange becomes one turn and no post-pass can put
/// the boundary back. The track keeps its clustering as a bootstrap and asks the
/// speaker model about every 1.5 s of audio instead, with a switch penalty so a
/// single noisy window cannot flip a speaker.
pub fn run_track(base: Vec<Turn>, samples: &[f32], embedder: &Embedder) -> Vec<Turn> {
    let labels: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for turn in &base {
            if !seen.contains(&turn.speaker) {
                seen.push(turn.speaker.clone());
            }
        }
        seen
    };
    if labels.len() != 2 {
        return base;
    }
    let bootstrap = track_centroids(&base, samples, embedder, &labels);

    let window = (SPLIT_WINDOW_S * SAMPLE_RATE as f32) as usize;
    let hop = (SPLIT_HOP_S * SAMPLE_RATE as f32) as usize;
    if samples.len() < window {
        return base;
    }
    let starts: Vec<usize> = (0..=(samples.len() - window)).step_by(hop).collect();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(starts.len());
    for start in &starts {
        let Some(vector) = embedder.embed(&samples[*start..start + window]).ok() else {
            return base;
        };
        vectors.push(vector);
    }
    if vectors.len() < 3 {
        return base;
    }
    let seed = crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_TRACK_SEED",
        "TRANSIEL_DIARIZE_TRACK_SEED",
    )
    .unwrap_or_default();
    let centroids = if seed.eq_ignore_ascii_case("kmeans") {
        // Cluster the windows themselves: sherpa's labels are least reliable
        // exactly where the track is needed.
        let Some((_, assignment)) = two_means(&vectors) else {
            return base;
        };
        let mut sums = [vec![0.0f32; vectors[0].len()], vec![0.0f32; vectors[0].len()]];
        let mut counts = [0usize; 2];
        for (vector, side) in vectors.iter().zip(&assignment) {
            for (slot, value) in sums[*side].iter_mut().zip(vector) {
                *slot += value;
            }
            counts[*side] += 1;
        }
        if counts[0] == 0 || counts[1] == 0 {
            return base;
        }
        for sum in sums.iter_mut() {
            normalize(sum);
        }
        sums
    } else {
        let Some(centroids) = bootstrap.clone() else {
            return base;
        };
        [centroids[0].clone(), centroids[1].clone()]
    };
    if trace() {
        eprintln!(
            "track: windows {}, bootstrap centroid cosine {:.3}",
            vectors.len(),
            bootstrap
                .as_ref()
                .map(|centroids| cosine(&centroids[0], &centroids[1]))
                .unwrap_or(f32::NAN)
        );
    }
    let scores: Vec<[f32; 2]> = vectors
        .iter()
        .map(|vector| [cosine(vector, &centroids[0]), cosine(vector, &centroids[1])])
        .collect();

    // Viterbi over the two speakers.
    let penalty = track_penalty();
    let mut best = vec![[0.0f32; 2]; scores.len()];
    let mut back = vec![[0u8; 2]; scores.len()];
    best[0] = scores[0];
    for index in 1..scores.len() {
        for side in 0..2 {
            let stay = best[index - 1][side];
            let switch = best[index - 1][1 - side] - penalty;
            let (previous, value) = if stay >= switch {
                (side, stay)
            } else {
                (1 - side, switch)
            };
            best[index][side] = scores[index][side] + value;
            back[index][side] = previous as u8;
        }
    }
    let last = if best[scores.len() - 1][0] >= best[scores.len() - 1][1] {
        0usize
    } else {
        1
    };
    let mut track = vec![0usize; scores.len()];
    track[scores.len() - 1] = last;
    for index in (1..scores.len()).rev() {
        track[index - 1] = back[index][track[index]] as usize;
    }

    let mut out: Vec<Turn> = Vec::new();
    let mut run_start = 0usize;
    for index in 1..=track.len() {
        let boundary = index == track.len() || track[index] != track[run_start];
        if !boundary {
            continue;
        }
        let start = starts[run_start] as f32 / SAMPLE_RATE as f32;
        let end = if index < starts.len() {
            starts[index] as f32 / SAMPLE_RATE as f32
        } else {
            samples.len() as f32 / SAMPLE_RATE as f32
        };
        if end - start >= TRACK_MIN_TURN_S {
            let speaker = labels[track[run_start]].clone();
            match out.last_mut() {
                Some(previous) if previous.speaker == speaker => previous.end = end,
                _ => out.push(Turn {
                    start,
                    end,
                    speaker,
                }),
            }
        }
        run_start = index;
    }
    if out.is_empty() {
        base
    } else {
        out
    }
}

/// Centroid per label from the diarizer's own turns, one vote per turn so a
/// single collapsed turn cannot dominate.
fn track_centroids(
    base: &[Turn],
    samples: &[f32],
    embedder: &Embedder,
    labels: &[String],
) -> Option<Vec<Vec<f32>>> {
    let mut centroids = Vec::with_capacity(labels.len());
    for label in labels {
        let mut sum: Vec<f32> = Vec::new();
        let mut count = 0usize;
        for turn in base.iter().filter(|turn| &turn.speaker == label) {
            let width = SPLIT_WINDOW_S.min(turn.end - turn.start);
            if width < 0.5 {
                continue;
            }
            let middle = (turn.start + turn.end) / 2.0;
            let Some(vector) = embed_span(
                samples,
                embedder,
                middle - width / 2.0,
                middle + width / 2.0,
            ) else {
                continue;
            };
            if sum.is_empty() {
                sum = vec![0.0f32; vector.len()];
            }
            for (slot, value) in sum.iter_mut().zip(vector) {
                *slot += value;
            }
            count += 1;
        }
        if count == 0 {
            return None;
        }
        normalize(&mut sum);
        centroids.push(sum);
    }
    // Two identical centroids would make the track arbitrary.
    if cosine(&centroids[0], &centroids[1]) > 0.95 {
        return None;
    }
    Some(centroids)
}

// ------------------------------------------------------- drift-based splitting

/// A turn longer than this may really hold two speakers: the clustering put
/// them together and the segmentation never broke the span.
const SPLIT_MIN_TURN_S: f32 = 15.0;
/// Window length and hop used to sample a long turn for embeddings.
const SPLIT_WINDOW_S: f32 = 1.5;
const SPLIT_HOP_S: f32 = 0.75;
/// Reject a split whose halves are shorter than this, or too similar to be two
/// voices. Same-speaker windows typically sit above 0.85 cosine.
const SPLIT_MIN_PART_S: f32 = 2.0;
const SPLIT_MAX_CENTROID_SIM: f32 = 0.80;
/// Runs of windows shorter than this are smoothed into their neighbours.
const SPLIT_MIN_RUN_S: f32 = 1.5;
/// How many turns per label are sampled to build the label centroids.
const CENTROID_SAMPLES: usize = 6;

/// Off by default until it is measured on the eval set.
pub fn split_enabled() -> bool {
    crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_SPLIT",
        "TRANSIEL_DIARIZE_SPLIT",
    )
    .map(|value| {
        let value = value.trim().to_ascii_lowercase();
        value == "1" || value == "true" || value == "yes"
    })
    .unwrap_or(false)
}

/// Split turns that the clustering collapsed onto one speaker, using embedding
/// drift inside the turn.
///
/// A whole-file pass can put two voices in one cluster; when that happens the
/// turn is long and its windows point at two different centroids. The existing
/// label centroids decide which half is which speaker, so the split cannot
/// invent a third identity.
pub fn split_long_turns(
    turns: Vec<Turn>,
    samples: &[f32],
    embedder: &Embedder,
) -> Vec<Turn> {
    split_with(turns, |start, end| embed_span(samples, embedder, start, end))
}

/// The splitter, with the embedding step injected so the segmentation logic can
/// be tested without a model.
fn split_with<F>(turns: Vec<Turn>, embed: F) -> Vec<Turn>
where
    F: Fn(f32, f32) -> Option<Vec<f32>>,
{
    let labels: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for turn in &turns {
            if !seen.contains(&turn.speaker) {
                seen.push(turn.speaker.clone());
            }
        }
        seen
    };
    if labels.len() != 2 {
        return turns;
    }
    let mut out: Vec<Turn> = Vec::with_capacity(turns.len());
    for (index, turn) in turns.iter().enumerate() {
        let duration = turn.end - turn.start;
        if duration < SPLIT_MIN_TURN_S {
            out.push(turn.clone());
            continue;
        }
        // The centroids must not be built from the turn under test: a collapsed
        // turn would pull its own label towards one of the halves and make the
        // mapping agree with itself.
        let centroids = local_centroids(&turns, index, &labels, &embed);
        match centroids.and_then(|centroids| {
            split_one(turn, &embed, &labels, &centroids)
        }) {
            Some(parts) => out.extend(parts),
            None => out.push(turn.clone()),
        }
    }
    out
}

/// Mean embedding per speaker label, sampled from the turns of that label that
/// lie closest to `exclude`. Near neighbours share the recording conditions of
/// the turn under test better than a turn five minutes away.
fn local_centroids<F>(
    turns: &[Turn],
    exclude: usize,
    labels: &[String],
    embed: &F,
) -> Option<Vec<Vec<f32>>>
where
    F: Fn(f32, f32) -> Option<Vec<f32>>,
{
    let middle = (turns[exclude].start + turns[exclude].end) / 2.0;
    let mut centroids = Vec::with_capacity(labels.len());
    for label in labels {
        let mut candidates: Vec<(f32, &Turn)> = turns
            .iter()
            .enumerate()
            .filter(|(index, turn)| {
                *index != exclude
                    && &turn.speaker == label
                    && turn.end - turn.start >= 0.75
            })
            .map(|(_, turn)| (((turn.start + turn.end) / 2.0 - middle).abs(), turn))
            .collect();
        candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut sum: Vec<f32> = Vec::new();
        let mut count = 0usize;
        for (_, turn) in candidates.into_iter().take(CENTROID_SAMPLES * 2) {
            let centre = (turn.start + turn.end) / 2.0;
            let half = (turn.end - turn.start).min(SPLIT_WINDOW_S) / 2.0;
            if let Some(vector) = embed(centre - half, centre + half) {
                if sum.is_empty() {
                    sum = vec![0.0f32; vector.len()];
                }
                for (slot, value) in sum.iter_mut().zip(vector) {
                    *slot += value;
                }
                count += 1;
            }
        }
        if count == 0 || sum.is_empty() {
            return None;
        }
        normalize(&mut sum);
        centroids.push(sum);
    }
    Some(centroids)
}

fn embed_span(
    samples: &[f32],
    embedder: &Embedder,
    start: f32,
    end: f32,
) -> Option<Vec<f32>> {
    let first = (start.max(0.0) * SAMPLE_RATE as f32) as usize;
    let last = ((end * SAMPLE_RATE as f32) as usize).min(samples.len());
    if last <= first {
        return None;
    }
    embedder.embed(&samples[first..last]).ok()
}

fn trace() -> bool {
    crate::env_compat("HIPPO_TRANSCRIBE_DIARIZE_TRACE", "TRANSIEL_DIARIZE_TRACE").is_some()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Two-means over unit vectors, seeded with the most distant pair.
fn two_means(vectors: &[Vec<f32>]) -> Option<([usize; 2], Vec<usize>)> {
    if vectors.len() < 4 {
        return None;
    }
    let mut seed = [0usize, 1usize];
    let mut worst = f32::INFINITY;
    for i in 0..vectors.len() {
        for j in (i + 1)..vectors.len() {
            let similarity = cosine(&vectors[i], &vectors[j]);
            if similarity < worst {
                worst = similarity;
                seed = [i, j];
            }
        }
    }
    let mut centroids = [vectors[seed[0]].clone(), vectors[seed[1]].clone()];
    let mut assignment = vec![0usize; vectors.len()];
    for _ in 0..8 {
        let mut changed = false;
        for (index, vector) in vectors.iter().enumerate() {
            let side = if cosine(vector, &centroids[1]) > cosine(vector, &centroids[0]) {
                1
            } else {
                0
            };
            if assignment[index] != side {
                assignment[index] = side;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        for side in 0..2 {
            let mut sum = vec![0.0f32; centroids[side].len()];
            let mut count = 0usize;
            for (index, vector) in vectors.iter().enumerate() {
                if assignment[index] == side {
                    for (slot, value) in sum.iter_mut().zip(vector) {
                        *slot += value;
                    }
                    count += 1;
                }
            }
            if count == 0 {
                return None;
            }
            normalize(&mut sum);
            centroids[side] = sum;
        }
    }
    Some(([seed[0], seed[1]], assignment))
}

fn split_one<F>(
    turn: &Turn,
    embed: &F,
    labels: &[String],
    centroids: &[Vec<f32>],
) -> Option<Vec<Turn>>
where
    F: Fn(f32, f32) -> Option<Vec<f32>>,
{
    let mut starts: Vec<f32> = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut start = turn.start;
    while start + SPLIT_WINDOW_S <= turn.end {
        if let Some(vector) = embed(start, start + SPLIT_WINDOW_S) {
            starts.push(start);
            vectors.push(vector);
        }
        start += SPLIT_HOP_S;
    }
    if vectors.len() < 8 {
        return None;
    }
    let (_, mut assignment) = two_means(&vectors)?;

    // The two halves have to be distinguishable; a single voice drifts too.
    let dim = vectors[0].len();
    let mut means = [vec![0.0f32; dim], vec![0.0f32; dim]];
    let mut counts = [0usize; 2];
    for (index, vector) in vectors.iter().enumerate() {
        let side = assignment[index];
        for (slot, value) in means[side].iter_mut().zip(vector) {
            *slot += value;
        }
        counts[side] += 1;
    }
    if counts[0] == 0 || counts[1] == 0 {
        return None;
    }
    for side in 0..2 {
        normalize(&mut means[side]);
    }
    let separation = cosine(&means[0], &means[1]);
    if trace() {
        eprintln!(
            "split: turn {:.1}-{:.1}s windows {} separation {:.3} parts {} {}",
            turn.start,
            turn.end,
            vectors.len(),
            separation,
            counts[0],
            counts[1]
        );
    }
    if separation > SPLIT_MAX_CENTROID_SIM {
        return None;
    }

    // Smooth short runs so a pause or a breath does not create a speaker turn.
    let min_run = (SPLIT_MIN_RUN_S / SPLIT_HOP_S).ceil() as usize;
    let mut index = 0usize;
    while index < assignment.len() {
        let mut end = index;
        while end + 1 < assignment.len() && assignment[end + 1] == assignment[index] {
            end += 1;
        }
        if end - index + 1 < min_run && index > 0 && end + 1 < assignment.len() {
            let neighbour = assignment[index - 1];
            for slot in assignment.iter_mut().take(end + 1).skip(index) {
                *slot = neighbour;
            }
        }
        index = end + 1;
    }

    // Which global speaker is which half.
    let mut side_label = [0usize; 2];
    for side in 0..2 {
        let mut best = (usize::MAX, f32::NEG_INFINITY);
        for (label_index, centroid) in centroids.iter().enumerate() {
            let similarity = cosine(&means[side], centroid);
            if similarity > best.1 {
                best = (label_index, similarity);
            }
        }
        side_label[side] = best.0;
    }
    if trace() {
        let sims: Vec<[f32; 2]> = (0..2)
            .map(|side| {
                let mut row = [0.0f32; 2];
                for label in 0..centroids.len().min(2) {
                    row[label] = cosine(&means[side], &centroids[label]);
                }
                row
            })
            .collect();
        eprintln!(
            "split: sides map to {:?} (labels {:?}) similarities {:?}",
            side_label, labels, sims
        );
    }
    if side_label[0] == side_label[1] {
        return None;
    }

    let mut parts: Vec<Turn> = Vec::new();
    let mut run_start = 0usize;
    for index in 1..=assignment.len() {
        let boundary = index == assignment.len()
            || assignment[index] != assignment[run_start];
        if !boundary {
            continue;
        }
        let from = starts[run_start];
        let to = if index < starts.len() {
            starts[index]
        } else {
            turn.end
        };
        let label = labels[side_label[assignment[run_start]]].clone();
        match parts.last_mut() {
            Some(last) if last.speaker == label => last.end = to,
            _ => parts.push(Turn {
                start: from,
                end: to,
                speaker: label,
            }),
        }
        run_start = index;
    }

    let longest = parts
        .iter()
        .map(|part| part.end - part.start)
        .fold(0.0f32, f32::max);
    if parts.len() < 2 || longest < SPLIT_MIN_PART_S {
        return None;
    }
    Some(parts)
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
    let mut turns = if blocks_enabled() {
        run_blockwise(&session, samples, num_speakers, None)?
    } else {
        session.run_with_progress(samples, None)?
    };
    if track_enabled() {
        let embedder = Embedder::new(embedding, None)?;
        turns = run_track(turns, samples, &embedder);
    }
    if split_enabled() {
        let embedder = Embedder::new(embedding, None)?;
        return Ok(split_long_turns(turns, samples, &embedder));
    }
    Ok(turns)
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

/// Salt for the diarization cache: the model pair plus the turn geometry, so a
/// cached single pass cannot be served to a blockwise run. Both the pipeline and
/// the GUI's queue restore have to build the key from this, or a restored item
/// silently loses its speakers.
pub fn cache_salt() -> String {
    format!("{}-{}", crate::models::diarizer_id(), geometry_id())
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

    /// Diagnostic, not a gate: measure whether the speaker model separates the
    /// reference speakers at all, and what clustering over reference spans
    /// would score. Ignored by default because it needs an audio file and an
    /// RTTM on disk.
    ///
    /// `DIARIZE_AUDIO`, `DIARIZE_RTTM`, optional `DIARIZE_OUT` (TSV of the
    /// oracle clustering).
    #[test]
    #[ignore = "diagnostic: needs DIARIZE_AUDIO and DIARIZE_RTTM"]
    fn measure_reference_separability() {
        let Ok(audio_path) = std::env::var("DIARIZE_AUDIO") else {
            return;
        };
        let Ok(rttm_path) = std::env::var("DIARIZE_RTTM") else {
            return;
        };
        let (samples, _rate) = crate::audio::decode(std::path::Path::new(&audio_path))
            .expect("decode audio");
        let embedder = Embedder::new(&crate::models::embedding_model(), Some(4))
            .expect("embedding model");

        let mut spans: Vec<(f32, f32, String)> = Vec::new();
        for line in std::fs::read_to_string(&rttm_path)
            .expect("read rttm")
            .lines()
        {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 8 || parts[0] != "SPEAKER" {
                continue;
            }
            let start: f32 = parts[3].parse().unwrap_or(0.0);
            let duration: f32 = parts[4].parse().unwrap_or(0.0);
            if duration <= 0.0 {
                continue;
            }
            spans.push((start, start + duration, parts[7].to_string()));
        }

        let mut vectors: Vec<Vec<f32>> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        for (start, end, speaker) in &spans {
            let width = SPLIT_WINDOW_S.min(end - start);
            let middle = (start + end) / 2.0;
            let Some(vector) = embed_span(
                &samples,
                &embedder,
                (middle - width / 2.0).max(*start),
                (middle + width / 2.0).min(*end),
            ) else {
                continue;
            };
            vectors.push(vector);
            labels.push(speaker.clone());
        }
        assert!(!vectors.is_empty(), "no spans embedded");

        let mut same = (0.0f32, 0usize);
        let mut different = (0.0f32, 0usize);
        for i in 0..vectors.len() {
            for j in (i + 1)..vectors.len() {
                let similarity = cosine(&vectors[i], &vectors[j]);
                if labels[i] == labels[j] {
                    same.0 += similarity;
                    same.1 += 1;
                } else {
                    different.0 += similarity;
                    different.1 += 1;
                }
            }
        }
        println!(
            "intra-speaker cosine {:.3} ({} pairs), inter-speaker {:.3} ({} pairs)",
            same.0 / same.1.max(1) as f32,
            same.1,
            different.0 / different.1.max(1) as f32,
            different.1
        );

        let Some((_, assignment)) = two_means(&vectors) else {
            panic!("clustering failed");
        };
        let mut body = String::new();
        let mut index = 0usize;
        for (start, end, _) in &spans {
            // Spans that could not be embedded have no assignment.
            if index < assignment.len() {
                body.push_str(&format!(
                    "{start:.3}\t{end:.3}\tspk{}\n",
                    assignment[index]
                ));
                index += 1;
            }
        }
        if let Ok(out) = std::env::var("DIARIZE_OUT") {
            std::fs::write(out, body).expect("write assignments");
        }
    }

    #[test]
    fn normalize_scales_to_unit_length() {
        let mut vector = vec![3.0f32, 4.0];
        normalize(&mut vector);
        assert!((vector[0] - 0.6).abs() < 1e-6);
        assert!((vector[1] - 0.8).abs() < 1e-6);

        let mut zero = vec![0.0f32; 4];
        normalize(&mut zero);
        assert_eq!(zero, vec![0.0f32; 4]);
    }

    #[test]
    fn a_collapsed_turn_is_split_by_embedding_drift() {
        // Each label also has a short turn of its own: the centroids come from
        // those, never from the collapsed turn.
        let turns = vec![
            turn(0.0, 40.0, "A"),
            turn(50.0, 56.0, "B"),
            turn(60.0, 66.0, "A"),
        ];
        // The first 20 s sound like A's own turn, the rest like B's.
        let split = split_with(turns, |start, _end| {
            Some(if start < 20.0 || start >= 60.0 {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            })
        });
        assert_eq!(split.len(), 4, "{split:?}");
        assert_eq!(split[0].speaker, "A");
        assert_eq!(split[1].speaker, "B");
        assert!((split[0].end - 20.0).abs() <= 1.5, "{:?}", split[0]);
        assert!((split[1].end - 40.0).abs() <= 0.1, "{:?}", split[1]);
    }

    #[test]
    fn a_long_single_voice_turn_is_left_alone() {
        let turns = vec![turn(0.0, 40.0, "A"), turn(50.0, 56.0, "B")];
        let unchanged = split_with(turns.clone(), |_start, _end| Some(vec![1.0, 0.0]));
        assert_eq!(unchanged.len(), turns.len());
        for (kept, original) in unchanged.iter().zip(&turns) {
            assert_eq!(kept.speaker, original.speaker);
            assert!((kept.start - original.start).abs() < 1e-6);
            assert!((kept.end - original.end).abs() < 1e-6);
        }
    }

    /// Loads the real model when it is installed. Two different speakers must
    /// land closer to their own embedding than to each other's, which is the
    /// assumption every downstream centroid comparison rests on.
    #[test]
    fn embeddings_of_different_files_are_not_identical() {
        let model = crate::models::embedding_model();
        if !model.is_file() {
            return;
        }
        let Ok(embedder) = Embedder::new(&model, Some(1)) else {
            return;
        };
        assert!(embedder.dim() > 0);
        let tone = |hz: f32| -> Vec<f32> {
            (0..SAMPLE_RATE * 2)
                .map(|i| {
                    let t = i as f32 / SAMPLE_RATE as f32;
                    (t * hz * std::f32::consts::TAU).sin() * 0.2
                })
                .collect()
        };
        let a = embedder.embed(&tone(180.0)).expect("embedding");
        let b = embedder.embed(&tone(320.0)).expect("embedding");
        assert_eq!(a.len(), embedder.dim());
        let norm = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "not normalised: {norm}");
        assert!(a != b, "different input produced an identical vector");
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
