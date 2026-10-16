//! The diarize + transcribe pipeline.
//!
//! P2 wires diarization; ASR (P3) fills in transcription.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::audio;
use crate::diarize::{self, CLUSTER_THRESHOLD};
use crate::event::{Event, SpeakerStat, Stats, Turn};
use crate::models;

const MERGE_GAP_S: f32 = 1.0;
const MIN_TURN_S: f32 = 0.4;

#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub inputs: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub preset: String,
    pub num_speakers: Option<i32>,
    pub timestamps: bool,
    pub join_speaker: bool,
    pub cache: bool,
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

/// Run the pipeline over all inputs, emitting progress events.
pub fn run(opts: &PipelineOptions, emit: &mut dyn FnMut(Event)) {
    let start = Instant::now();
    let total = opts.inputs.len().max(1);

    let segmentation = models::segmentation_model();
    let embedding = models::embedding_model();

    for (index, input) in opts.inputs.iter().enumerate() {
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
        let raw = match diarize::diarize(
            &samples,
            &segmentation,
            &embedding,
            opts.num_speakers,
            CLUSTER_THRESHOLD,
        ) {
            Ok(turns) => turns,
            Err(message) => {
                emit(Event::Error { message });
                continue;
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

        // TODO(P3): per-turn ASR.
        emit(Event::Stage {
            stage: "done".to_string(),
            label: "Done".to_string(),
            progress: (index + 1) as f32 / total as f32,
        });

        let mut speaker_stats: HashMap<String, SpeakerStat> = HashMap::new();
        for turn in &turns {
            let entry = speaker_stats.entry(turn.speaker.clone()).or_default();
            entry.seconds += (turn.end - turn.start).max(0.0);
            entry.segments += 1;
        }

        emit(Event::Done {
            txt: String::new(),
            stats: Stats {
                audio_seconds: duration,
                run_seconds: start.elapsed().as_secs_f64(),
                speakers: map.len(),
                num_speakers: opts.num_speakers,
                preset: opts.preset.clone(),
                speaker_stats,
            },
        });
    }

    emit(Event::AllDone);
}
