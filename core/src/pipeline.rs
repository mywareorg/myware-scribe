//! The diarize + transcribe pipeline.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::asr::{Segment, Transcriber};
use crate::audio;
use crate::cache;
use crate::diarize;
use crate::event::{Event, SpeakerStat, Stats, Turn};
use crate::models;

const MERGE_GAP_S: f32 = 1.0;
const MIN_TURN_S: f32 = 0.4;
/// A split group shorter than this is treated as a diarization artefact.
const MIN_GROUP_S: f32 = 0.5;
const CLIP_PAD_S: f32 = 0.1;
/// Bucket count for the waveform emitted once diarization identifies speakers.
const TIMELINE_BUCKETS: usize = 2000;
/// Per-file progress after decoding and after diarization. Keeping these as
/// fixed phase boundaries makes the progress bar monotonic across stages
/// instead of jumping back to zero when transcription starts.
const DECODE_DONE: f32 = 0.03;
const DIARIZATION_DONE: f32 = 0.45;
/// Per-file progress when the overlap schedule's concurrent phases are done.
/// The last 3 % is left for attribution and writing the transcript.
const OVERLAP_DONE: f32 = 0.97;

/// Whether the pipeline runs the speaker-diarization stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PipelineMode {
    /// Diarize first, then transcribe each speaker turn.
    #[default]
    Diarized,
    /// Transcribe the whole file as one turn with no speaker labels. Skips the
    /// sherpa/ONNX models entirely.
    AsrOnly,
}

impl PipelineMode {
    pub fn diarizes(self) -> bool {
        matches!(self, PipelineMode::Diarized)
    }
}

/// Pick the internal pipeline for a run. A single speaker never needs the
/// diarization stage, so it takes the fastest path regardless of preset.
/// Report an error to the window and to the log. Without the second half a
/// failure that the user closes leaves nothing behind to diagnose.
fn fail(emit: &mut dyn FnMut(Event), message: String) {
    crate::logging::error(&message);
    emit(Event::Error { message });
}

pub fn route(num_speakers: Option<i32>) -> PipelineMode {
    if num_speakers == Some(1) {
        PipelineMode::AsrOnly
    } else {
        PipelineMode::Diarized
    }
}

/// The one turn an ASR-only run transcribes. A caller that explicitly knows
/// the recording is one voice can ask for a label on it; the default stays
/// unlabelled so an ordinary ASR-only run keeps its text-only output.
fn asr_only_turns(duration: f32, single_speaker: bool) -> Vec<Turn> {
    vec![Turn {
        start: 0.0,
        end: duration,
        speaker: if single_speaker {
            "SPEAKER 1".to_string()
        } else {
            String::new()
        },
    }]
}

#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub inputs: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub preset: String,
    pub num_speakers: Option<i32>,
    pub timestamps: bool,
    pub cache: bool,
    /// BCP-47-ish Whisper language code, or `None` for auto-detection.
    pub language: Option<String>,
    pub mode: PipelineMode,
    /// Label the single ASR-only turn as `SPEAKER 1` instead of leaving it
    /// unlabelled. Only meaningful when `mode` does not diarize.
    pub single_speaker: bool,
    /// Lift quiet recordings to a workable level before ASR and diarization.
    pub normalize: bool,
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone)]
struct Line {
    start: f32,
    end: f32,
    speaker: String,
    text: String,
    confidence: f32,
    /// Word times, when the decoder produced token timestamps.
    words: Vec<crate::sidecar::Word>,
}

/// Group token pieces into words for the sidecar.
///
/// Whisper pieces start a new word when they begin with whitespace; punctuation
/// and sub-word pieces are appended. Times come from the first and last piece,
/// so a word's span includes its punctuation without inventing gaps.
fn words_from_tokens(tokens: &[(f32, f32, String)]) -> Vec<crate::sidecar::Word> {
    let mut words: Vec<crate::sidecar::Word> = Vec::new();
    for (start, end, text) in tokens {
        if text.trim().is_empty() {
            continue;
        }
        let begins = text.starts_with(char::is_whitespace) || words.is_empty();
        if begins {
            words.push(crate::sidecar::Word {
                start: *start,
                end: *end,
                text: text.trim_start().to_string(),
            });
        } else if let Some(word) = words.last_mut() {
            word.text.push_str(text);
            word.end = *end;
        }
    }
    for word in &mut words {
        word.text = word.text.trim().to_string();
    }
    words.retain(|word| !word.text.is_empty());
    words
}

/// Absolute word times for one segment inside a clip.
fn segment_words(segment: &Segment, offset: f32) -> Vec<crate::sidecar::Word> {
    let tokens: Vec<(f32, f32, String)> = segment
        .tokens
        .iter()
        .map(|token| (offset + token.start, offset + token.end, token.text.clone()))
        .collect();
    words_from_tokens(&tokens)
}

/// How often the in-progress transcript is parked beside the audio.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

/// The parked transcript for a file whose run is still going.
///
/// Public so the viewer can clean it up when a row is removed or skipped, and
/// so a resume can report that text was recovered.
///
/// Hidden, beside the audio, so a crash leaves the text where the next run
/// knows to look and the user never sees it in the output folder.
pub fn checkpoint_path(input: &Path) -> PathBuf {
    let name = input
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript".to_string());
    input.with_file_name(format!(".{name}.hippo-transcribe.partial"))
}

/// Write the lines produced so far through a temporary file, so a kill during
/// the write cannot leave half a line behind.
fn write_checkpoint(path: &Path, lines: &[Line], use_hours: bool) {
    let text = lines
        .iter()
        .map(|line| {
            let stamp = format_timestamp(line.start, use_hours);
            if line.speaker.is_empty() {
                format!("[{stamp}] {}", line.text)
            } else {
                format!("[{stamp}] {}: {}", line.speaker, line.text)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let temporary = path.with_extension("partial.tmp");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

/// Merge near-adjacent same-speaker turns and drop ultra-short fragments.
/// Returns the turns plus how many were merged and how many were dropped, so
/// the log can tell model granularity apart from our own filtering.
pub(crate) fn clean_turns(turns: Vec<Turn>) -> (Vec<Turn>, usize, usize) {
    let mut merged: Vec<Turn> = Vec::new();
    let mut merge_count = 0usize;
    for turn in turns {
        match merged.last_mut() {
            Some(last) if last.speaker == turn.speaker && turn.start - last.end <= MERGE_GAP_S => {
                last.end = last.end.max(turn.end);
                merge_count += 1;
            }
            _ => merged.push(turn),
        }
    }
    let before = merged.len();
    let kept: Vec<Turn> = merged
        .into_iter()
        .filter(|t| t.end - t.start >= MIN_TURN_S)
        .collect();
    let dropped = before - kept.len();
    (kept, merge_count, dropped)
}

/// Map raw diarization labels to display labels ("SPEAKER 1", ...).
fn build_speaker_map(turns: &[Turn]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for turn in turns {
        let next = map.len() + 1;
        map.entry(turn.speaker.clone())
            .or_insert_with(|| format!("SPEAKER {next}"));
    }
    map
}

fn format_timestamp(seconds: f32, use_hours: bool) -> String {
    let total = seconds.max(0.0) as u32;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if use_hours || hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

fn output_path_with_suffix(
    input: &Path,
    output_dir: &Option<PathBuf>,
    preset: &str,
    speakers: usize,
    suffix: &str,
) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("transcript");
    let speaker_label = if speakers == 1 {
        "1speaker".to_string()
    } else {
        format!("{speakers}speakers")
    };
    let name = format!("{stem}-{preset}-transcription-{speaker_label}{suffix}.txt");
    match output_dir {
        Some(dir) => dir.join(name),
        None => input.with_file_name(name),
    }
}

fn output_path(
    input: &Path,
    output_dir: &Option<PathBuf>,
    preset: &str,
    speakers: usize,
) -> PathBuf {
    output_path_with_suffix(input, output_dir, preset, speakers, "")
}

/// One ASR call: a span of audio covering one or more speaker turns.
struct Clip {
    /// (turn index, turn start, turn end) used to attribute segments.
    turns: Vec<(usize, f32, f32)>,
    /// Absolute start time of the decoded span.
    start: f32,
    from: usize,
    to: usize,
}

/// Trim leading/trailing near-silence inside a span, keeping a 50 ms margin.
/// A cheap energy VAD: it does not need another model and never touches the
/// middle of the clip.
fn trim_silence(samples: &[f32], from: usize, to: usize, rate: u32) -> (usize, usize) {
    const THRESHOLD: f32 = 0.003;
    let step = (rate as usize / 100).max(1);
    if to <= from + 2 * step {
        return (from, to);
    }
    let rms = |slice: &[f32]| {
        (slice.iter().map(|sample| sample * sample).sum::<f32>() / slice.len() as f32).sqrt()
    };
    let mut start = from;
    while start + step <= to && rms(&samples[start..start + step]) < THRESHOLD {
        start += step;
    }
    let mut end = to;
    while end >= start + step && rms(&samples[end - step..end]) < THRESHOLD {
        end -= step;
    }
    let min_len = (rate as f32 * 0.2) as usize;
    if end.saturating_sub(start) < min_len {
        return (from, to);
    }
    let margin = (rate as f32 * 0.05) as usize;
    (
        start.saturating_sub(margin).max(from),
        (end + margin).min(to),
    )
}

/// Group consecutive turns into whisper-sized windows. A window can span
/// several speakers; token timestamps assign each piece of text back to the
/// turn that actually covered it. Short gaps are left inside the window so
/// whisper keeps context, while longer gaps start a new clip.
fn build_clips(turns: &[Turn], duration: f32, rate: u32, samples: &[f32]) -> Vec<Clip> {
    const TARGET_S: f32 = 29.0;
    const MIN_S: f32 = 0.15;
    const MAX_GAP_S: f32 = 0.75;
    let mut clips = Vec::new();
    let mut index = 0;
    while index < turns.len() {
        // Group every turn connected by a short gap. Do not cap the group by
        // TARGET_S here: a long turn can contain short overlapping turns, and
        // stopping at the last short turn would skip the remainder of the long
        // turn. The chunk loop below handles the 29 s split.
        let mut last = index;
        let mut group_end = turns[index].end;
        while last + 1 < turns.len() && turns[last + 1].start - group_end <= MAX_GAP_S {
            last += 1;
            group_end = group_end.max(turns[last].end);
        }
        let window_start = (turns[index].start - CLIP_PAD_S).max(0.0);
        let window_end = (group_end + CLIP_PAD_S).min(duration);
        let mut chunk_start = window_start;
        while chunk_start < window_end {
            let chunk_end = (chunk_start + TARGET_S).min(window_end);
            let clip_turns: Vec<(usize, f32, f32)> = turns[index..=last]
                .iter()
                .enumerate()
                .filter(|(_, turn)| turn.end > chunk_start && turn.start < chunk_end)
                .map(|(offset, turn)| (index + offset, turn.start, turn.end))
                .collect();
            if !clip_turns.is_empty() {
                let raw_from = (chunk_start * rate as f32) as usize;
                let raw_to = ((chunk_end * rate as f32) as usize).min(samples.len());
                let (from, to) = trim_silence(samples, raw_from, raw_to, rate);
                if to > from && (to - from) as f32 >= MIN_S * rate as f32 {
                    clips.push(Clip {
                        turns: clip_turns,
                        start: from as f32 / rate as f32,
                        from,
                        to,
                    });
                }
            }
            chunk_start = chunk_end;
        }
        index = last + 1;
    }
    clips
}

/// Pick the turn that overlaps a segment most; fall back to the nearest turn.
fn attribute_turn(clip: &Clip, start: f32, end: f32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (index, turn_start, turn_end) in &clip.turns {
        let overlap = (end.min(*turn_end) - start.max(*turn_start)).max(0.0);
        if overlap > 0.0 && best.map_or(true, |(_, best_overlap)| overlap > best_overlap) {
            best = Some((*index, overlap));
        }
    }
    if let Some((index, _)) = best {
        return Some(index);
    }
    let midpoint = (start + end) * 0.5;
    clip.turns
        .iter()
        .min_by(|a, b| {
            let distance = |turn: &(usize, f32, f32)| (midpoint - (turn.1 + turn.2) * 0.5).abs();
            distance(a)
                .partial_cmp(&distance(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _, _)| *index)
}

/// How speaker attribution resolved for one file. Diarization quality cannot
/// be judged from the transcript alone, so the counters are logged per file:
/// a rising `collapsed` rate on long audio points at the one-word fallback.
#[derive(Debug, Default)]
struct SplitStats {
    segments: usize,
    /// Segments whose tokens covered more than one diarization turn.
    multi_turn: usize,
    /// Segments collapsed to one speaker because a group held a single word.
    collapsed: usize,
    /// Segments that were actually split between speakers.
    split: usize,
    /// Segments without usable token times (no timestamps from the model).
    untimed: usize,
}

/// Overlaps within this distance of the best one are treated as a tie, and the
/// shorter turn wins it.
const NESTED_TOLERANCE_S: f32 = 0.15;

/// Token-level attribution. Diarization emits nested turns: a one-word
/// interjection sits inside the other speaker's longer turn, so both cover the
/// word and the overlap ties. The earlier (longer) turn used to win by
/// default, which is how short fragments kept landing on the dominant speaker;
/// a tie now goes to the shorter, more specific turn. Whole segments keep the
/// plain maximum-overlap rule: they legitimately span several turns.
fn attribute_token(clip: &Clip, start: f32, end: f32) -> Option<usize> {
    let mut best: Option<(usize, f32, f32)> = None;
    for (index, turn_start, turn_end) in &clip.turns {
        let overlap = (end.min(*turn_end) - start.max(*turn_start)).max(0.0);
        if overlap <= 0.0 {
            continue;
        }
        let duration = (turn_end - turn_start).max(0.0);
        let better = match best {
            None => true,
            Some((_, best_overlap, best_duration)) => {
                overlap > best_overlap + NESTED_TOLERANCE_S
                    || ((overlap - best_overlap).abs() <= NESTED_TOLERANCE_S
                        && duration < best_duration)
            }
        };
        if better {
            best = Some((*index, overlap, duration));
        }
    }
    if let Some((index, _, _)) = best {
        return Some(index);
    }
    attribute_turn(clip, start, end)
}

/// Split one ASR segment at speaker changes using token timestamps. When the
/// model did not provide token times, or every token belongs to one turn,
/// return the original segment as a single line.
fn segment_lines(
    clip: &Clip,
    segment: &Segment,
    turns: &[Turn],
    stats: &mut SplitStats,
    vocabulary: &crate::vocabulary::Vocabulary,
) -> Vec<Line> {
    stats.segments += 1;
    let speaker_for = |turn: Option<usize>| {
        turn.and_then(|index| turns.get(index))
            .map(|turn| turn.speaker.clone())
            .unwrap_or_default()
    };
    let whole = || Line {
        start: clip.start + segment.start,
        end: clip.start + segment.end,
        speaker: speaker_for(attribute_turn(
            clip,
            clip.start + segment.start,
            clip.start + segment.end,
        )),
        text: vocabulary.apply(&segment.text),
        confidence: segment.confidence,
        words: segment_words(segment, clip.start),
    };

    if segment.tokens.len() < 2 {
        stats.untimed += 1;
        return vec![whole()];
    }

    let mut groups: Vec<(Option<usize>, String, f32, f32, usize, Vec<(f32, f32, String)>)> =
        Vec::new();
    for token in &segment.tokens {
        let start = clip.start + token.start;
        let end = clip.start + token.end;
        let turn = attribute_token(clip, start, end);
        match groups.last_mut() {
            Some((last_turn, text, _, last_end, count, tokens)) if *last_turn == turn => {
                text.push_str(&token.text);
                *last_end = end;
                *count += 1;
                tokens.push((start, end, token.text.clone()));
            }
            _ => groups.push((
                turn,
                token.text.clone(),
                start,
                end,
                1,
                vec![(start, end, token.text.clone())],
            )),
        }
    }

    // A brief detached fragment is a diarization boundary error, not a real
    // speaker change. Judge that by duration, not word count: one word that
    // takes half a second ("What?") is a real interjection, one that barely
    // covers a moment is an artefact.
    let mut runs: Vec<Option<usize>> = groups.iter().map(|(turn, _, _, _, _, _)| *turn).collect();
    runs.dedup();
    if runs.len() > 1 {
        stats.multi_turn += 1;
    }
    if groups.len() <= 1 {
        return vec![whole()];
    }
    let fragment = groups
        .iter()
        .any(|(_, _, start, end, count, _)| *count < 2 && end - start < MIN_GROUP_S);
    if fragment {
        stats.collapsed += 1;
        return vec![whole()];
    }
    stats.split += 1;

    groups
        .into_iter()
        .filter_map(|(turn, text, start, end, _, tokens)| {
            let text = vocabulary.apply(text.trim());
            (!text.is_empty()).then(|| Line {
                start,
                end,
                speaker: speaker_for(turn),
                text,
                confidence: segment.confidence,
                words: words_from_tokens(&tokens),
            })
        })
        .collect()
}

/// Per-file ASR counters shared with the worker threads.
#[derive(Clone, Default)]
struct AsrCounters {
    /// Microseconds spent inside whisper across all workers.
    busy_us: Arc<AtomicU64>,
    /// Clips actually sent to the model.
    calls: Arc<AtomicU64>,
    /// Clips served from the segment cache.
    hits: Arc<AtomicU64>,
}

impl AsrCounters {
    fn seconds(&self) -> (f64, u64, u64) {
        (
            self.busy_us.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            self.calls.load(Ordering::Relaxed),
            self.hits.load(Ordering::Relaxed),
        )
    }
}

/// Transcribe a list of decoded spans with a small worker pool. Results are
/// handed to `on_result` in completion order; the caller owns ordering and
/// whatever it wants to do with the text.
#[allow(clippy::too_many_arguments)]
fn run_asr_pool(
    spans: &[(usize, usize)],
    token_flags: &[bool],
    samples: &[f32],
    transcriber: &Transcriber,
    workers: usize,
    per_call_threads: i32,
    language: Option<&str>,
    fast: bool,
    best_of: i32,
    audio_hash: Option<&String>,
    model_name: &str,
    preprocess: &str,
    cancelled: &dyn Fn() -> bool,
    counters: &AsrCounters,
    mut on_result: impl FnMut(usize, Result<Vec<Segment>, String>),
) {
    if spans.is_empty() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let busy = counters.busy_us.clone();
            let calls = counters.calls.clone();
            let hits = counters.hits.clone();
            let next = &next;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= spans.len() {
                    break;
                }
                let (from, to) = spans[index];
                let token_timestamps = token_flags.get(index).copied().unwrap_or(true);
                let key = audio_hash.map(|hash| {
                    cache::clip_key(
                        hash,
                        model_name,
                        language,
                        from,
                        to,
                        best_of,
                        fast,
                        token_timestamps,
                        transcriber.prompt(),
                        preprocess,
                    )
                });
                let result = match key.as_ref().and_then(|key| cache::read_segments(key)) {
                    Some(segments) => {
                        hits.fetch_add(1, Ordering::Relaxed);
                        Ok(segments)
                    }
                    None => {
                        let call_started = Instant::now();
                        let result = transcriber.transcribe_full(
                            &samples[from..to],
                            per_call_threads,
                            language,
                            fast,
                            best_of,
                            token_timestamps,
                        );
                        calls.fetch_add(1, Ordering::Relaxed);
                        busy.fetch_add(
                            call_started.elapsed().as_micros() as u64,
                            Ordering::Relaxed,
                        );
                        if let (Ok(segments), Some(key)) = (&result, key.as_ref()) {
                            cache::write_segments(key, segments);
                        }
                        result
                    }
                };
                if tx.send((index, result)).is_err() {
                    break;
                }
            });
        }
        drop(tx);

        let mut remaining = spans.len();
        while remaining > 0 {
            if cancelled() {
                break;
            }
            let Ok((index, result)) = rx.recv() else {
                break;
            };
            remaining -= 1;
            on_result(index, result);
        }
    });
}

/// How transcription is scheduled against diarization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Schedule {
    /// Diarize first, then transcribe the resulting speaker clips.
    Classic,
    /// Transcribe fixed windows while diarization runs, then attribute.
    Overlap,
}

/// Below this many physical cores the overlap schedule loses more to
/// contention than it gains; those machines keep the sequential path.
const OVERLAP_MIN_CORES: usize = 8;

impl Schedule {
    fn select(physical: usize) -> Self {
        let setting = crate::env_compat("HIPPO_TRANSCRIBE_PIPELINE", "TRANSIEL_PIPELINE")
            .map(|value| value.trim().to_ascii_lowercase());
        Self::from_setting(setting.as_deref(), physical)
    }

    fn from_setting(setting: Option<&str>, physical: usize) -> Self {
        match setting {
            Some("classic") | Some("sequential") => Schedule::Classic,
            Some("overlap") | Some("parallel") => Schedule::Overlap,
            _ if physical >= OVERLAP_MIN_CORES => Schedule::Overlap,
            _ => Schedule::Classic,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Schedule::Classic => "classic",
            Schedule::Overlap => "overlap",
        }
    }
}

/// A diarization run that may already be in flight.
enum DiarizeJob {
    Ready(Vec<Turn>),
    Running(
        std::thread::JoinHandle<(Result<Vec<Turn>, String>, f64)>,
        Arc<AtomicU32>,
    ),
}

/// Bar fraction shared by the overlap schedule's concurrent phases. The bar
/// follows whichever phase is behind, so neither can finish it early.
fn overlap_progress(asr_fraction: f32, diarize_fraction: f32) -> f32 {
    let combined = asr_fraction
        .clamp(0.0, 1.0)
        .min(diarize_fraction.clamp(0.0, 1.0));
    (DECODE_DONE + (OVERLAP_DONE - DECODE_DONE) * combined).clamp(DECODE_DONE, OVERLAP_DONE)
}

/// Bar fraction while waiting for the speaker map inside an already-started
/// range.
fn quiet_band_progress(floor: f32, ceiling: f32, progress: &Arc<AtomicU32>) -> f32 {
    let fraction = (progress.load(Ordering::Relaxed) as f32 / 1_000_000.0).clamp(0.0, 1.0);
    (floor + (ceiling - floor) * fraction).clamp(floor, ceiling)
}

/// Wait for a diarization job, reporting its progress when `quiet` is false.
/// `quiet_band` is set when the caller already ran its own phase (the overlap
/// schedule's ASR pool) and wants the wait to keep moving inside that range.
fn finish_diarization(
    job: DiarizeJob,
    emit: &mut dyn FnMut(Event),
    cancelled: &dyn Fn() -> bool,
    cache_keys: &[&str],
    quiet: bool,
    quiet_band: Option<(f32, f32)>,
) -> (Result<Vec<Turn>, String>, f64) {
    let DiarizeJob::Running(handle, progress) = job else {
        let DiarizeJob::Ready(turns) = job else {
            unreachable!()
        };
        // Nothing to diarize (ASR-only, one speaker, or a cache hit): the
        // speaker map is already complete as far as the caller is concerned.
        emit(Event::Diarize { fraction: 1.0 });
        return (Ok(turns), 0.0);
    };
    if !quiet {
        emit(Event::Stage {
            stage: "diarization".to_string(),
            label: "Running Diarization 0%".to_string(),
            progress: DECODE_DONE,
        });
        while !handle.is_finished() {
            if cancelled() {
                break;
            }
            let fraction = progress.load(Ordering::Relaxed) as f32 / 1_000_000.0;
            emit(Event::Stage {
                stage: "diarization".to_string(),
                label: format!(
                    "Running Diarization {:.0}%",
                    (fraction * 100.0).clamp(0.0, 100.0)
                ),
                progress: (DECODE_DONE
                    + (DIARIZATION_DONE - DECODE_DONE) * fraction.clamp(0.0, 1.0))
                .clamp(0.0, 1.0),
            });
            emit(Event::Diarize {
                fraction: fraction.clamp(0.0, 1.0),
            });
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if quiet {
        if let Some((floor, ceiling)) = quiet_band {
            // Transcription finished first and the speaker map is still being
            // built: keep the bar moving instead of parking it.
            while !handle.is_finished() {
                if cancelled() {
                    break;
                }
                let fraction = (progress.load(Ordering::Relaxed) as f32 / 1_000_000.0)
                    .clamp(0.0, 1.0);
                emit(Event::Stage {
                    stage: "diarization".to_string(),
                    label: "Finishing speaker labels".to_string(),
                    progress: quiet_band_progress(floor, ceiling, &progress),
                });
                emit(Event::Diarize { fraction });
                std::thread::sleep(Duration::from_millis(100));
            }
            emit(Event::Stage {
                stage: "diarization".to_string(),
                label: "Finishing speaker labels".to_string(),
                progress: ceiling,
            });
        }
    }
    let joined = handle.join();
    emit(Event::Diarize { fraction: 1.0 });
    if !quiet {
        emit(Event::Stage {
            stage: "diarization".to_string(),
            label: "Running Diarization 100%".to_string(),
            progress: DIARIZATION_DONE,
        });
    }
    match joined {
        Ok((Ok(turns), elapsed)) => {
            for key in cache_keys {
                cache::write(key, &turns);
            }
            (Ok(turns), elapsed)
        }
        Ok((Err(message), elapsed)) => (Err(message), elapsed),
        Err(_) => (
            Err("diarization thread panicked".to_string()),
            diarized_elapsed_hint(),
        ),
    }
}

/// Elapsed time for a diarization job that panicked, so the timing line still
/// has a finite value.
fn diarized_elapsed_hint() -> f64 {
    0.0
}

/// Clean raw turns, label the speakers, and publish both to the UI.
fn resolve_and_emit_turns(
    raw: Vec<Turn>,
    diarized: bool,
    audio_path: &str,
    samples: &[f32],
    elapsed: f64,
    emit: &mut dyn FnMut(Event),
) -> (Vec<Turn>, usize) {
    let raw_turns = raw.len();
    let (cleaned, merged_turns, dropped_turns) = clean_turns(raw);
    let mut map = build_speaker_map(&cleaned);
    if map.len() <= 1 {
        for label in map.values_mut() {
            label.clear();
        }
    }
    // ASR-only transcripts have no speaker labels, but the filename still
    // describes a single undifferentiated stream.
    let speaker_count = if diarized { map.len() } else { 1 };
    if diarized {
        // Separate diarizer granularity from our own cleanup: if `raw` is
        // already coarse the model missed the changes, not the filters.
        crate::logging::info(format!(
            "diarize: {raw_turns} raw turn(s), {merged_turns} merged, {dropped_turns} \
             dropped as short, {} kept, {} speaker(s), {:.1}s",
            cleaned.len(),
            map.len(),
            elapsed
        ));
    }
    let turns: Vec<Turn> = cleaned
        .iter()
        .map(|turn| Turn {
            start: turn.start,
            end: turn.end,
            speaker: map
                .get(&turn.speaker)
                .cloned()
                .unwrap_or_else(|| turn.speaker.clone()),
        })
        .collect();
    emit(Event::Turns {
        turns: turns.clone(),
    });
    // Speakers are now known: surface the waveform without re-decoding.
    emit(Event::Waveform {
        audio: audio_path.to_string(),
        peaks: audio::rms_peaks(samples, TIMELINE_BUCKETS),
    });
    (turns, speaker_count)
}

/// Pin the language once per file. Auto-detection otherwise repeats on every
/// clip or window, which is pure overhead once the first call has decided.
fn pin_language(
    transcriber: &Transcriber,
    requested: &Option<&str>,
    samples: &[f32],
    rate: u32,
    spans: usize,
    threads: i32,
) -> Option<String> {
    let probe = samples.len().min(30 * rate as usize);
    if requested.is_some() || spans < 2 || probe < 30 * rate as usize {
        return None;
    }
    let language = transcriber.detect_language(&samples[..probe], threads)?;
    crate::logging::info(format!("language: pinned {language} (detected once)"));
    Some(language)
}

/// A fixed transcription window that does not depend on speaker turns.
struct Window {
    start: f32,
    from: usize,
    to: usize,
}

/// Fixed 29 s windows with a quiet-point cut near each boundary. Used by the
/// overlap schedule, where the turns are not known yet when ASR starts.
const WINDOW_S: f32 = 29.0;
const WINDOW_CUT_SEARCH_S: f32 = 1.5;
/// Energy below which a span carries no usable speech (same level as the clip
/// trimmer).
const SPEECH_THRESHOLD: f32 = 0.003;

fn span_rms(samples: &[f32], from: usize, to: usize) -> f32 {
    if to <= from {
        return 0.0;
    }
    (samples[from..to]
        .iter()
        .map(|sample| sample * sample)
        .sum::<f32>()
        / (to - from) as f32)
        .sqrt()
}

/// Move a window boundary to the quietest 100 ms slice near it, so a word is
/// not sliced in half.
fn quiet_cut(samples: &[f32], rate: u32, start: f32, end: f32) -> f32 {
    let step = (rate as f32 * 0.1) as usize;
    let search = (rate as f32 * WINDOW_CUT_SEARCH_S) as usize;
    let centre = (end * rate as f32) as usize;
    let floor = ((start * rate as f32) as usize + step).min(samples.len());
    let low = centre.saturating_sub(search).max(floor);
    let high = (centre + search).min(samples.len().saturating_sub(step));
    if high <= low + step {
        return end;
    }
    let mut best = centre;
    let mut best_rms = f32::INFINITY;
    let mut at = low;
    while at + step <= high {
        let rms = span_rms(samples, at, at + step);
        if rms < best_rms {
            best_rms = rms;
            best = at + step / 2;
        }
        at += step;
    }
    (best as f32 / rate as f32).min(end)
}

fn build_windows(duration: f32, rate: u32, samples: &[f32]) -> Vec<Window> {
    let mut windows = Vec::new();
    let mut start = 0.0f32;
    while start < duration {
        let end = if start + WINDOW_S >= duration {
            duration
        } else {
            quiet_cut(samples, rate, start, start + WINDOW_S)
        };
        if end <= start {
            break;
        }
        let raw_from = (start * rate as f32) as usize;
        let raw_to = ((end * rate as f32) as usize).min(samples.len());
        let (from, to) = trim_silence(samples, raw_from, raw_to, rate);
        let long_enough = (to.saturating_sub(from)) as f32 >= 0.2 * rate as f32;
        if long_enough && span_rms(samples, from, to) >= SPEECH_THRESHOLD {
            windows.push(Window {
                start: from as f32 / rate as f32,
                from,
                to,
            });
        }
        start = end;
    }
    windows
}

/// Run the pipeline over all inputs, emitting progress events.
pub fn run(opts: &PipelineOptions, emit: &mut dyn FnMut(Event)) {
    let run_start = std::time::Instant::now();
    let total = opts.inputs.len().max(1);
    let is_cancelled = || {
        opts.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
    };

    crate::logging::info(format!(
        "run: {} input(s), preset {}, mode {:?}, {} speakers",
        opts.inputs.len(),
        opts.preset,
        opts.mode,
        opts.num_speakers
            .map(|count| count.to_string())
            .unwrap_or_else(|| "auto".to_string())
    ));

    let segmentation = models::segmentation_model();
    let embedding = models::embedding_model();
    let whisper = models::whisper_model(&opts.preset);
    // The salt carries the turn geometry as well as the model pair: a cached
    // single pass must not be served to a blockwise run.
    let preprocess = audio::preprocess_tag(opts.normalize);
    // Normalized audio is different audio: the tag keeps its diarization turns
    // and ASR clips out of the cache of a raw run, and vice versa.
    let diarizer = format!("{}-{}", diarize::cache_salt(), preprocess);
    let diarized = opts.mode.diarizes();

    emit(Event::Stage {
        stage: "loading".to_string(),
        label: "Loading models".to_string(),
        progress: 0.0,
    });

    if diarized {
        if let Err(message) = diarize::ensure_available() {
            fail(emit, message);
            return;
        }
    }

    let vocabulary = crate::vocabulary::Vocabulary::load();
    let prompt = vocabulary.prompt(crate::vocabulary::PROMPT_BUDGET);
    if let Some(prompt) = &prompt {
        crate::logging::info(format!("vocabulary: {} character prompt", prompt.len()));
    }

    let load_started = Instant::now();
    let mut transcriber = match Transcriber::load(&whisper) {
        Ok(transcriber) => Some(transcriber),
        Err(message) => {
            // Fail before diarization/processing: never write an empty
            // transcript just because the ASR model is unavailable.
            let hint = if whisper.is_file() {
                "run: hippo-transcribe --cli models download --force"
            } else {
                "run: hippo-transcribe --cli models download"
            };
            let name = if opts.preset.eq_ignore_ascii_case("speed") {
                "Speed"
            } else if opts.preset.eq_ignore_ascii_case("balanced") {
                "Balanced"
            } else {
                "Accuracy"
            };
            emit(Event::Log {
                message: format!("{message} (expected at {})", whisper.display()),
            });
            fail(emit, format!("{name} model unavailable — {hint}"));
            return;
        }
    };
    if let Some(transcriber) = &mut transcriber {
        transcriber.set_prompt(prompt);
    }

    if let Some(transcriber) = &transcriber {
        crate::logging::info(format!("ASR backend: {}", transcriber.backend().label()));
        emit(Event::Backend {
            backend: transcriber.backend().label().to_string(),
            device: None,
            fallback: transcriber.fallback_reason().map(str::to_string),
        });
        emit(Event::Log {
            message: format!("ASR backend: {}", transcriber.backend().label()),
        });
        if let Some(reason) = transcriber.fallback_reason() {
            crate::logging::warn(format!(
                "GPU unavailable, using CPU ({} failed: {reason})",
                transcriber.backend().label()
            ));
            emit(Event::Log {
                message: format!(
                    "GPU unavailable, using CPU ({} failed: {reason})",
                    transcriber.backend().label()
                ),
            });
        }
    }

    let load_elapsed = load_started.elapsed().as_secs_f64();

    // Parallel ASR: a small pool of workers sharing one model (each call creates
    // its own decode state). Per-call threads split the physical cores.
    let physical = num_cpus::get_physical().max(1);
    let env_int = |current: &str, legacy: &str| {
        crate::env_compat(current, legacy).and_then(|v| v.parse::<usize>().ok())
    };
    // Turbo is encoder-bound: one call with all threads beats four calls
    // splitting the same cores. Smaller models keep the parallel pool.
    let default_workers = if opts.preset.eq_ignore_ascii_case("accuracy") {
        1
    } else {
        physical.min(4)
    };
    let workers = env_int("HIPPO_TRANSCRIBE_ASR_WORKERS", "TRANSIEL_ASR_WORKERS")
        .unwrap_or(default_workers)
        .max(1);
    let requested_threads = env_int("HIPPO_TRANSCRIBE_ASR_THREADS", "TRANSIEL_ASR_THREADS")
        .map(|v| v as i32)
        .unwrap_or((physical / workers).max(1) as i32)
        .max(1);

    let schedule = Schedule::select(physical);
    crate::logging::info(format!(
        "schedule: {} ({} physical core(s), {} ASR worker(s) x {} requested thread(s))",
        schedule.label(),
        physical,
        workers,
        requested_threads
    ));

    crate::logging::info(format!(
        "timing models: whisper {:.2}s, startup {:.2}s",
        load_elapsed,
        run_start.elapsed().as_secs_f64()
    ));

    // Decode the next file while the current one is diarized and transcribed.
    // This only helps batches; a single file is decoded inline.
    //
    // Prefetching holds two decoded buffers at once (~230 MB per hour of 16 kHz
    // f32 audio), so a blocking decode used to be the cheaper trade for long
    // files. It is not: the blocked decode stalls every ASR worker. The cap is
    // now opt-in — HIPPO_TRANSCRIBE_PREFETCH_MAX_S=<seconds> restores it.
    let prefetch_max_s =
        crate::env_compat("HIPPO_TRANSCRIBE_PREFETCH_MAX_S", "TRANSIEL_PREFETCH_MAX_S")
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0);
    type Decoded = Result<(Vec<f32>, u32), String>;
    let mut prefetched: Option<std::thread::JoinHandle<Decoded>> = None;

    // One sherpa session for the whole run. Built on first use so a fully
    // cached batch never pays to load the diarization models.
    let diarizer_session: std::sync::OnceLock<Arc<diarize::Diarizer>> = std::sync::OnceLock::new();

    for (index, input) in opts.inputs.iter().enumerate() {
        if is_cancelled() {
            break;
        }
        let file_start = Instant::now();
        emit(Event::File {
            audio: input.display().to_string(),
        });
        emit(Event::Stage {
            stage: "decoding".to_string(),
            label: "Decoding".to_string(),
            progress: 0.0,
        });

        let decode_started = Instant::now();
        let decode_prefetched = prefetched.is_some();
        let decoded = match prefetched.take() {
            Some(handle) => handle
                .join()
                .unwrap_or_else(|_| Err("audio decode thread panicked".to_string())),
            None => audio::decode_with_progress(input, &mut |fraction| {
                emit(Event::Stage {
                    stage: "decoding".to_string(),
                    label: format!("Decoding {:.0}%", fraction * 100.0),
                    progress: DECODE_DONE * fraction,
                });
            }),
        };
        if index + 1 < opts.inputs.len() {
            let next = opts.inputs[index + 1].clone();
            let fits = prefetch_max_s <= 0.0
                || audio::duration_fast(&next).is_some_and(|seconds| seconds <= prefetch_max_s);
            if fits {
                prefetched = Some(std::thread::spawn(move || audio::decode(&next)));
            }
        }
        let (mut samples, rate) = match decoded {
            Ok(value) => value,
            Err(message) => {
                fail(emit, message);
                continue;
            }
        };
        if opts.normalize {
            let gain = audio::normalize_loudness(&mut samples, rate);
            if (gain - 1.0).abs() > 0.01 {
                crate::logging::info(format!("loudness: gain {gain:.2}x"));
            }
        }
        let duration = samples.len() as f64 / rate as f64;
        let checkpoint = checkpoint_path(&input);
        let checkpoint_hours = duration >= 3600.0;
        let decoded_at = Instant::now();
        let samples = Arc::new(samples);
        emit(Event::Meta {
            audio: input.display().to_string(),
            duration: duration as f32,
        });
        emit(Event::Stage {
            stage: "decoding".to_string(),
            label: "Decoding".to_string(),
            progress: DECODE_DONE,
        });

        // Hash the samples the decoder just produced: diarization and per-clip
        // ASR caches both use it, and this avoids reading the file again.
        let samples_hash_started = Instant::now();
        let audio_hash = if opts.cache {
            Some(cache::samples_hash(samples.as_slice()))
        } else {
            None
        };
        let hash_elapsed = samples_hash_started.elapsed().as_secs_f64();
        let cache_key = if diarized && opts.num_speakers != Some(1) {
            audio_hash
                .as_deref()
                .map(|hash| cache::key_from_hash(hash, &diarizer, opts.num_speakers))
        } else {
            None
        };
        // The GUI restores a completed item's turns at startup, where there are
        // no decoded samples to hash, so it looks them up by a hash of the file
        // bytes instead (`cache::key`, see `gui/src/backend.rs`). Write the
        // turns under that key too, or a restored item silently loses its
        // speakers.
        let file_cache_key = if cache_key.is_some() {
            cache::key(&input, &diarizer, opts.num_speakers)
        } else {
            None
        };
        let cache_keys: Vec<&str> = cache_key
            .iter()
            .chain(file_cache_key.iter())
            .map(String::as_str)
            .collect();
        let mut did_diarize = false;
        // Peek the diarization cache first: the overlap schedule is only worth
        // it when diarization actually has work to do.
        let cached_turns = if diarized && opts.num_speakers != Some(1) {
            cache_key.as_deref().and_then(cache::read)
        } else {
            None
        };
        // Only overlap when diarization is about to run for real.
        let overlap = matches!(schedule, Schedule::Overlap)
            && diarized
            && opts.num_speakers != Some(1)
            && cached_turns.is_none();
        // Overlap runs diarization beside the ASR pool, so the two split the
        // cores instead of both claiming all of them.
        let (per_call_threads, diarize_thread_budget) = if overlap {
            let reserved = ((physical / 3).max(1)) as i32;
            let asr_cores = (physical as i32 - reserved).max(1);
            (
                requested_threads.min((asr_cores / workers as i32).max(1)),
                Some(reserved),
            )
        } else {
            (requested_threads, None)
        };

        let job = if !diarized {
            // ASR-only mode: no speaker turns, no sherpa models, no cache.
            // An explicit single-speaker request labels the lone turn even
            // though diarization never ran.
            DiarizeJob::Ready(asr_only_turns(duration as f32, opts.single_speaker))
        } else if opts.num_speakers == Some(1) {
            // One speaker: skip diarization entirely and transcribe the file as
            // a single turn.
            DiarizeJob::Ready(vec![Turn {
                start: 0.0,
                end: duration as f32,
                speaker: "SPEAKER 1".to_string(),
            }])
        } else {
            match cached_turns {
                Some(turns) => {
                    emit(Event::Log {
                        message: "Using cached diarization.".to_string(),
                    });
                    DiarizeJob::Ready(turns)
                }
                None => {
                    did_diarize = true;
                    let progress = Arc::new(AtomicU32::new(0));
                    let progress_for_thread = progress.clone();
                    let audio = samples.clone();
                    let session = match diarizer_session.get() {
                        Some(session) => session.clone(),
                        None => {
                            match diarize::Diarizer::new_with_threads(
                                &segmentation,
                                &embedding,
                                opts.num_speakers,
                                diarize::CLUSTER_THRESHOLD,
                                diarize_thread_budget,
                            ) {
                                Ok(session) => {
                                    crate::logging::info(format!(
                                        "Diarizer ready ({} threads)",
                                        session.threads()
                                    ));
                                    let session = Arc::new(session);
                                    let _ = diarizer_session.set(session.clone());
                                    session
                                }
                                Err(message) => {
                                    fail(emit, message);
                                    continue;
                                }
                            }
                        }
                    };
                    let handle = std::thread::spawn(move || {
                        let started = Instant::now();
                        let raw = session
                            .run_with_progress(audio.as_slice(), Some(progress_for_thread));
                        // A collapsed turn is the failure that survives the
                        // clustering: one speaker is credited with a stretch
                        // that holds both. Splitting it by embedding drift is
                        // the only pass that can recover those turns.
                        let result = raw.and_then(|turns| {
                            if !diarize::split_enabled() {
                                return Ok(turns);
                            }
                            let embedder = diarize::Embedder::new(
                                &crate::models::embedding_model(),
                                None,
                            )?;
                            Ok(diarize::split_long_turns(
                                turns,
                                audio.as_slice(),
                                &embedder,
                            ))
                        });
                        (result, started.elapsed().as_secs_f64())
                    });
                    DiarizeJob::Running(handle, progress)
                }
            }
        };
        // The overlap path reads diarization progress while the ASR pool runs so
        // the bar can follow whichever phase is behind.
        let job_progress = match &job {
            DiarizeJob::Running(_, progress) => Some(progress.clone()),
            DiarizeJob::Ready(_) => None,
        };

        // Every preset uses a multilingual model, so the requested language (or
        // auto-detection) always applies. `auto`/empty mean detection,
        // matching the CLI.
        let decode_language = opts
            .language
            .as_deref()
            .filter(|code| !code.is_empty() && !code.eq_ignore_ascii_case("auto"));
        let fast_decode = opts.preset.eq_ignore_ascii_case("speed");
        // `best_of` only repeats the decoder; the encoder dominates. One sample
        // is enough for every preset, and it roughly halves the decode stage.
        // HIPPO_TRANSCRIBE_BEST_OF raises it again for an A/B check.
        let best_of = env_int("HIPPO_TRANSCRIBE_BEST_OF", "TRANSIEL_BEST_OF")
            .map(|value| value as i32)
            .unwrap_or(1)
            .max(1);
        let model_name = models::whisper_relative(&opts.preset);

        let mut lines: Vec<Line> = Vec::new();
        let mut split_stats = SplitStats::default();
        // Timing accumulators for this file; filled by the ASR section below.
        let counters = AsrCounters::default();
        let mut prepare_seconds = 0.0f64;
        let mut asr_seconds = 0.0f64;
        let turns: Vec<Turn>;
        let speaker_count: usize;
        let diarized_at: Instant;
        let diarize_seconds: f64;

        if overlap {
            let transcriber = match transcriber.as_ref() {
                Some(transcriber) => transcriber,
                None => {
                    fail(emit, "speech model unavailable".to_string());
                    continue;
                }
            };
            // Fixed windows cut before the turns exist: transcription runs on
            // them while diarization is still working.
            let windows = build_windows(duration as f32, rate, samples.as_slice());
            let spans: Vec<(usize, usize)> = windows.iter().map(|w| (w.from, w.to)).collect();
            let total_windows = spans.len();
            let total_seconds: f64 = spans
                .iter()
                .map(|(from, to)| (to - from) as f64 / rate as f64)
                .sum();
            let mut done_seconds = 0.0f64;
            let mut done_windows = 0usize;

            emit(Event::Stage {
                stage: "transcription".to_string(),
                label: format!("Transcribing 0/{total_windows}"),
                progress: DECODE_DONE,
            });
            crate::logging::info(
                "schedule: diarization and transcription running in parallel".to_string(),
            );

            // Quality presets need token times on every window: the turns are
            // not known yet, so any window could span a speaker change. The
            // speed preset keeps its no-token-timestamps decode.
            let token_flags = vec![!fast_decode; spans.len()];
            let pinned_language = pin_language(
                transcriber,
                &decode_language,
                samples.as_slice(),
                rate,
                spans.len(),
                per_call_threads,
            );
            let language = decode_language.or(pinned_language.as_deref());
            let asr_started = Instant::now();
            let mut results: Vec<Option<Result<Vec<Segment>, String>>> =
                (0..total_windows).map(|_| None).collect();
            run_asr_pool(
                &spans,
                &token_flags,
                samples.as_slice(),
                transcriber,
                workers,
                per_call_threads,
                language,
                fast_decode,
                best_of,
                audio_hash.as_ref(),
                &model_name,
                preprocess,
                &is_cancelled,
                &counters,
                |index, result| {
                    results[index] = Some(result);
                    done_seconds += (spans[index].1 - spans[index].0) as f64 / rate as f64;
                    done_windows += 1;
                    let fraction = if total_seconds > 0.0 {
                        (done_seconds / total_seconds).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    // Diarization runs beside the pool: the bar follows whichever
                    // phase is behind so neither can finish it early.
                    let diarize_fraction = job_progress
                        .as_ref()
                        .map(|progress| {
                            (progress.load(Ordering::Relaxed) as f32 / 1_000_000.0).clamp(0.0, 1.0)
                        })
                        .unwrap_or(1.0);
                    emit(Event::Stage {
                        stage: "transcription".to_string(),
                        label: format!("Transcribing {done_windows}/{total_windows}"),
                        progress: overlap_progress(fraction as f32, diarize_fraction),
                    });
                    emit(Event::Work {
                        done: done_seconds,
                        total: total_seconds,
                    });
                    emit(Event::Diarize {
                        fraction: diarize_fraction.clamp(0.0, 1.0),
                    });
                },
            );
            asr_seconds = asr_started.elapsed().as_secs_f64();

            let (raw, elapsed) = match finish_diarization(
                job,
                emit,
                &is_cancelled,
                &cache_keys,
                true,
                Some((DECODE_DONE, OVERLAP_DONE)),
            ) {
                (Ok(turns), elapsed) => (turns, elapsed),
                (Err(message), elapsed) => {
                    // The text is already transcribed: keep it under a single
                    // unlabelled speaker rather than losing the whole file.
                    fail(emit, message);
                    (
                        vec![Turn {
                            start: 0.0,
                            end: duration as f32,
                            speaker: String::new(),
                        }],
                        elapsed,
                    )
                }
            };
            diarize_seconds = elapsed;
            diarized_at = Instant::now();
            let (resolved, count) = resolve_and_emit_turns(
                raw,
                diarized,
                &input.display().to_string(),
                samples.as_slice(),
                diarize_seconds,
                emit,
            );
            turns = resolved;
            speaker_count = count;

            // Attribute the finished text to whoever held the floor.
            for (index, window) in windows.iter().enumerate() {
                let window_end = window.start + (window.to - window.from) as f32 / rate as f32;
                let clip_turns: Vec<(usize, f32, f32)> = turns
                    .iter()
                    .enumerate()
                    .filter(|(_, turn)| turn.end > window.start && turn.start < window_end)
                    .map(|(turn_index, turn)| (turn_index, turn.start, turn.end))
                    .collect();
                if clip_turns.is_empty() {
                    continue;
                }
                let clip = Clip {
                    turns: clip_turns,
                    start: window.start,
                    from: window.from,
                    to: window.to,
                };
                match results[index].take() {
                    Some(Ok(segments)) => {
                        for segment in segments {
                            lines.extend(segment_lines(
                                &clip,
                                &segment,
                                &turns,
                                &mut split_stats,
                                &vocabulary,
                            ));
                        }
                    }
                    Some(Err(message)) => emit(Event::Log { message }),
                    None => {}
                }
            }
            // Attribution replaced clip building as the post-diarization step.
            prepare_seconds = diarized_at.elapsed().as_secs_f64();
            emit(Event::Stage {
                stage: "transcription".to_string(),
                label: "Writing transcript".to_string(),
                progress: 1.0,
            });
            lines.sort_by(|a, b| {
                a.start
                    .partial_cmp(&b.start)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for line in std::mem::take(&mut lines) {
                emit(Event::Line {
                    start: line.start,
                    speaker: line.speaker.clone(),
                    text: line.text.clone(),
                    confidence: line.confidence,
                });
                lines.push(line);
            }
        } else {
            let transcription_base = if did_diarize {
                DIARIZATION_DONE
            } else {
                DECODE_DONE
            };
            let (raw, elapsed) = match finish_diarization(
                job,
                emit,
                &is_cancelled,
                &cache_keys,
                false,
                None,
            ) {
                (Ok(turns), elapsed) => (turns, elapsed),
                (Err(message), _) => {
                    fail(emit, message);
                    continue;
                }
            };
            diarize_seconds = elapsed;
            diarized_at = Instant::now();
            let (resolved, count) = resolve_and_emit_turns(
                raw,
                diarized,
                &input.display().to_string(),
                samples.as_slice(),
                diarize_seconds,
                emit,
            );
            turns = resolved;
            speaker_count = count;

            if let Some(transcriber) = &transcriber {
                // Consecutive turns are grouped into ~29 s clips so each ASR call
                // encodes one whisper window instead of one window per short turn.
                let clips = build_clips(&turns, duration as f32, rate, samples.as_slice());

                let total_turns = clips.len();
                let total_seconds: f64 = clips
                    .iter()
                    .map(|clip| (clip.to - clip.from) as f64 / rate as f64)
                    .sum();
                let mut done_seconds = 0.0f64;
                let mut done_turns = 0usize;

                emit(Event::Stage {
                    stage: "transcription".to_string(),
                    label: format!("Transcribing 0/{total_turns}"),
                    progress: transcription_base,
                });

                // Clips are already ordered by start time; the pool returns results
                // out of order, and the closure below re-orders them before any line
                // is emitted.
                let spans: Vec<(usize, usize)> =
                    clips.iter().map(|clip| (clip.from, clip.to)).collect();
                // A clip inside one turn can never be split between speakers, and
                // the speed preset never asks for token times, so both skip
                // whisper's extra pass.
                let token_flags: Vec<bool> = clips
                    .iter()
                    .map(|clip| !fast_decode && clip.turns.len() > 1)
                    .collect();
                let pinned_language = pin_language(
                    transcriber,
                    &decode_language,
                    samples.as_slice(),
                    rate,
                    spans.len(),
                    per_call_threads,
                );
                let language = decode_language.or(pinned_language.as_deref());
                let asr_started = Instant::now();
                let mut pending: BTreeMap<usize, Result<Vec<Segment>, String>> = BTreeMap::new();
                let mut buffer: Vec<Line> = Vec::new();
                let mut next_emit = 0usize;
                let mut last_checkpoint: Option<Instant> = None;

                run_asr_pool(
                    &spans,
                    &token_flags,
                    samples.as_slice(),
                    transcriber,
                    workers,
                    per_call_threads,
                    language,
                    fast_decode,
                    best_of,
                    audio_hash.as_ref(),
                    &model_name,
                    preprocess,
                    &is_cancelled,
                    &counters,
                    |index, result| {
                        // Progress reflects completed work.
                        done_seconds += (clips[index].to - clips[index].from) as f64 / rate as f64;
                        done_turns += 1;
                        pending.insert(index, result);

                        // Process completed clips in clip order. Overlapping turns can
                        // make a later clip's lines start earlier, so buffer lines and
                        // emit only up to the next unprocessed clip's start time.
                        while let Some(result) = pending.remove(&next_emit) {
                            let clip = &clips[next_emit];
                            match result {
                                Ok(segments) => {
                                    for segment in segments {
                                        buffer.extend(segment_lines(
                                            clip,
                                            &segment,
                                            &turns,
                                            &mut split_stats,
                                            &vocabulary,
                                        ));
                                    }
                                }
                                Err(message) => emit(Event::Log { message }),
                            }
                            next_emit += 1;

                            let watermark = clips
                                .get(next_emit)
                                .map(|clip| clip.start)
                                .unwrap_or(f32::INFINITY);
                            buffer.sort_by(|a, b| {
                                a.start
                                    .partial_cmp(&b.start)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let split = buffer.partition_point(|line| line.start <= watermark);
                            for line in buffer.drain(..split) {
                                emit(Event::Line {
                                    start: line.start,
                                    speaker: line.speaker.clone(),
                                    text: line.text.clone(),
                                    confidence: line.confidence,
                                });
                                lines.push(line);
                            }
                            if last_checkpoint
                                .is_none_or(|at: Instant| at.elapsed() >= CHECKPOINT_INTERVAL)
                            {
                                write_checkpoint(&checkpoint, &lines, checkpoint_hours);
                                last_checkpoint = Some(Instant::now());
                            }
                        }

                        let fraction = if total_seconds > 0.0 {
                            (done_seconds / total_seconds).clamp(0.0, 1.0)
                        } else {
                            1.0
                        };
                        emit(Event::Stage {
                            stage: "transcription".to_string(),
                            label: format!("Transcribing {done_turns}/{total_turns}"),
                            progress: (transcription_base
                                + (1.0 - transcription_base) * fraction as f32)
                                .clamp(0.0, 1.0),
                        });
                        emit(Event::Work {
                            done: done_seconds,
                            total: total_seconds,
                        });
                    },
                );

                // Flush anything still buffered (end of file or cancellation).
                buffer.sort_by(|a, b| {
                    a.start
                        .partial_cmp(&b.start)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for line in buffer.drain(..) {
                    emit(Event::Line {
                        start: line.start,
                        speaker: line.speaker.clone(),
                        text: line.text.clone(),
                        confidence: line.confidence,
                    });
                    lines.push(line);
                }

                asr_seconds = asr_started.elapsed().as_secs_f64();
                prepare_seconds = asr_started.duration_since(diarized_at).as_secs_f64();
            }
        }

        let asr_at = Instant::now();
        lines.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if diarized && split_stats.segments > 0 {
            // Compare files: a `collapsed` rate that climbs with duration is
            // the one-word fallback eating short turns, not diarization drift.
            crate::logging::info(format!(
                "speakers: {} segment(s), {} spanning turns, {} split, {} collapsed \
                 ({:.1}% collapsed), {} untimed ({:.1}%), {} turn(s)",
                split_stats.segments,
                split_stats.multi_turn,
                split_stats.split,
                split_stats.collapsed,
                100.0 * split_stats.collapsed as f64 / split_stats.segments as f64,
                split_stats.untimed,
                100.0 * split_stats.untimed as f64 / split_stats.segments as f64,
                turns.len()
            ));
        }

        let use_hours = duration >= 3600.0;
        let render = |with_timestamps: bool| {
            lines
                .iter()
                .map(|line| {
                    let stamp = format_timestamp(line.start, use_hours);
                    match (with_timestamps, line.speaker.is_empty()) {
                        (true, true) => format!("[{stamp}] {}", line.text),
                        (true, false) => format!("[{stamp}] {}: {}", line.speaker, line.text),
                        (false, true) => line.text.clone(),
                        (false, false) => format!("{}: {}", line.speaker, line.text),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let txt = render(false);
        let txt_ts = render(true);

        let path = output_path(input, &opts.output_dir, &opts.preset, speaker_count);
        let ts_path =
            output_path_with_suffix(input, &opts.output_dir, &opts.preset, speaker_count, "-ts");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(&path, format!("{txt}\n")) {
            emit(Event::Log {
                message: format!("could not write {}: {error}", path.display()),
            });
        }
        if let Err(error) = std::fs::write(&ts_path, format!("{txt_ts}\n")) {
            emit(Event::Log {
                message: format!("could not write {}: {error}", ts_path.display()),
            });
        }

        // Sidecar with per-line confidence and bounds, so the viewer, exports
        // and summaries all read the same record. Line order matches `txt_ts`.
        let json_path = ts_path.with_extension("json");
        let sidecar = crate::sidecar::Sidecar::new(
            lines
                .iter()
                .map(|line| crate::sidecar::Line {
                    time: format_timestamp(line.start, use_hours),
                    speaker: line.speaker.clone(),
                    text: line.text.clone(),
                    start: Some(line.start),
                    confidence: Some(line.confidence),
                    end: Some(line.end),
                    words: line.words.clone(),
                    ..crate::sidecar::Line::default()
                })
                .collect(),
            turns.clone(),
            crate::sidecar::Meta {
                audio: Some(input.display().to_string()),
                duration: Some(duration as f32),
                preset: Some(opts.preset.clone()),
                language: opts.language.clone(),
                model: None,
            },
        );
        if let Err(error) = crate::sidecar::write_at(&json_path, &sidecar) {
            emit(Event::Log {
                message: format!("could not write {}: {error}", json_path.display()),
            });
        }
        // The real transcript exists now; the parked copy is no longer needed.
        let _ = std::fs::remove_file(&checkpoint);

        let mut speaker_stats: HashMap<String, SpeakerStat> = HashMap::new();
        for line in &lines {
            let entry = speaker_stats.entry(line.speaker.clone()).or_default();
            entry.segments += 1;
            entry.words += line.text.split_whitespace().count();
        }
        for turn in &turns {
            let entry = speaker_stats.entry(turn.speaker.clone()).or_default();
            entry.seconds += (turn.end - turn.start).max(0.0);
        }

        emit(Event::Stage {
            stage: "done".to_string(),
            label: "Done".to_string(),
            progress: (index + 1) as f32 / total as f32,
        });
        let total_elapsed = file_start.elapsed().as_secs_f64().max(0.001);
        let name = input
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio");
        let (busy_seconds, calls, hits) = counters.seconds();
        let utilisation = if asr_seconds > 0.0 && workers > 0 {
            100.0 * busy_seconds / (asr_seconds * workers as f64)
        } else {
            0.0
        };
        emit(Event::Log {
            message: format!(
                "timing {name} [{}]: decode {:.2}s{} hash {:.2}s prepare {:.2}s diarize {:.2}s{} \
                 asr {:.2}s (busy {:.2}s, {:.0}% of {} worker(s), {calls} call(s), {hits} cached) \
                 write {:.2}s total {:.2}s (audio {:.1}s, {:.1}x realtime)",
                if overlap { "overlap" } else { "classic" },
                decoded_at.duration_since(decode_started).as_secs_f64(),
                if decode_prefetched {
                    " (prefetched)"
                } else {
                    ""
                },
                hash_elapsed,
                prepare_seconds,
                diarize_seconds,
                if overlap { " (parallel)" } else { "" },
                asr_seconds,
                busy_seconds,
                utilisation,
                workers,
                asr_at.elapsed().as_secs_f64(),
                total_elapsed,
                duration,
                duration / total_elapsed,
            ),
        });
        emit(Event::Done {
            txt: ts_path.display().to_string(),
            stats: Stats {
                audio_seconds: duration,
                run_seconds: file_start.elapsed().as_secs_f64(),
                speakers: speaker_count,
                num_speakers: opts.num_speakers,
                preset: opts.preset.clone(),
                speaker_stats,
                decode_seconds: decoded_at.duration_since(file_start).as_secs_f64(),
                diarize_seconds: diarized_at.duration_since(decoded_at).as_secs_f64(),
                asr_seconds: asr_at.duration_since(diarized_at).as_secs_f64(),
                prepare_seconds,
            },
        });
    }

    crate::logging::info(format!(
        "done: {} file(s) in {:.1}s",
        opts.inputs.len(),
        run_start.elapsed().as_secs_f64()
    ));
    emit(Event::AllDone);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_progress_follows_the_phase_that_is_behind() {
        // ASR ahead: diarization is the brake.
        assert!((overlap_progress(0.9, 0.2) - overlap_progress(0.2, 0.2)).abs() < 1e-6);
        // Diarization ahead: ASR is the brake.
        assert!((overlap_progress(0.1, 0.8) - overlap_progress(0.1, 0.1)).abs() < 1e-6);
        // Both done: stops short of 1.0 so attribution and writing still fit.
        assert!((overlap_progress(1.0, 1.0) - OVERLAP_DONE).abs() < 1e-6);
        // Neither phase can drive the bar past its end.
        assert!(overlap_progress(1.0, 0.0) >= DECODE_DONE);
        assert!(overlap_progress(0.0, 1.0) <= OVERLAP_DONE);
        // Out-of-range inputs are clamped instead of extrapolating.
        assert!(overlap_progress(-1.0, -1.0) >= DECODE_DONE);
        assert!(overlap_progress(2.0, 2.0) <= OVERLAP_DONE);
    }

    #[test]
    fn overlap_progress_is_monotonic_in_both_fractions() {
        let steps = [0.0, 0.05, 0.25, 0.5, 0.75, 0.95, 1.0];
        for pair in steps.windows(2) {
            let (before, after) = (pair[0], pair[1]);
            for fixed in steps {
                assert!(overlap_progress(after, fixed) >= overlap_progress(before, fixed));
                assert!(overlap_progress(fixed, after) >= overlap_progress(fixed, before));
            }
        }
    }

    #[test]
    fn routing_skips_diarization_for_one_speaker() {
        assert_eq!(route(Some(1)), PipelineMode::AsrOnly);
        assert_eq!(route(Some(2)), PipelineMode::Diarized);
        assert_eq!(route(Some(5)), PipelineMode::Diarized);
        // No speaker count means auto behaviour, which still diarizes.
        assert_eq!(route(None), PipelineMode::Diarized);
        assert!(!PipelineMode::AsrOnly.diarizes());
        assert!(PipelineMode::Diarized.diarizes());
    }

    #[test]
    fn asr_only_with_single_speaker_labels_lines() {
        let segment = Segment {
            start: 0.0,
            end: 2.0,
            text: "hello".to_string(),
            confidence: 0.0,
            tokens: Vec::new(),
        };
        let clip = |turns: &[Turn]| Clip {
            turns: turns
                .iter()
                .enumerate()
                .map(|(index, turn)| (index, turn.start, turn.end))
                .collect(),
            start: 0.0,
            from: 0,
            to: 32_000,
        };
        let vocabulary = crate::vocabulary::Vocabulary::default();

        let labelled = asr_only_turns(2.0, true);
        let mut stats = SplitStats::default();
        let lines = segment_lines(
            &clip(&labelled),
            &segment,
            &labelled,
            &mut stats,
            &vocabulary,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].speaker, "SPEAKER 1");

        // Without the option, the ASR-only path still has no label.
        let unlabelled = asr_only_turns(2.0, false);
        let mut stats = SplitStats::default();
        let lines = segment_lines(
            &clip(&unlabelled),
            &segment,
            &unlabelled,
            &mut stats,
            &vocabulary,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].speaker, "");
    }

    #[test]
    fn overlapping_short_turns_do_not_skip_the_rest_of_a_long_turn() {
        let turns = vec![
            Turn {
                start: 0.0,
                end: 60.0,
                speaker: "SPEAKER 1".to_string(),
            },
            Turn {
                start: 10.0,
                end: 12.0,
                speaker: "SPEAKER 2".to_string(),
            },
            Turn {
                start: 61.0,
                end: 62.0,
                speaker: "SPEAKER 2".to_string(),
            },
        ];
        let rate = 16_000;
        let samples = vec![0.1; 70 * rate as usize];
        let clips = build_clips(&turns, 70.0, rate, &samples);
        let covered: f32 = clips
            .iter()
            .map(|clip| (clip.to - clip.from) as f32 / rate as f32)
            .sum();
        assert!(
            covered >= 60.0,
            "long turn coverage was only {covered} seconds"
        );
        assert!(
            clips
                .iter()
                .any(|clip| clip.start > 55.0 && clip.start < 60.0),
            "the tail of the long turn was not transcribed"
        );
    }

    #[test]
    fn token_timestamps_split_a_segment_between_speakers() {
        let turns = vec![
            Turn {
                start: 0.0,
                end: 1.0,
                speaker: "SPEAKER 1".to_string(),
            },
            Turn {
                start: 1.0,
                end: 2.0,
                speaker: "SPEAKER 2".to_string(),
            },
        ];
        let clip = Clip {
            turns: vec![(0, 0.0, 1.0), (1, 1.0, 2.0)],
            start: 0.0,
            from: 0,
            to: 32_000,
        };
        let segment = Segment {
            start: 0.0,
            end: 2.0,
            text: "hello there general kenobi".to_string(),
            confidence: 0.0,
            tokens: vec![
                crate::asr::Token {
                    start: 0.0,
                    end: 0.4,
                    text: " hello".to_string(),
                },
                crate::asr::Token {
                    start: 0.4,
                    end: 0.9,
                    text: " there".to_string(),
                },
                crate::asr::Token {
                    start: 1.1,
                    end: 1.5,
                    text: " general".to_string(),
                },
                crate::asr::Token {
                    start: 1.5,
                    end: 1.9,
                    text: " kenobi".to_string(),
                },
            ],
        };
        let mut stats = SplitStats::default();
        let lines = segment_lines(
            &clip,
            &segment,
            &turns,
            &mut stats,
            &crate::vocabulary::Vocabulary::default(),
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].speaker, "SPEAKER 1");
        assert_eq!(lines[0].text, "hello there");
        assert_eq!(lines[1].speaker, "SPEAKER 2");
        assert_eq!(lines[1].text, "general kenobi");
        assert_eq!(stats.segments, 1);
        assert_eq!(stats.multi_turn, 1);
        assert_eq!(stats.split, 1);
        assert_eq!(stats.collapsed, 0);

        // One detached token is a boundary artefact, not a speaker change.
        let mut short = segment.clone();
        short.tokens[2].start = 0.95;
        short.tokens[2].end = 1.0;
        let mut stats = SplitStats::default();
        let lines = segment_lines(
            &clip,
            &short,
            &turns,
            &mut stats,
            &crate::vocabulary::Vocabulary::default(),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(stats.collapsed, 1);

        // A one-word interjection that actually lasts is a real speaker change.
        let mut spoken = segment.clone();
        spoken.tokens[2].start = 1.0;
        spoken.tokens[2].end = 1.75;
        spoken.tokens.truncate(3);
        let mut stats = SplitStats::default();
        let lines = segment_lines(
            &clip,
            &spoken,
            &turns,
            &mut stats,
            &crate::vocabulary::Vocabulary::default(),
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].speaker, "SPEAKER 2");
        assert_eq!(lines[1].text, "general");
        assert_eq!(stats.collapsed, 0);
    }

    #[test]
    fn vocabulary_replacements_reach_the_transcript_text() {
        let turns = vec![Turn {
            start: 0.0,
            end: 2.0,
            speaker: "SPEAKER 1".to_string(),
        }];
        let clip = Clip {
            turns: vec![(0, 0.0, 2.0)],
            start: 0.0,
            from: 0,
            to: 32_000,
        };
        let segment = Segment {
            start: 0.0,
            end: 2.0,
            text: "we use pie torch".to_string(),
            confidence: 0.0,
            tokens: Vec::new(),
        };
        let vocabulary = crate::vocabulary::Vocabulary::from_fields("", "pie torch = PyTorch");
        let mut stats = SplitStats::default();

        let lines = segment_lines(&clip, &segment, &turns, &mut stats, &vocabulary);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "we use PyTorch");
    }

    #[test]
    fn token_pieces_group_into_words() {
        let tokens = vec![
            (0.0, 0.4, " Hello".to_string()),
            (0.4, 0.9, " there".to_string()),
            (0.9, 1.0, ",".to_string()),
            (1.1, 1.5, " gen".to_string()),
            (1.5, 1.8, "eral".to_string()),
        ];
        let words = words_from_tokens(&tokens);
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].start, 0.0);
        assert_eq!(words[1].text, "there,");
        assert_eq!(words[1].end, 1.0);
        assert_eq!(words[2].text, "general");
        assert_eq!(words[2].end, 1.8);
    }

    #[test]
    fn the_parked_transcript_sits_beside_the_audio() {
        let path = checkpoint_path(Path::new("/tmp/interview.mp3"));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".interview.mp3.hippo-transcribe.partial")
        );
        assert_eq!(path.parent(), Some(Path::new("/tmp")));
    }

    #[test]
    fn output_name_includes_the_preset() {
        let path = output_path(Path::new("/tmp/my-audio.mp3"), &None, "speed", 2);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("my-audio-speed-transcription-2speakers.txt")
        );

        let out = PathBuf::from("/out");
        let path = output_path(Path::new("/tmp/my-audio.mp3"), &Some(out), "accuracy", 1);
        assert_eq!(
            path,
            PathBuf::from("/out/my-audio-accuracy-transcription-1speaker.txt")
        );
    }

    #[test]
    fn schedule_keeps_low_core_machines_sequential() {
        // The overlap schedule contends for cores; small machines default to
        // the sequential path.
        assert_eq!(Schedule::from_setting(None, 2), Schedule::Classic);
        assert_eq!(Schedule::from_setting(None, 4), Schedule::Classic);
        assert_eq!(Schedule::from_setting(None, 8), Schedule::Overlap);
        // An explicit setting wins either way.
        assert_eq!(
            Schedule::from_setting(Some("classic"), 32),
            Schedule::Classic
        );
        assert_eq!(
            Schedule::from_setting(Some("overlap"), 2),
            Schedule::Overlap
        );
    }

    #[test]
    fn fixed_windows_cover_speech_and_skip_silence() {
        let rate = 16_000u32;
        let duration = 40.0f32;
        let mut samples = vec![0.0f32; (duration * rate as f32) as usize];
        // Speech from 2 s to 20 s and 32 s to 38 s; the rest is silence.
        for (from, to) in [(2.0f32, 20.0f32), (32.0, 38.0)] {
            for index in (from * rate as f32) as usize..(to * rate as f32) as usize {
                samples[index] = if index % 2 == 0 { 0.2 } else { -0.2 };
            }
        }
        let windows = build_windows(duration, rate, samples.as_slice());
        assert!(!windows.is_empty());
        // No window may be entirely inside one of the silent stretches.
        for window in &windows {
            assert!(span_rms(samples.as_slice(), window.from, window.to) >= SPEECH_THRESHOLD);
        }
        // Every speech sample is covered by some window.
        let covered = |second: f32| {
            windows.iter().any(|window| {
                window.start <= second
                    && second <= window.start + (window.to - window.from) as f32 / rate as f32
            })
        };
        assert!(covered(3.0));
        assert!(covered(19.0));
        assert!(covered(35.0));
    }

    #[test]
    fn quiet_cut_prefers_a_silent_gap() {
        let rate = 16_000u32;
        let mut samples = vec![0.0f32; 40 * rate as usize];
        // Loud everywhere except a 0.4 s pocket centred on 30 s.
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = if index % 2 == 0 { 0.3 } else { -0.3 };
        }
        let pocket = 30.0 * rate as f32;
        let half = 0.2 * rate as f32;
        for index in (pocket - half) as usize..(pocket + half) as usize {
            samples[index] = 0.0;
        }
        let cut = quiet_cut(samples.as_slice(), rate, 20.0, 29.5);
        assert!(
            (cut - 30.0).abs() <= 1.6,
            "cut {cut} should land near the silent pocket"
        );
    }
}
