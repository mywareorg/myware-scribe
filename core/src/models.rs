//! Model location resolution. Models are provisioned by the deploy script, or
//! downloaded on demand from the GUI.

use std::path::PathBuf;

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

/// Root directory holding diarization/ASR models.
pub fn models_dir() -> PathBuf {
    if let Some(dir) = crate::env_compat("HIPPO_TRANSCRIBE_MODELS_DIR", "TRANSIEL_MODELS_DIR") {
        return PathBuf::from(dir);
    }
    root_dir().join("models")
}

pub fn segmentation_model() -> PathBuf {
    if let Some(path) = crate::env_compat(
        "HIPPO_TRANSCRIBE_SEGMENTATION_MODEL",
        "TRANSIEL_SEGMENTATION_MODEL",
    ) {
        return PathBuf::from(path);
    }
    models_dir()
        .join("sherpa-onnx-pyannote-segmentation-3-0")
        .join("model.onnx")
}

pub fn embedding_model() -> PathBuf {
    if let Some(path) = crate::env_compat(
        "HIPPO_TRANSCRIBE_EMBEDDING_MODEL",
        "TRANSIEL_EMBEDDING_MODEL",
    ) {
        return PathBuf::from(path);
    }
    models_dir().join("wespeaker_en_voxceleb_resnet34_LM.onnx")
}

pub fn whisper_model(preset: &str) -> PathBuf {
    if let Some(path) =
        crate::env_compat("HIPPO_TRANSCRIBE_WHISPER_MODEL", "TRANSIEL_WHISPER_MODEL")
    {
        return PathBuf::from(path);
    }
    let name = match preset {
        "speed" => "ggml-tiny.en-q5_1.bin",
        "balanced" => "ggml-base.en-q5_1.bin",
        _ => "ggml-large-v3-turbo-q5_0.bin",
    };
    models_dir().join(name)
}

/// Identifier for the diarization model pair (used as the cache salt).
pub fn diarizer_id() -> String {
    let seg = segmentation_model();
    let emb = embedding_model();
    let name = |p: &std::path::Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("model")
            .to_string()
    };
    format!("{}-{}", name(&seg), name(&emb))
}
