//! Per-audio version manifest.
//!
//! One JSON document per audio file, kept in the directory that holds its
//! transcripts. It records every run that produced a transcript, which one is
//! current, and the settings and provenance of each. `queue.json` is an index
//! over these documents, not the source of truth: deleting the index can be
//! recovered by scanning the manifests.
//!
//! Paths inside the document are relative to the manifest's directory, so a
//! folder of audio, transcripts and manifests can be moved as a unit.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Shape of the document. Additive changes keep older readers working.
pub const SCHEMA_VERSION: u32 = 1;

/// Salt mixed into the settings key. Bump when the meaning of a setting
/// changes, so an old key cannot silently match a new run.
const SETTINGS_VERSION: &str = "settings-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    pub asset: Asset,
    /// Version id the UI opens by default.
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub versions: Vec<Version>,
    /// Top-level keys written by a newer build; preserved verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    /// File name of the audio, not an absolute path.
    pub audio: String,
    #[serde(default)]
    pub audio_hash: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub modified: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// Stable id for this settings combination; kept when the same settings
    /// are run again.
    pub id: String,
    pub settings_key: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub preset: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub language_requested: Option<String>,
    #[serde(default)]
    pub language_detected: Option<String>,
    #[serde(default)]
    pub speakers_requested: Option<i32>,
    #[serde(default)]
    pub speakers_detected: Option<usize>,
    #[serde(default)]
    pub normalize: bool,
    #[serde(default)]
    pub producer: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    /// Timestamped transcript, relative to the manifest directory.
    pub transcript: String,
    /// Plain transcript, relative to the manifest directory.
    pub plain: String,
    #[serde(default)]
    pub sidecar: Option<String>,
    #[serde(default)]
    pub edited: bool,
    /// Pre-edit copy, relative to the manifest directory.
    #[serde(default)]
    pub original: Option<String>,
    /// `run`, `imported` or `legacy`.
    #[serde(default = "origin_run")]
    pub origin: String,
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

fn origin_run() -> String {
    "run".to_string()
}

impl Manifest {
    /// A fresh document for `audio`, stored at `path`.
    pub fn new(_path: &Path, audio: &Path) -> Self {
        let meta = std::fs::metadata(audio).ok();
        Self {
            schema_version: SCHEMA_VERSION,
            asset: Asset {
                id: uuid::Uuid::new_v4().to_string(),
                audio: audio
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("audio")
                    .to_string(),
                audio_hash: None,
                size: meta.as_ref().map(|meta| meta.len()),
                modified: meta.as_ref().and_then(modified_secs),
            },
            current: None,
            versions: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// Insert or replace the version for its settings key, then make it
    /// current. Returns the version id.
    pub fn upsert(&mut self, mut version: Version) -> String {
        if let Some(existing) = self
            .versions
            .iter_mut()
            .find(|existing| existing.settings_key == version.settings_key)
        {
            // Re-running the same settings replaces the output in place and
            // keeps the identity the index may already point at.
            version.id = existing.id.clone();
            *existing = version.clone();
        } else {
            self.versions.push(version.clone());
        }
        self.current = Some(version.id.clone());
        version.id
    }

    pub fn current_version(&self) -> Option<&Version> {
        let current = self.current.as_deref()?;
        self.versions.iter().find(|version| version.id == current)
    }

    /// Resolve the current version's timestamped transcript, if it still
    /// exists on disk.
    pub fn current_transcript(&self, directory: &Path) -> Option<PathBuf> {
        let version = self.current_version()?;
        let path = directory.join(&version.transcript);
        path.is_file().then_some(path)
    }
}

/// Hash of the settings that determine a transcript. Requested values, not
/// detected ones: auto must stay auto across re-runs.
pub fn settings_key(
    preset: &str,
    model: &str,
    language_requested: Option<&str>,
    speakers_requested: Option<i32>,
    normalize: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SETTINGS_VERSION.as_bytes());
    hasher.update(preset.as_bytes());
    hasher.update(model.as_bytes());
    hasher.update(language_requested.unwrap_or("auto").as_bytes());
    hasher.update(speakers_requested.unwrap_or(-1).to_string().as_bytes());
    hasher.update([normalize as u8]);
    hex(&hasher.finalize()[..8])
}

/// Where the manifest for `audio` lives: beside its transcripts, named after
/// the audio and a short content hash so same-stem files cannot collide.
pub fn manifest_path(audio: &Path, output_dir: Option<&Path>, audio_hash: Option<&str>) -> PathBuf {
    let directory = output_dir
        .map(Path::to_path_buf)
        .or_else(|| audio.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let stem = audio
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("audio");
    let suffix = audio_hash
        .map(|hash| &hash[..hash.len().min(8)])
        .unwrap_or("nohash");
    directory.join(format!("{stem}-{suffix}.hippo.json"))
}

/// Read a manifest. `None` when it is missing or not valid.
pub fn read(path: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read a manifest, or start one for `audio`, refreshing the asset metadata.
pub fn read_or_new(path: &Path, audio: &Path) -> Manifest {
    let mut manifest = read(path).unwrap_or_else(|| Manifest::new(path, audio));
    if let Ok(meta) = std::fs::metadata(audio) {
        manifest.asset.size = Some(meta.len());
        manifest.asset.modified = modified_secs(&meta);
    }
    manifest
}

/// Write through a temporary file in the same directory, then rename over the
/// target, so a crash cannot leave a partial manifest.
pub fn write_at(path: &Path, manifest: &Manifest) -> io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "manifest.json".to_string());
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));

    std::fs::write(&temporary, text)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Path relative to the manifest directory, or the file name when it is
/// outside it.
pub fn relative(directory: &Path, path: &Path) -> String {
    path.strip_prefix(directory)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// A free pair of transcript names. The base names are used when free; a
/// numeric suffix is added when either already exists, so a run never
/// overwrites a transcript it does not own.
pub fn free_paths(directory: &Path, plain: &Path, timestamped: &Path) -> (String, String) {
    let plain_name = plain
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript.txt".to_string());
    let timestamped_name = timestamped
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript-ts.txt".to_string());
    let mut suffix = 1usize;
    loop {
        let plain = suffixed(&plain_name, suffix);
        let timestamped = suffixed(&timestamped_name, suffix);
        if !directory.join(&plain).exists() && !directory.join(&timestamped).exists() {
            return (plain, timestamped);
        }
        suffix += 1;
    }
}

fn suffixed(name: &str, suffix: usize) -> String {
    if suffix <= 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) => format!("{stem}-{suffix}.{extension}"),
        None => format!("{name}-{suffix}"),
    }
}

/// The pre-edit copy beside a transcript. Same shape as the GUI's helper so
/// both sides agree.
pub fn unedited_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript".to_string());
    path.with_file_name(format!("{stem}-unedited.txt"))
}

/// Every manifest in `directory`, with the file it was read from. Used by the
/// explicit import and by tests.
pub fn scan_with_paths(directory: &Path) -> Vec<(PathBuf, Manifest)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !name.ends_with(".hippo.json") {
            continue;
        }
        if let Some(manifest) = read(&path) {
            manifests.push((path, manifest));
        }
    }
    manifests
}

/// Every manifest in `directory`.
pub fn scan(directory: &Path) -> Vec<Manifest> {
    scan_with_paths(directory)
        .into_iter()
        .map(|(_, manifest)| manifest)
        .collect()
}

/// Pipeline-named transcripts in `directory` that no manifest references.
/// Only files that look like pipeline output are considered, so exports and
/// user-created text are never reported. This is a maintenance report, not an
/// automatic deletion: the caller decides what to do with it.
pub fn unreferenced_transcripts(directory: &Path) -> Vec<PathBuf> {
    let manifests = scan_with_paths(directory);
    let mut referenced: Vec<PathBuf> = Vec::new();
    for (manifest_path, manifest) in &manifests {
        let dir = manifest_path.parent().unwrap_or(directory);
        for version in &manifest.versions {
            for relative in [
                Some(version.transcript.as_str()),
                Some(version.plain.as_str()),
                version.sidecar.as_deref(),
                version.original.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                let path = dir.join(relative);
                referenced.push(path.clone());
                referenced.push(unedited_path(&path));
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut orphans = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !name.ends_with(".txt") || !name.contains("-transcription-") {
            continue;
        }
        if !referenced.iter().any(|known| known == &path) {
            orphans.push(path);
        }
    }
    orphans
}

fn modified_secs(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(key: &str, transcript: &str) -> Version {
        Version {
            id: uuid::Uuid::new_v4().to_string(),
            settings_key: key.to_string(),
            run_id: Some("run".to_string()),
            preset: "accuracy".to_string(),
            model: Some("model.bin".to_string()),
            language_requested: Some("en".to_string()),
            language_detected: None,
            speakers_requested: Some(2),
            speakers_detected: Some(2),
            normalize: false,
            producer: Some("core / asr-v1".to_string()),
            created_at: 1,
            transcript: transcript.to_string(),
            plain: transcript.replace("-ts", ""),
            sidecar: None,
            edited: false,
            original: None,
            origin: "run".to_string(),
        }
    }

    #[test]
    fn the_same_settings_replace_one_version() {
        let mut manifest = Manifest::new(Path::new("/tmp/talk.mp3"), Path::new("/tmp/talk.mp3"));
        let first = manifest.upsert(version("a", "one-ts.txt"));
        let second = manifest.upsert(version("b", "two-ts.txt"));
        assert_eq!(manifest.versions.len(), 2);
        assert_eq!(manifest.current.as_deref(), Some(second.as_str()));

        // Same key: same id, replaced content, still current.
        let replacement = version("a", "three-ts.txt");
        let third = manifest.upsert(replacement);
        assert_eq!(third, first);
        assert_eq!(manifest.versions.len(), 2);
        assert_eq!(
            manifest
                .versions
                .iter()
                .find(|version| version.id == first)
                .unwrap()
                .transcript,
            "three-ts.txt"
        );
    }

    #[test]
    fn settings_keys_separate_models_and_requested_values() {
        let base = settings_key("accuracy", "large.bin", Some("en"), Some(2), false);
        assert_eq!(
            base,
            settings_key("accuracy", "large.bin", Some("en"), Some(2), false)
        );
        assert_ne!(
            base,
            settings_key("accuracy", "small.bin", Some("en"), Some(2), false)
        );
        assert_ne!(
            base,
            settings_key("accuracy", "large.bin", None, Some(2), false)
        );
        assert_ne!(
            base,
            settings_key("accuracy", "large.bin", Some("en"), Some(3), false)
        );
    }

    #[test]
    fn free_names_do_not_overwrite_an_existing_pair() {
        let dir = std::env::temp_dir().join(format!("hippo-versions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("talk-accuracy-transcription-2speakers.txt");
        let ts = dir.join("talk-accuracy-transcription-2speakers-ts.txt");
        assert_eq!(
            free_paths(&dir, &plain, &ts),
            (
                "talk-accuracy-transcription-2speakers.txt".to_string(),
                "talk-accuracy-transcription-2speakers-ts.txt".to_string()
            )
        );

        std::fs::write(&plain, "one").unwrap();
        let (plain, ts) = free_paths(&dir, &plain, &ts);
        assert_eq!(plain, "talk-accuracy-transcription-2speakers-2.txt");
        assert_eq!(ts, "talk-accuracy-transcription-2speakers-ts-2.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_finds_every_manifest_in_a_directory() {
        let dir = std::env::temp_dir().join(format!("hippo-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let path = manifest_path(&audio, None, Some("abcdef123456"));
        let mut manifest = Manifest::new(&path, &audio);
        manifest.upsert(version("a", "talk-ts.txt"));
        write_at(&path, &manifest).unwrap();
        // An unrelated JSON beside it must not be mistaken for a manifest.
        std::fs::write(dir.join("notes.json"), "{}").unwrap();

        let found = scan(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].versions.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanning_and_reserializing_round_trips() {
        let dir = std::env::temp_dir().join(format!("hippo-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, hash) in [("one.mp3", "aaaaaaaa"), ("two.mp3", "bbbbbbbb")] {
            let audio = dir.join(name);
            std::fs::write(&audio, b"audio").unwrap();
            let path = manifest_path(&audio, None, Some(hash));
            let mut manifest = Manifest::new(&path, &audio);
            manifest.upsert(version(name, &format!("{name}-ts.txt")));
            write_at(&path, &manifest).unwrap();
        }

        let scanned = scan(&dir);
        assert_eq!(scanned.len(), 2);
        let values: Vec<serde_json::Value> = scanned
            .iter()
            .map(|manifest| serde_json::to_value(manifest).unwrap())
            .collect();
        let reparsed: Vec<Manifest> = values
            .iter()
            .map(|value| serde_json::from_value(value.clone()).unwrap())
            .collect();
        assert_eq!(values.len(), reparsed.len());
        for (before, after) in scanned.iter().zip(reparsed.iter()) {
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                serde_json::to_value(after).unwrap()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_orphan_report_ignores_referenced_and_unrelated_files() {
        let dir = std::env::temp_dir().join(format!("hippo-orphans-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let referenced = dir.join("talk-accuracy-transcription-2speakers.txt");
        std::fs::write(&referenced, "kept").unwrap();
        let orphan = dir.join("talk-speed-transcription-2speakers.txt");
        std::fs::write(&orphan, "orphan").unwrap();
        let user_file = dir.join("notes.txt");
        std::fs::write(&user_file, "mine").unwrap();

        let path = manifest_path(&audio, None, Some("abcdef123456"));
        let mut manifest = Manifest::new(&path, &audio);
        let mut version = version("a", "talk-accuracy-transcription-2speakers-ts.txt");
        version.plain = "talk-accuracy-transcription-2speakers.txt".to_string();
        manifest.upsert(version);
        write_at(&path, &manifest).unwrap();

        let orphans = unreferenced_transcripts(&dir);
        assert_eq!(orphans, vec![orphan]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_round_trips_and_keeps_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("hippo-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let path = manifest_path(&audio, None, Some("abcdef123456"));
        let mut manifest = Manifest::new(&path, &audio);
        let id = manifest.upsert(version("a", "talk-ts.txt"));
        manifest
            .extra
            .insert("future".to_string(), serde_json::json!({"ok": true}));
        write_at(&path, &manifest).unwrap();

        let read_back = read(&path).expect("manifest");
        assert_eq!(read_back.current.as_deref(), Some(id.as_str()));
        assert_eq!(read_back.versions.len(), 1);
        assert_eq!(read_back.extra["future"]["ok"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
