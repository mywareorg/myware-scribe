//! transiel-core: audio, diarization, ASR and the pipeline (no GUI).

pub mod asr;
pub mod audio;
pub mod cache;
pub mod cli;
pub mod diarize;
pub mod download;
pub mod event;
pub mod export;
pub mod models;
pub mod pipeline;

#[cfg(windows)]
mod sherpa_dynamic;

pub use event::{Event, SpeakerStat, Stats, Turn};
