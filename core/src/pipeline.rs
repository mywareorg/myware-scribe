//! The diarize + transcribe pipeline.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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
/// Never prefetch-decode a file longer than this: decoded 16 kHz f32 audio for
/// 30 minutes is ~115 MB, so a batch stays within a predictable memory budget.
const PREFETCH_MAX_SECONDS: f64 = 1800.0;
const DIARIZATION_DONE: f32 = 0.45;

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
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone)]
struct Line {
    start: f32,
    speaker: String,
    text: String,
    confidence: f32,
}

/// Merge near-adjacent same-speaker turns and drop ultra-short fragments.
/// Returns the turns plus how many were merged and how many were dropped, so
/// the log can tell model granularity apart from our own filtering.
fn clean_turns(turns: Vec<Turn>) -> (Vec<Turn>, usize, usize) {
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

/// Split one ASR segment at speaker changes using token timestamps. When the
/// model did not provide token times, or every token belongs to one turn,
/// return the original segment as a single line.
fn segment_lines(
    clip: &Clip,
    segment: &Segment,
    turns: &[Turn],
    stats: &mut SplitStats,
) -> Vec<Line> {
    stats.segments += 1;
    let speaker_for = |turn: Option<usize>| {
        turn.and_then(|index| turns.get(index))
            .map(|turn| turn.speaker.clone())
            .unwrap_or_default()
    };
    let whole = || Line {
        start: clip.start + segment.start,
        speaker: speaker_for(attribute_turn(
            clip,
            clip.start + segment.start,
            clip.start + segment.end,
        )),
        text: segment.text.clone(),
        confidence: segment.confidence,
    };

    if segment.tokens.len() < 2 {
        stats.untimed += 1;
        return vec![whole()];
    }

    let mut groups: Vec<(Option<usize>, String, f32, f32, usize)> = Vec::new();
    for token in &segment.tokens {
        let start = clip.start + token.start;
        let end = clip.start + token.end;
        let turn = attribute_turn(clip, start, end);
        match groups.last_mut() {
            Some((last_turn, text, _, last_end, count)) if *last_turn == turn => {
                text.push_str(&token.text);
                *last_end = end;
                *count += 1;
            }
            _ => groups.push((turn, token.text.clone(), start, end, 1)),
        }
    }

    // A brief detached fragment is a diarization boundary error, not a real
    // speaker change. Judge that by duration, not word count: one word that
    // takes half a second ("What?") is a real interjection, one that barely
    // covers a moment is an artefact.
    let mut runs: Vec<Option<usize>> = groups.iter().map(|(turn, _, _, _, _)| *turn).collect();
    runs.dedup();
    if runs.len() > 1 {
        stats.multi_turn += 1;
    }
    if groups.len() <= 1 {
        return vec![whole()];
    }
    let fragment = groups
        .iter()
        .any(|(_, _, start, end, count)| *count < 2 && end - start < MIN_GROUP_S);
    if fragment {
        stats.collapsed += 1;
        return vec![whole()];
    }
    stats.split += 1;

    groups
        .into_iter()
        .filter_map(|(turn, text, start, _, _)| {
            let text = text.trim().to_string();
            (!text.is_empty()).then(|| Line {
                start,
                speaker: speaker_for(turn),
                text,
                confidence: segment.confidence,
            })
        })
        .collect()
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
    let embedding = models::embedding_model(&opts.preset);
    let whisper = models::whisper_model(&opts.preset);
    let diarizer = models::diarizer_id(&opts.preset);
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

    let transcriber = match Transcriber::load(&whisper) {
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
    let per_call_threads = env_int("HIPPO_TRANSCRIBE_ASR_THREADS", "TRANSIEL_ASR_THREADS")
        .map(|v| v as i32)
        .unwrap_or((physical / workers).max(1) as i32)
        .max(1);

    // Decode the next file while the current one is diarized and transcribed.
    // This only helps batches; a single file is decoded inline.
    type Decoded = Result<(Vec<f32>, u32), String>;
    let mut prefetched: Option<std::thread::JoinHandle<Decoded>> = None;

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

        let decoded = match prefetched.take() {
            Some(handle) => handle
                .join()
                .unwrap_or_else(|_| Err("audio decode thread panicked".to_string())),
            None => audio::decode(input),
        };
        if index + 1 < opts.inputs.len() {
            let next = opts.inputs[index + 1].clone();
            let small =
                audio::duration_fast(&next).is_some_and(|seconds| seconds <= PREFETCH_MAX_SECONDS);
            if small {
                prefetched = Some(std::thread::spawn(move || audio::decode(&next)));
            }
        }
        let (samples, rate) = match decoded {
            Ok(value) => value,
            Err(message) => {
                fail(emit, message);
                continue;
            }
        };
        let duration = samples.len() as f64 / rate as f64;
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

        // Hash the audio once: diarization and per-clip ASR caches both use it.
        let audio_hash = if opts.cache {
            cache::audio_hash(input)
        } else {
            None
        };
        let cache_key = if diarized && opts.num_speakers != Some(1) {
            audio_hash
                .as_deref()
                .map(|hash| cache::key_from_hash(hash, &diarizer, opts.num_speakers))
        } else {
            None
        };
        let mut did_diarize = false;
        let raw = if !diarized {
            // ASR-only mode: no speaker turns, no sherpa models, no cache.
            vec![Turn {
                start: 0.0,
                end: duration as f32,
                speaker: String::new(),
            }]
        } else if opts.num_speakers == Some(1) {
            // One speaker: skip diarization entirely and transcribe the file as
            // a single turn.
            vec![Turn {
                start: 0.0,
                end: duration as f32,
                speaker: "SPEAKER 1".to_string(),
            }]
        } else {
            match cache_key.as_deref().and_then(cache::read) {
                Some(turns) => {
                    emit(Event::Log {
                        message: "Using cached diarization.".to_string(),
                    });
                    emit(Event::Stage {
                        stage: "diarization".to_string(),
                        label: "Running Diarization 100%".to_string(),
                        progress: DECODE_DONE,
                    });
                    turns
                }
                None => {
                    did_diarize = true;
                    let progress = Arc::new(AtomicU32::new(0));
                    let progress_for_thread = progress.clone();
                    let segmentation = segmentation.clone();
                    let embedding = embedding.clone();
                    let audio = samples.clone();
                    let num_speakers = opts.num_speakers;
                    let profile = diarize::profile(&opts.preset);
                    let handle = std::thread::spawn(move || {
                        diarize::diarize_with_progress(
                            audio.as_slice(),
                            &segmentation,
                            &embedding,
                            num_speakers,
                            profile.threshold,
                            profile.min_duration_on,
                            profile.min_duration_off,
                            progress_for_thread,
                        )
                    });

                    emit(Event::Stage {
                        stage: "diarization".to_string(),
                        label: "Running Diarization 0%".to_string(),
                        progress: DECODE_DONE,
                    });
                    while !handle.is_finished() {
                        if is_cancelled() {
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
                        std::thread::sleep(Duration::from_millis(100));
                    }

                    let joined = handle.join();
                    emit(Event::Stage {
                        stage: "diarization".to_string(),
                        label: "Running Diarization 100%".to_string(),
                        progress: DIARIZATION_DONE,
                    });
                    match joined {
                        Ok(Ok(turns)) => {
                            if let Some(key) = &cache_key {
                                cache::write(key, &turns);
                            }
                            turns
                        }
                        Ok(Err(message)) => {
                            fail(emit, message);
                            continue;
                        }
                        Err(_) => {
                            fail(emit, "diarization thread panicked".to_string());
                            continue;
                        }
                    }
                }
            }
        };
        let diarized_at = Instant::now();
        let transcription_base = if did_diarize {
            DIARIZATION_DONE
        } else {
            DECODE_DONE
        };
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
                 dropped as short, {} kept, {} speaker(s)",
                cleaned.len(),
                map.len()
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
            audio: input.display().to_string(),
            peaks: audio::rms_peaks(samples.as_slice(), TIMELINE_BUCKETS),
        });

        let mut lines: Vec<Line> = Vec::new();
        let mut split_stats = SplitStats::default();
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

            // Every preset uses a multilingual model, so the requested
            // language (or auto-detection) always applies. `auto`/empty mean
            // detection, matching the CLI.
            let decode_language = opts
                .language
                .as_deref()
                .filter(|code| !code.is_empty() && !code.eq_ignore_ascii_case("auto"));
            let fast_decode = opts.preset.eq_ignore_ascii_case("speed");
            // `best_of` only repeats the decoder; the encoder dominates. Keep
            // it at 1 for speed and at 2 for the quality tiers.
            let best_of = if fast_decode { 1 } else { 2 };
            let model_name = models::whisper_relative(&opts.preset);

            emit(Event::Stage {
                stage: "transcription".to_string(),
                label: format!("Transcribing 0/{total_turns}"),
                progress: transcription_base,
            });

            // Worker pool + channel: turns finish out of order, but lines are
            // emitted strictly in clip (turn/start) order.
            let (tx, rx) = std::sync::mpsc::channel();
            let next = AtomicUsize::new(0);
            let samples_ref = samples.as_slice();
            let clips_ref = clips.as_slice();

            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let tx = tx.clone();
                    let next = &next;
                    let model_name = model_name.clone();
                    let audio_hash = audio_hash.clone();
                    scope.spawn(move || loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= clips_ref.len() {
                            break;
                        }
                        let clip = &clips_ref[index];
                        let key = audio_hash.as_ref().map(|hash| {
                            cache::clip_key(
                                hash,
                                &model_name,
                                decode_language,
                                clip.from,
                                clip.to,
                                best_of,
                                fast_decode,
                            )
                        });
                        let result = match key.as_ref().and_then(|key| cache::read_segments(key)) {
                            Some(segments) => Ok(segments),
                            None => {
                                let result = transcriber.transcribe_with_threads_and_language(
                                    &samples_ref[clip.from..clip.to],
                                    per_call_threads,
                                    decode_language,
                                    fast_decode,
                                    best_of,
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
                // Drop the spare sender so the channel closes when workers finish.
                drop(tx);

                let mut pending: BTreeMap<usize, Result<Vec<Segment>, String>> = BTreeMap::new();
                let mut buffer: Vec<Line> = Vec::new();
                let mut next_emit = 0usize;
                let mut remaining = clips.len();
                while remaining > 0 {
                    if is_cancelled() {
                        break;
                    }
                    let Ok((index, result)) = rx.recv() else {
                        break;
                    };
                    remaining -= 1;

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
                }

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
            });
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
        emit(Event::Log {
            message: format!(
                "timing {name}: decode {:.2}s diarize {:.2}s asr {:.2}s write {:.2}s total {:.2}s (audio {:.1}s, {:.1}x realtime)",
                decoded_at.duration_since(file_start).as_secs_f64(),
                diarized_at.duration_since(decoded_at).as_secs_f64(),
                asr_at.duration_since(diarized_at).as_secs_f64(),
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
        let lines = segment_lines(&clip, &segment, &turns, &mut stats);
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
        let lines = segment_lines(&clip, &short, &turns, &mut stats);
        assert_eq!(lines.len(), 1);
        assert_eq!(stats.collapsed, 1);

        // A one-word interjection that actually lasts is a real speaker change.
        let mut spoken = segment.clone();
        spoken.tokens[2].start = 1.0;
        spoken.tokens[2].end = 1.75;
        spoken.tokens.truncate(3);
        let mut stats = SplitStats::default();
        let lines = segment_lines(&clip, &spoken, &turns, &mut stats);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].speaker, "SPEAKER 2");
        assert_eq!(lines[1].text, "general");
        assert_eq!(stats.collapsed, 0);
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
}
