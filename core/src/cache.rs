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

/// Cache key from the audio bytes and the diarization parameters.
pub fn key(audio: &Path, diarizer: &str, num_speakers: Option<i32>) -> Option<String> {
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
    hasher.update(diarizer.as_bytes());
    hasher.update(num_speakers.unwrap_or(-1).to_string().as_bytes());
    let digest = hasher.finalize();
    Some(format!(
        "{}-{}-{}",
        hex(&digest[..8]),
        diarizer,
        num_speakers.unwrap_or(-1)
    ))
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
