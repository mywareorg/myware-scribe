//! The diarize + transcribe pipeline.
//!
//! P1 wires the structure and event flow; diarization (P2) and ASR (P3) fill in
//! the actual inference.

use std::path::PathBuf;
use std::time::Instant;

use crate::audio;
use crate::event::{Event, Stats};

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

/// Run the pipeline over all inputs, emitting progress events.
pub fn run(opts: &PipelineOptions, emit: &mut dyn FnMut(Event)) {
    let start = Instant::now();
    let total = opts.inputs.len().max(1);

    for (index, input) in opts.inputs.iter().enumerate() {
        emit(Event::File {
            audio: input.display().to_string(),
        });
        emit(Event::Stage {
            stage: "loading".to_string(),
            label: "Loading".to_string(),
            progress: index as f32 / total as f32,
        });

        let duration = audio::probe(input, 1).map(|(d, _)| d).unwrap_or(0.0);

        emit(Event::Meta {
            audio: input.display().to_string(),
            duration: duration as f32,
        });
        emit(Event::Turns { turns: Vec::new() });

        // TODO(P2): diarization.  TODO(P3): per-turn ASR.
        emit(Event::Stage {
            stage: "done".to_string(),
            label: "Done".to_string(),
            progress: (index + 1) as f32 / total as f32,
        });

        emit(Event::Done {
            txt: String::new(),
            stats: Stats {
                audio_seconds: duration,
                run_seconds: start.elapsed().as_secs_f64(),
                preset: opts.preset.clone(),
                num_speakers: opts.num_speakers,
                ..Default::default()
            },
        });
    }

    emit(Event::AllDone);
}
