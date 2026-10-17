//! transiel-core: audio, diarization, ASR and the pipeline (no GUI).

pub mod asr;
pub mod audio;
pub mod cache;
pub mod cli;
pub mod download;
pub mod diarize;
pub mod event;
pub mod models;
pub mod pipeline;

pub use event::{Event, SpeakerStat, Stats, Turn};
