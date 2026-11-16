//! Diarization result cache keyed by audio content + parameters.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::event::Turn;

pub fn cache_dir() -> PathBuf {
    if let Some(dir) = crate::env_compat("MYWARE_SCRIBE_CACHE_DIR", "TRANSIEL_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            let base = PathBuf::from(base);
            let root = crate::adopt_dir(base.join("myware-scribe"), base.join("transiel"));
            return root.join("cache").join("diarization");
        }
    } else if let Ok(home) = std::env::var("HOME") {
        let cache = PathBuf::from(home).join(".cache");
        let root = crate::adopt_dir(cache.join("myware-scribe"), cache.join("transiel"));
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
pub fn samples_hash(samples: &[f32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((samples.len() as u64).to_le_bytes());
    let mut buffer = Vec::with_capacity(4096 * 4);
    for chunk in samples.chunks(4096) {
        buffer.clear();
        for sample in chunk {
            buffer.extend_from_slice(&sample.to_le_bytes());
        }
        hasher.update(&buffer);
    }
    hex(&hasher.finalize())
}

/// SHA-256 of the audio bytes, computed once per file and reused by every
/// cache key that needs it. Prefer [`samples_hash`] when the file has already
/// been decoded: it avoids reading the file a second time.
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

/// Version of the diarization output the cache is allowed to serve.
///
/// Bump this whenever a change alters the turns produced from the same audio
/// and the same model pair: turn geometry, cleanup thresholds, blockwise
/// clustering. [`crate::models::diarizer_id`] only changes when the model
/// files do, so without this the app would keep serving turns produced by a
/// pipeline that no longer exists. The eval loop is unaffected — it calls
/// [`crate::diarize::diarize`] directly and never touches this cache.
pub const DIARIZER_VERSION: &str = "diar-v1";

/// Cache key from the audio hash and the diarization parameters.
pub fn key_from_hash(hash: &str, diarizer: &str, num_speakers: Option<i32>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hash.as_bytes());
    hasher.update(diarizer.as_bytes());
    hasher.update(num_speakers.unwrap_or(-1).to_string().as_bytes());
    hasher.update(DIARIZER_VERSION.as_bytes());
    let digest = hasher.finalize();
    format!(
        "{}-{}-{}-{}",
        hex(&digest[..8]),
        diarizer,
        num_speakers.unwrap_or(-1),
        DIARIZER_VERSION
    )
}

/// Cache key from the audio bytes and the diarization parameters.
pub fn key(audio: &Path, diarizer: &str, num_speakers: Option<i32>) -> Option<String> {
    audio_hash(audio).map(|hash| key_from_hash(&hash, diarizer, num_speakers))
}

/// Per-clip ASR cache: same audio bytes, model, language and decode settings
/// produce the same segments, so a re-run can skip whisper entirely.
#[allow(clippy::too_many_arguments)]
pub fn clip_key(
    audio_hash: &str,
    model: &str,
    language: Option<&str>,
    from: usize,
    to: usize,
    best_of: i32,
    fast: bool,
    token_timestamps: bool,
    prompt: Option<&str>,
    preprocess: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(audio_hash.as_bytes());
    hasher.update(model.as_bytes());
    hasher.update(language.unwrap_or("auto").as_bytes());
    hasher.update(from.to_string().as_bytes());
    hasher.update(to.to_string().as_bytes());
    hasher.update(best_of.to_string().as_bytes());
    hasher.update([fast as u8]);
    hasher.update([token_timestamps as u8]);
    hasher.update(preprocess.as_bytes());
    // Only a non-empty prompt changes the decode, so a run without one keeps
    // hitting the clips cached before the vocabulary existed.
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        hasher.update(b"prompt");
        hasher.update(prompt.as_bytes());
    }
    // v3: token timestamps are optional per span, and the overlap schedule
    // caches fixed windows rather than per-turn clips.
    hasher.update(b"tokens-v3");
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

/// Version of the waveform sidecar. Bump when the bucket count or the RMS
/// computation changes, so a cached strip is never drawn from stale maths.
pub const WAVEFORM_VERSION: &str = "wave-v1";

/// Cache key for one audio file's timeline peaks.
pub fn waveform_key(hash: &str, buckets: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hash.as_bytes());
    hasher.update(buckets.to_string().as_bytes());
    hasher.update(WAVEFORM_VERSION.as_bytes());
    format!("wave-{}", hex(&hasher.finalize()[..8]))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WaveformRecord {
    duration: f64,
    peaks: Vec<f32>,
}

/// Read the timeline peaks cached for `audio`, if the file is unchanged.
pub fn read_waveform(audio: &Path, buckets: usize) -> Option<(f64, Vec<f32>)> {
    let key = waveform_key(&audio_hash(audio)?, buckets);
    let text = std::fs::read_to_string(cache_dir().join(format!("{key}.json"))).ok()?;
    let record: WaveformRecord = serde_json::from_str(&text).ok()?;
    Some((record.duration, record.peaks))
}

/// Store the timeline peaks for `audio`. The pipeline already has the peaks in
/// hand after diarization, so this costs a hash and a write instead of the full
/// decode a reopen would otherwise repeat.
pub fn write_waveform(audio: &Path, buckets: usize, duration: f64, peaks: &[f32]) {
    let Some(hash) = audio_hash(audio) else {
        return;
    };
    let key = waveform_key(&hash, buckets);
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let record = WaveformRecord {
        duration,
        peaks: peaks.to_vec(),
    };
    if let Ok(text) = serde_json::to_string(&record) {
        let _ = std::fs::write(dir.join(format!("{key}.json")), text);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_hash_is_stable_and_content_sensitive() {
        let a = vec![0.0f32, 0.5, -0.5, 1.0];
        let b = vec![0.0f32, 0.5, -0.5, 1.0];
        assert_eq!(samples_hash(&a), samples_hash(&b));

        // A single changed sample, and a length change, both change the hash.
        let mut changed = a.clone();
        changed[2] = -0.4;
        assert_ne!(samples_hash(&a), samples_hash(&changed));
        assert_ne!(samples_hash(&a), samples_hash(&a[..3]));
    }

    #[test]
    fn diarization_key_carries_the_pipeline_version() {
        let key = key_from_hash("abc", "model-x", Some(2));
        assert!(key.ends_with(DIARIZER_VERSION));
        assert_ne!(key, key_from_hash("abd", "model-x", Some(2)));
        assert_ne!(key, key_from_hash("abc", "model-x", None));
    }

    #[test]
    fn preprocessing_changes_the_clip_key() {
        let base = |preprocess| {
            clip_key(
                "hash", "model", Some("en"), 0, 10, 1, false, false, None, preprocess,
            )
        };
        assert_ne!(base("norm-v1"), base("raw-v1"));
        assert_eq!(base("norm-v1"), base("norm-v1"));
    }

}
