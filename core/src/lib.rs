//! transiel-core: audio, diarization, ASR and the pipeline (no GUI).

pub mod audio;
pub mod cli;
pub mod event;
pub mod models;
pub mod pipeline;

pub use event::{Event, SpeakerStat, Stats, Turn};
