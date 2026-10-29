//! hippo-transcribe-core: audio, diarization, ASR and the pipeline (no GUI).

pub mod asr;
pub mod audio;
pub mod cache;
pub mod cli;
pub mod diarize;
pub mod download;
pub mod event;
pub mod export;
pub mod license;
pub mod logging;
pub mod models;
pub mod pipeline;

#[cfg(windows)]
mod sherpa_dynamic;

pub use event::{Event, SpeakerStat, Stats, Turn};

/// Read an environment variable, preferring the current name over the legacy
/// one so existing installations keep working after the rename.
pub fn env_compat(current: &str, legacy: &str) -> Option<String> {
    std::env::var(current)
        .ok()
        .or_else(|| std::env::var(legacy).ok())
}

/// Adopt a directory left behind under the legacy name.
///
/// Returns `new` when it already exists. Otherwise tries a single rename of
/// `legacy` to `new` (instant on the same volume); if that fails, keeps using
/// the legacy directory so data such as downloaded models is never lost.
pub fn adopt_dir(new: std::path::PathBuf, legacy: std::path::PathBuf) -> std::path::PathBuf {
    if new.exists() || !legacy.exists() {
        return new;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(&legacy, &new).is_ok() {
        new
    } else {
        legacy
    }
}
