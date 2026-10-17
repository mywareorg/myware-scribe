//! The diarize + transcribe pipeline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::asr::Transcriber;
use crate::audio;
use crate::cache;
use crate::diarize::{self, CLUSTER_THRESHOLD};
use crate::event::{Event, SpeakerStat, Stats, Turn};
use crate::models;
use rayon::prelude::*;

const MERGE_GAP_S: f32 = 1.0;
const MIN_TURN_S: f32 = 0.4;
const CLIP_PAD_S: f32 = 0.1;

#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub inputs: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub preset: String,
    pub num_speakers: Option<i32>,
    pub timestamps: bool,
    pub join_speaker: bool,
    pub cache: bool,
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

fn join_speaker_lines(lines: Vec<Line>) -> Vec<Line> {
    let mut merged: Vec<Line> = Vec::new();
    for line in lines {
        match merged.last_mut() {
            Some(last) if last.speaker == line.speaker => {
                last.text = format!("{} {}", last.text, line.text).trim().to_string();
            }
            _ => merged.push(line),
        }
    }
    merged
}

fn output_path(input: &Path, output_dir: &Option<PathBuf>) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("transcript");
    let name = format!("{stem}-transcript.txt");
    match output_dir {
        Some(dir) => dir.join(name),
        None => input.with_file_name(name),
    }
}

type ClipResult = (usize, f32, Result<Vec<crate::asr::Segment>, String>);

/// Transcribe the turn clips in parallel (order preserved by index).
fn transcribe_clips(
    transcriber: &Transcriber,
    samples: &[f32],
    clips: &[(usize, f32, usize, usize)],
    threads: i32,
) -> Vec<ClipResult> {
    clips
        .par_iter()
        .map(|&(turn_index, clip_start, from, to)| {
            (
                turn_index,
                clip_start,
                transcriber.transcribe_with_threads(&samples[from..to], threads),
            )
        })
        .collect()
}

/// Run the pipeline over all inputs, emitting progress events.
pub fn run(opts: &PipelineOptions, emit: &mut dyn FnMut(Event)) {
    let total = opts.inputs.len().max(1);
    let is_cancelled = || opts.cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst));

    let segmentation = models::segmentation_model();
    let embedding = models::embedding_model();
    let whisper = models::whisper_model(&opts.preset);
    let diarizer = models::diarizer_id();

    let transcriber = match Transcriber::load(&whisper) {
        Ok(transcriber) => Some(transcriber),
        Err(message) => {
            emit(Event::Error { message });
            None
        }
    };

    // Parallel ASR: a small pool of workers sharing one model (each call creates
    // its own decode state). Per-call threads split the physical cores.
    let physical = num_cpus::get_physical().max(1);
    let env_int = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<usize>().ok());
    let workers = env_int("TRANSIEL_ASR_WORKERS")
        .unwrap_or(physical.min(4))
        .max(1);
    let per_call_threads = env_int("TRANSIEL_ASR_THREADS")
        .map(|v| v as i32)
        .unwrap_or((physical / workers).max(1) as i32)
        .max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .ok();

    for (index, input) in opts.inputs.iter().enumerate() {
        if is_cancelled() {
            break;
        }
        let file_start = Instant::now();
        emit(Event::File {
            audio: input.display().to_string(),
        });
        emit(Event::Stage {
            stage: "loading".to_string(),
            label: "Loading".to_string(),
            progress: index as f32 / total as f32,
        });

        let (samples, rate) = match audio::decode(input) {
            Ok(value) => value,
            Err(message) => {
                emit(Event::Error { message });
                continue;
            }
        };
        let duration = samples.len() as f64 / rate as f64;
        emit(Event::Meta {
            audio: input.display().to_string(),
            duration: duration as f32,
        });

        emit(Event::Stage {
            stage: "diarization".to_string(),
            label: "Diarizing".to_string(),
            progress: 0.0,
        });
        let cache_key = if opts.cache {
            cache::key(input, &diarizer, opts.num_speakers)
        } else {
            None
        };
        let raw = match cache_key.as_deref().and_then(cache::read) {
            Some(turns) => {
                emit(Event::Log {
                    message: "Using cached diarization.".to_string(),
                });
                turns
            }
            None => {
                match diarize::diarize(
                    &samples,
                    &segmentation,
                    &embedding,
                    opts.num_speakers,
                    CLUSTER_THRESHOLD,
                ) {
                    Ok(turns) => {
                        if let Some(key) = &cache_key {
                            cache::write(key, &turns);
                        }
                        turns
                    }
                    Err(message) => {
                        emit(Event::Error { message });
                        continue;
                    }
                }
            }
        };
        let cleaned = clean_turns(raw);
        let map = build_speaker_map(&cleaned);
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

        emit(Event::Stage {
            stage: "transcription".to_string(),
            label: "Transcribing".to_string(),
            progress: 0.0,
        });
        let mut lines: Vec<Line> = Vec::new();
        if let Some(transcriber) = &transcriber {
            // (turn_index, clip_start, from, to) for each transcribable turn.
            let clips: Vec<(usize, f32, usize, usize)> = turns
                .iter()
                .enumerate()
                .filter_map(|(turn_index, turn)| {
                    let clip_start = (turn.start - CLIP_PAD_S).max(0.0);
                    let clip_end = (turn.end + CLIP_PAD_S).min(duration as f32);
                    let from = (clip_start * rate as f32) as usize;
                    let to = ((clip_end * rate as f32) as usize).min(samples.len());
                    (to > from && (to - from) >= (0.15 * rate as f32) as usize)
                        .then_some((turn_index, clip_start, from, to))
                })
                .collect();

            let results = match &pool {
                Some(pool) => pool.install(|| {
                    transcribe_clips(transcriber, &samples, &clips, per_call_threads)
                }),
                None => transcribe_clips(transcriber, &samples, &clips, per_call_threads),
            };

            // par_iter().collect() preserves order, so results are in turn order.
            for (turn_index, clip_start, result) in results {
                if is_cancelled() {
                    break;
                }
                let speaker = turns[turn_index].speaker.clone();
                match result {
                    Ok(segments) => {
                        for segment in segments {
                            let line = Line {
                                start: clip_start + segment.start,
                                speaker: speaker.clone(),
                                text: segment.text,
                                confidence: segment.confidence,
                            };
                            emit(Event::Line {
                                start: line.start,
                                speaker: line.speaker.clone(),
                                text: line.text.clone(),
                                confidence: line.confidence,
                            });
                            lines.push(line);
                        }
                    }
                    Err(message) => emit(Event::Log { message }),
                }
            }
            emit(Event::Stage {
                stage: "transcription".to_string(),
                label: "Transcribing".to_string(),
                progress: 1.0,
            });
        }

        lines.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let use_hours = duration >= 3600.0;
        let ordered = if opts.join_speaker {
            join_speaker_lines(lines)
        } else {
            lines
        };
        let txt = ordered
            .iter()
            .map(|line| {
                if opts.timestamps {
                    format!(
                        "[{}] {}: {}",
                        format_timestamp(line.start, use_hours),
                        line.speaker,
                        line.text
                    )
                } else {
                    format!("{}: {}", line.speaker, line.text)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let path = output_path(input, &opts.output_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(&path, format!("{txt}\n")) {
            emit(Event::Log {
                message: format!("could not write {}: {error}", path.display()),
            });
        }

        let mut speaker_stats: HashMap<String, SpeakerStat> = HashMap::new();
        for line in &ordered {
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
            txt: path.display().to_string(),
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
