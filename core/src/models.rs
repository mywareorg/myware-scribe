//! Model location resolution and the pinned model manifest.
//!
//! `models.lock.json` at the repository root is the single source of truth for
//! model file names, sizes, SHA-256 hashes, licenses and download mirrors. It is
//! compiled into the binary here and also read by `scripts/fetch-models.*`, so
//! the build-time fetch and the runtime downloader can never disagree.
//!
//! Resolution order for a model file:
//! 1. explicit environment override (`HIPPO_TRANSCRIBE_*_MODEL`),
//! 2. a complete file in the user's writable models directory,
//! 3. a `models/` directory beside the executable (bundled/read-only).
//!
//! Downloads always target the user directory; bundled files are never written.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

/// Model set version. Bumping it (or editing a hash in the manifest)
/// invalidates the diarization cache, because `diarizer_id` includes it.
pub const MANIFEST_JSON: &str = include_str!("../../models.lock.json");

/// Primary mirror for every model: the public GitLab package registry. All
/// pinned upstream URLs in the manifest are fallbacks.
pub const MIRROR_BASE: &str = "https://gitlab.com/api/v4/projects/librewarelive%2Fhippo-transcribe-models/packages/generic/hippo-models/1.0.0";

#[derive(Debug, Clone, Deserialize)]
pub struct ModelSpec {
    /// Stable id used by the downloader and by `required_for`.
    pub id: String,
    /// Path relative to a models directory, e.g. `ggml-tiny-q5_1.bin`.
    pub file: String,
    /// Exact byte length of the upstream file.
    pub size: u64,
    /// SHA-256 of the upstream file, lowercase hex.
    pub sha256: String,
    /// SPDX-ish license identifier for attribution.
    pub license: String,
    /// Human-readable provenance.
    pub source: String,
    /// Whether release packages ship this file.
    pub bundled: bool,
    /// Download mirrors in priority order.
    pub mirrors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub models: Vec<ModelSpec>,
}

const MANIFEST: &str = MANIFEST_JSON;

/// The parsed, pinned model manifest.
pub fn manifest() -> &'static Manifest {
    static MANIFEST_CACHE: OnceLock<Manifest> = OnceLock::new();
    MANIFEST_CACHE
        .get_or_init(|| serde_json::from_str(MANIFEST).expect("models.lock.json is not valid JSON"))
}

/// Manifest version string, e.g. `2`.
pub fn set_version() -> &'static str {
    &manifest().version
}

/// Look up one model by id.
pub fn spec(id: &str) -> Option<&'static ModelSpec> {
    manifest().models.iter().find(|model| model.id == id)
}

/// Every model that release packages ship.
pub fn bundled() -> impl Iterator<Item = &'static ModelSpec> {
    manifest().models.iter().filter(|model| model.bundled)
}

/// Platform data root, adopting the pre-rename directory if needed.
fn root_dir() -> PathBuf {
    if cfg!(windows) {
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            let base = PathBuf::from(base);
            return crate::adopt_dir(base.join("hippo-transcribe"), base.join("transiel"));
        }
    } else if let Ok(home) = std::env::var("HOME") {
        let cache = PathBuf::from(home).join(".cache");
        return crate::adopt_dir(cache.join("hippo-transcribe"), cache.join("transiel"));
    }
    PathBuf::from(".")
}

/// Writable root directory holding models (downloads always land here).
pub fn models_dir() -> PathBuf {
    if let Some(dir) = crate::env_compat("HIPPO_TRANSCRIBE_MODELS_DIR", "TRANSIEL_MODELS_DIR") {
        return PathBuf::from(dir);
    }
    root_dir().join("models")
}

/// Read-only models directory shipped beside the executable, if present.
pub fn bundled_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("models");
    dir.is_dir().then_some(dir)
}

/// Where a model lives: user directory if present, else the bundled copy.
///
/// The returned path may not exist; it is then the location a download will
/// write to.
pub fn resolve(relative: &str) -> PathBuf {
    let user = models_dir().join(relative);
    if user.is_file() {
        // A stale or partial file in AppData must not shadow a valid copy
        // shipped inside the MSIX. Downloads always target the user directory,
        // so a complete user copy still wins on the next launch.
        let expected = manifest()
            .models
            .iter()
            .find(|model| model.file == relative)
            .map(|model| model.size);
        let valid = std::fs::metadata(&user)
            .map(|meta| expected.is_none_or(|size| meta.len() == size))
            .unwrap_or(false);
        if valid {
            return user;
        }
    }
    if let Some(bundled) = bundled_dir() {
        let shipped = bundled.join(relative);
        if shipped.is_file() {
            return shipped;
        }
    }
    user
}

/// Short description of where `path` came from, for diagnostics and the CLI.
pub fn source_of(path: &Path) -> &'static str {
    if let Some(bundled) = bundled_dir() {
        if path.starts_with(bundled) {
            return "bundled";
        }
    }
    if path.starts_with(models_dir()) {
        return "user";
    }
    "override"
}

pub fn segmentation_model() -> PathBuf {
    if let Some(path) = crate::env_compat(
        "HIPPO_TRANSCRIBE_SEGMENTATION_MODEL",
        "TRANSIEL_SEGMENTATION_MODEL",
    ) {
        return PathBuf::from(path);
    }
    resolve(&segmentation_relative())
}

/// Embedding model id for a preset. Accuracy uses the larger ResNet152
/// speaker encoder; the other presets keep the smaller ResNet34 model.
pub fn embedding_id(preset: &str) -> &'static str {
    if preset.eq_ignore_ascii_case("accuracy") {
        "embedding-accuracy"
    } else {
        "embedding"
    }
}

pub fn embedding_model(preset: &str) -> PathBuf {
    if let Some(path) = crate::env_compat(
        "HIPPO_TRANSCRIBE_EMBEDDING_MODEL",
        "TRANSIEL_EMBEDDING_MODEL",
    ) {
        return PathBuf::from(path);
    }
    resolve(&embedding_relative(preset))
}

/// Manifest id for a CLI/GUI preset string.
pub fn whisper_id(preset: &str) -> &'static str {
    if preset.eq_ignore_ascii_case("speed") {
        "whisper-speed"
    } else if preset.eq_ignore_ascii_case("balanced") {
        "whisper-balanced"
    } else {
        "whisper-accuracy"
    }
}

/// File name of the whisper model for a preset.
pub fn whisper_relative(preset: &str) -> String {
    spec(whisper_id(preset))
        .map(|model| model.file.clone())
        .unwrap_or_else(|| "ggml-large-v3-turbo-q5_0.bin".to_string())
}

pub fn whisper_model(preset: &str) -> PathBuf {
    if let Some(path) =
        crate::env_compat("HIPPO_TRANSCRIBE_WHISPER_MODEL", "TRANSIEL_WHISPER_MODEL")
    {
        return PathBuf::from(path);
    }
    resolve(&whisper_relative(preset))
}

pub fn segmentation_relative() -> String {
    spec("segmentation")
        .map(|model| model.file.clone())
        .unwrap_or_else(|| "sherpa-onnx-pyannote-segmentation-3-0/model.onnx".to_string())
}

pub fn embedding_relative(preset: &str) -> String {
    spec(embedding_id(preset))
        .map(|model| model.file.clone())
        .unwrap_or_else(|| "wespeaker_en_voxceleb_resnet34_LM.onnx".to_string())
}

/// Identifier for the diarization model pair (used as the cache salt).
///
/// Includes the model set version so a model update cannot reuse stale turns.
pub fn diarizer_id(preset: &str) -> String {
    let seg = segmentation_model();
    let emb = embedding_model(preset);
    let name = |p: &std::path::Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("model")
            .to_string()
    };
    format!("{}-{}-v{}", name(&seg), name(&emb), set_version())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_is_internally_consistent() {
        let manifest = manifest();
        assert!(!manifest.version.is_empty());
        assert_eq!(manifest.models.len(), 6);
        for model in &manifest.models {
            assert!(!model.id.is_empty());
            assert!(!model.file.is_empty());
            assert!(model.size > 0, "{} has no size", model.id);
            assert_eq!(model.sha256.len(), 64, "{} sha256 length", model.id);
            assert!(
                model.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{} sha256 is not hex",
                model.id
            );
            assert!(!model.mirrors.is_empty(), "{} has no mirrors", model.id);
            assert!(!model.license.is_empty(), "{} has no license", model.id);
            assert!(
                !model
                    .mirrors
                    .iter()
                    .any(|url| url.contains("/resolve/main/")),
                "{} mirrors must be pinned to a revision, not main",
                model.id
            );
        }
        let mut ids: Vec<_> = manifest.models.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), manifest.models.len(), "duplicate model ids");
        let mut files: Vec<_> = manifest.models.iter().map(|m| m.file.as_str()).collect();
        files.sort_unstable();
        files.dedup();
        assert_eq!(files.len(), manifest.models.len(), "duplicate model files");
    }

    #[test]
    fn every_model_prefers_the_backed_up_mirror() {
        for model in &manifest().models {
            let primary = model.mirrors.first().expect("model has no mirrors");
            assert!(
                primary.starts_with(MIRROR_BASE),
                "{} primary mirror is not the GitLab backup: {primary}",
                model.id
            );
            let name = model.file.rsplit('/').next().unwrap();
            assert!(
                primary.ends_with(name),
                "{} primary mirror does not end with {name}: {primary}",
                model.id
            );
            assert!(
                model.mirrors.len() >= 2,
                "{} has no upstream fallback",
                model.id
            );
        }
    }

    #[test]
    fn bundled_models_fit_the_release_budget() {
        let bytes: u64 = bundled().map(|model| model.size).sum();
        // The release package ships the speed model and both diarization
        // models; balanced and accuracy download on demand.
        assert!(
            bytes < 200 * 1024 * 1024,
            "bundled models total {bytes} bytes"
        );
        assert_eq!(bundled().count(), 3);
        assert!(spec("whisper-accuracy").is_some_and(|m| !m.bundled));
    }

    #[test]
    fn presets_map_to_multilingual_models() {
        assert_eq!(whisper_id("speed"), "whisper-speed");
        assert_eq!(whisper_id("balanced"), "whisper-balanced");
        assert_eq!(whisper_id("nonsense"), "whisper-accuracy");
        assert_eq!(whisper_relative("speed"), "ggml-tiny-q5_1.bin");
        assert_eq!(whisper_relative("balanced"), "ggml-small-q5_1.bin");
        assert_eq!(whisper_relative("accuracy"), "ggml-large-v3-turbo-q5_0.bin");
        assert_eq!(embedding_id("balanced"), "embedding");
        assert_eq!(embedding_id("accuracy"), "embedding-accuracy");
        assert_eq!(
            embedding_relative("accuracy"),
            "wespeaker_en_voxceleb_CAM++_LM.onnx"
        );
    }
}
