//! The diarize + transcribe pipeline.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::asr::{Segment, Transcriber};
use crate::audio;
use crate::cache;
use crate::diarize::{self, CLUSTER_THRESHOLD};
use crate::event::{Event, SpeakerStat, Stats, Turn};
use crate::models;

const MERGE_GAP_S: f32 = 1.0;
const MIN_TURN_S: f32 = 0.4;
const CLIP_PAD_S: f32 = 0.1;
/// Bucket count for the waveform emitted once diarization identifies speakers.
const TIMELINE_BUCKETS: usize = 2000;
/// Per-file progress after decoding and after diarization. Keeping these as
/// fixed phase boundaries makes the progress bar monotonic across stages
/// instead of jumping back to zero when transcription starts.
const DECODE_DONE: f32 = 0.03;
const DIARIZATION_DONE: f32 = 0.45;

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
fn clean_turns(turns: Vec<Turn>) -> Vec<Turn> {
    let mut merged: Vec<Turn> = Vec::new();
    for turn in turns {
        match merged.last_mut() {
            Some(last) if last.speaker == turn.speaker && turn.start - last.end <= MERGE_GAP_S => {
                last.end = last.end.max(turn.end);
            }
            _ => merged.push(turn),
        }
    }
    merged
        .into_iter()
        .filter(|t| t.end - t.start >= MIN_TURN_S)
        .collect()
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

/// Run the pipeline over all inputs, emitting progress events.
pub fn run(opts: &PipelineOptions, emit: &mut dyn FnMut(Event)) {
    let total = opts.inputs.len().max(1);
    let is_cancelled = || {
        opts.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
    };

    let segmentation = models::segmentation_model();
    let embedding = models::embedding_model();
    let whisper = models::whisper_model(&opts.preset);
    let diarizer = models::diarizer_id();

    emit(Event::Stage {
        stage: "loading".to_string(),
        label: "Loading models".to_string(),
        progress: 0.0,
    });

    if let Err(message) = diarize::ensure_available() {
        emit(Event::Error { message });
        return;
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
            emit(Event::Error {
                message: format!("{name} model unavailable — {hint}"),
            });
            return;
        }
    };

    // Parallel ASR: a small pool of workers sharing one model (each call creates
    // its own decode state). Per-call threads split the physical cores.
    let physical = num_cpus::get_physical().max(1);
    let env_int = |current: &str, legacy: &str| {
        crate::env_compat(current, legacy).and_then(|v| v.parse::<usize>().ok())
    };
    let workers = env_int("HIPPO_TRANSCRIBE_ASR_WORKERS", "TRANSIEL_ASR_WORKERS")
        .unwrap_or(physical.min(4))
        .max(1);
    let per_call_threads = env_int("HIPPO_TRANSCRIBE_ASR_THREADS", "TRANSIEL_ASR_THREADS")
        .map(|v| v as i32)
        .unwrap_or((physical / workers).max(1) as i32)
        .max(1);

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

        let (samples, rate) = match audio::decode(input) {
            Ok(value) => value,
            Err(message) => {
                emit(Event::Error { message });
                continue;
            }
        };
        let duration = samples.len() as f64 / rate as f64;
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

        let cache_key = if opts.cache && opts.num_speakers != Some(1) {
            cache::key(input, &diarizer, opts.num_speakers)
        } else {
            None
        };
        let mut did_diarize = false;
        let raw = if opts.num_speakers == Some(1) {
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
                    let handle = std::thread::spawn(move || {
                        diarize::diarize_with_progress(
                            audio.as_slice(),
                            &segmentation,
                            &embedding,
                            num_speakers,
                            CLUSTER_THRESHOLD,
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
                            emit(Event::Error { message });
                            continue;
                        }
                        Err(_) => {
                            emit(Event::Error {
                                message: "diarization thread panicked".to_string(),
                            });
                            continue;
                        }
                    }
                }
            }
        };
        let transcription_base = if did_diarize {
            DIARIZATION_DONE
        } else {
            DECODE_DONE
        };
        let cleaned = clean_turns(raw);
        let mut map = build_speaker_map(&cleaned);
        if map.len() <= 1 {
            for label in map.values_mut() {
                label.clear();
            }
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
        if let Some(transcriber) = &transcriber {
            // (turn_index, clip_start, from, to) for each transcribable clip.
            // Long turns are chunked so progress and ETA advance during a
            // single-speaker file instead of waiting for the whole turn.
            const MAX_CLIP_S: f32 = 45.0;
            let mut clips: Vec<(usize, f32, usize, usize)> = Vec::new();
            for (turn_index, turn) in turns.iter().enumerate() {
                let clip_start = (turn.start - CLIP_PAD_S).max(0.0);
                let clip_end = (turn.end + CLIP_PAD_S).min(duration as f32);
                let mut chunk_start = clip_start;
                while chunk_start < clip_end {
                    let chunk_end = (chunk_start + MAX_CLIP_S).min(clip_end);
                    let from = (chunk_start * rate as f32) as usize;
                    let to = ((chunk_end * rate as f32) as usize).min(samples.len());
                    if to > from && (to - from) >= (0.15 * rate as f32) as usize {
                        clips.push((turn_index, chunk_start, from, to));
                    }
                    chunk_start = chunk_end;
                }
            }

            let total_turns = clips.len();
            let total_seconds: f64 = clips
                .iter()
                .map(|(_, _, from, to)| (to - from) as f64 / rate as f64)
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
                    scope.spawn(move || loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= clips_ref.len() {
                            break;
                        }
                        let (turn_index, clip_start, from, to) = clips_ref[index];
                        let result = transcriber.transcribe_with_threads_and_language(
                            &samples_ref[from..to],
                            per_call_threads,
                            decode_language,
                            fast_decode,
                        );
                        if tx.send((index, turn_index, clip_start, result)).is_err() {
                            break;
                        }
                    });
                }
                // Drop the spare sender so the channel closes when workers finish.
                drop(tx);

                let mut pending: BTreeMap<usize, (usize, f32, Result<Vec<Segment>, String>)> =
                    BTreeMap::new();
                let mut buffer: Vec<Line> = Vec::new();
                let mut next_emit = 0usize;
                let mut remaining = clips.len();
                while remaining > 0 {
                    if is_cancelled() {
                        break;
                    }
                    let Ok((index, turn_index, clip_start, result)) = rx.recv() else {
                        break;
                    };
                    remaining -= 1;

                    // Progress reflects completed work.
                    done_seconds += (clips[index].3 - clips[index].2) as f64 / rate as f64;
                    done_turns += 1;
                    pending.insert(index, (turn_index, clip_start, result));

                    // Process completed clips in clip order. Overlapping turns can
                    // make a later clip's lines start earlier, so buffer lines and
                    // emit only up to the next unprocessed clip's start time.
                    while let Some((turn_index, clip_start, result)) = pending.remove(&next_emit) {
                        let speaker = turns[turn_index].speaker.clone();
                        match result {
                            Ok(segments) => {
                                for segment in segments {
                                    buffer.push(Line {
                                        start: clip_start + segment.start,
                                        speaker: speaker.clone(),
                                        text: segment.text,
                                        confidence: segment.confidence,
                                    });
                                }
                            }
                            Err(message) => emit(Event::Log { message }),
                        }
                        next_emit += 1;

                        let watermark = clips
                            .get(next_emit)
                            .map(|clip| clip.1)
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

        lines.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

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

        let path = output_path(input, &opts.output_dir, &opts.preset, map.len());
        let ts_path =
            output_path_with_suffix(input, &opts.output_dir, &opts.preset, map.len(), "-ts");
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
        emit(Event::Done {
            txt: ts_path.display().to_string(),
            stats: Stats {
                audio_seconds: duration,
                run_seconds: file_start.elapsed().as_secs_f64(),
                speakers: map.len(),
                num_speakers: opts.num_speakers,
                preset: opts.preset.clone(),
                speaker_stats,
            },
        });
    }

    emit(Event::AllDone);
}

#[cfg(test)]
mod tests {
    use super::*;

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
