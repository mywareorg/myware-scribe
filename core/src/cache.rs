//! Diarization result cache keyed by audio content + parameters.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::event::Turn;

pub fn cache_dir() -> PathBuf {
    if let Some(dir) = crate::env_compat("HIPPO_TRANSCRIBE_CACHE_DIR", "TRANSIEL_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            let base = PathBuf::from(base);
            let root = crate::adopt_dir(base.join("hippo-transcribe"), base.join("transiel"));
            return root.join("cache").join("diarization");
        }
    } else if let Ok(home) = std::env::var("HOME") {
        let cache = PathBuf::from(home).join(".cache");
        let root = crate::adopt_dir(cache.join("hippo-transcribe"), cache.join("transiel"));
        return root.join("cache").join("diarization");
    }
    PathBuf::from("cache")
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// SHA-256 of the audio bytes, computed once per file and reused by every
/// cache key that needs it.
pub fn audio_hash(audio: &Path) -> Option<String> {
    let mut file = std::fs::File::open(audio).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex(&hasher.finalize()))
}

/// Cache key from the audio hash and the diarization parameters.
pub fn key_from_hash(hash: &str, diarizer: &str, num_speakers: Option<i32>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hash.as_bytes());
    hasher.update(diarizer.as_bytes());
    hasher.update(num_speakers.unwrap_or(-1).to_string().as_bytes());
    let digest = hasher.finalize();
    format!(
        "{}-{}-{}",
        hex(&digest[..8]),
        diarizer,
        num_speakers.unwrap_or(-1)
    )
}

/// Cache key from the audio bytes and the diarization parameters.
pub fn key(audio: &Path, diarizer: &str, num_speakers: Option<i32>) -> Option<String> {
    audio_hash(audio).map(|hash| key_from_hash(&hash, diarizer, num_speakers))
}

/// Per-clip ASR cache: same audio bytes, model, language and decode settings
/// produce the same segments, so a re-run can skip whisper entirely.
pub fn clip_key(
    audio_hash: &str,
    model: &str,
    language: Option<&str>,
    from: usize,
    to: usize,
    best_of: i32,
    fast: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(audio_hash.as_bytes());
    hasher.update(model.as_bytes());
    hasher.update(language.unwrap_or("auto").as_bytes());
    hasher.update(from.to_string().as_bytes());
    hasher.update(to.to_string().as_bytes());
    hasher.update(best_of.to_string().as_bytes());
    hasher.update([fast as u8]);
    // Token timestamps were added to cached segments; old entries lack them.
    hasher.update(b"tokens-v1");
    hex(&hasher.finalize()[..12])
}

pub fn read_segments(key: &str) -> Option<Vec<crate::asr::Segment>> {
    let path = cache_dir().join(format!("asr-{key}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_segments(key: &str, segments: &[crate::asr::Segment]) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string(segments) {
        let _ = std::fs::write(dir.join(format!("asr-{key}.json")), text);
    }
}

pub fn read(key: &str) -> Option<Vec<Turn>> {
    let path = cache_dir().join(format!("{key}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write(key: &str, turns: &[Turn]) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string(turns) {
        let _ = std::fs::write(dir.join(format!("{key}.json")), text);
    }
}
