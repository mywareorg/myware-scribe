//! Model location resolution (models are provisioned by the deploy script;
//! the app never downloads).

use std::path::PathBuf;

/// Root directory holding diarization/ASR models.
pub fn models_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TRANSIEL_MODELS_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(base).join("transiel").join("models");
        }
    } else if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("transiel")
            .join("models");
    }
    PathBuf::from("models")
}

pub fn segmentation_model() -> PathBuf {
    if let Ok(path) = std::env::var("TRANSIEL_SEGMENTATION_MODEL") {
        return PathBuf::from(path);
    }
    models_dir()
        .join("sherpa-onnx-pyannote-segmentation-3-0")
        .join("model.onnx")
}

pub fn embedding_model() -> PathBuf {
    if let Ok(path) = std::env::var("TRANSIEL_EMBEDDING_MODEL") {
        return PathBuf::from(path);
    }
    models_dir().join("wespeaker_en_voxceleb_resnet34_LM.onnx")
}

pub fn whisper_model(preset: &str) -> PathBuf {
    if let Ok(path) = std::env::var("TRANSIEL_WHISPER_MODEL") {
        return PathBuf::from(path);
    }
    let name = match preset {
        "speed" => "ggml-base.en-q5_1.bin",
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

/// The models a preset needs, as (label, path).
pub fn expected(preset: &str) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("segmentation", segmentation_model()),
        ("embedding", embedding_model()),
        ("whisper", whisper_model(preset)),
    ]
}

/// Missing required model files for a preset (empty when everything is present).
pub fn missing(preset: &str) -> Vec<PathBuf> {
    expected(preset)
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| !path.is_file())
        .collect()
}
