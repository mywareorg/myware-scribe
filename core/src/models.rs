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
    models_dir()
        .join("sherpa-onnx-pyannote-segmentation-3-0")
        .join("model.onnx")
}

pub fn embedding_model() -> PathBuf {
    models_dir().join("wespeaker_en_voxceleb_resnet34_LM.onnx")
}

pub fn whisper_model(preset: &str) -> PathBuf {
    let name = match preset {
        "speed" => "ggml-base.en.bin",
        _ => "ggml-large-v3-turbo-q5_0.bin",
    };
    models_dir().join(name)
}

/// Missing required model files for a preset (empty when everything is present).
pub fn missing(preset: &str) -> Vec<PathBuf> {
    [segmentation_model(), embedding_model(), whisper_model(preset)]
        .into_iter()
        .filter(|path| !path.is_file())
        .collect()
}
