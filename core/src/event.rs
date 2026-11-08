use std::collections::HashMap;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Turn {
    pub start: f32,
    pub end: f32,
    pub speaker: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SpeakerStat {
    pub seconds: f32,
    pub segments: usize,
    pub words: usize,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Stats {
    pub audio_seconds: f64,
    pub run_seconds: f64,
    pub speakers: usize,
    pub num_speakers: Option<i32>,
    pub preset: String,
    pub speaker_stats: HashMap<String, SpeakerStat>,
    /// Resolved model identity that actually produced this output.
    #[serde(default)]
    pub model: Option<String>,
    /// Effective decode language: requested when given, otherwise detected.
    #[serde(default)]
    pub language: Option<String>,
    /// Language the caller asked for; `None` means auto-detect.
    #[serde(default)]
    pub language_requested: Option<String>,
    /// Language the pipeline pinned by detection; `None` when one was requested.
    #[serde(default)]
    pub language_detected: Option<String>,
    /// Whether quiet audio was lifted before ASR and diarization.
    #[serde(default)]
    pub normalize: bool,
    /// Producer versions, e.g. `core 1.0.0 / asr-v1`.
    #[serde(default)]
    pub producer: Option<String>,
    /// Stable id for this output, used by the version manifest.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Wall-clock seconds spent decoding audio for this file.
    #[serde(default)]
    pub decode_seconds: f64,
    /// Wall-clock seconds spent diarizing this file.
    #[serde(default)]
    pub diarize_seconds: f64,
    /// Wall-clock seconds spent in ASR for this file.
    #[serde(default)]
    pub asr_seconds: f64,
    /// Wall-clock seconds spent after ASR — attribution and writing.
    #[serde(default)]
    pub prepare_seconds: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    #[serde(rename = "stage")]
    Stage {
        stage: String,
        label: String,
        progress: f32,
    },
    #[serde(rename = "file")]
    File { audio: String },
    #[serde(rename = "meta")]
    Meta { audio: String, duration: f32 },
    #[serde(rename = "turns")]
    Turns { turns: Vec<Turn> },
    #[serde(rename = "waveform")]
    Waveform { audio: String, peaks: Vec<f32> },
    #[serde(rename = "line")]
    Line {
        start: f32,
        speaker: String,
        text: String,
        confidence: f32,
    },
    #[serde(rename = "done")]
    Done { txt: String, stats: Stats },
    /// The ASR backend actually selected for this run.
    #[serde(rename = "backend")]
    Backend {
        backend: String,
        device: Option<String>,
        fallback: Option<String>,
    },
    /// Audio seconds transcribed so far, and the total for the current file.
    #[serde(rename = "work")]
    Work { done: f64, total: f64 },
    /// Fraction of the current file's diarization completed.
    #[serde(rename = "diarize")]
    Diarize { fraction: f32 },
    #[serde(rename = "log")]
    Log { message: String },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "all_done")]
    AllDone,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_stats_payloads_gain_empty_provenance() {
        let stats: Stats = serde_json::from_str(r#"{"preset":"speed","speakers":2}"#)
            .expect("old stats must still parse");
        assert_eq!(stats.preset, "speed");
        assert!(stats.model.is_none());
        assert!(stats.language.is_none());
        assert!(stats.language_requested.is_none());
        assert!(stats.language_detected.is_none());
        assert!(stats.run_id.is_none());
        assert!(!stats.normalize);
    }

    #[test]
    fn provenance_survives_a_round_trip() {
        let stats = Stats {
            preset: "accuracy".to_string(),
            model: Some("ggml-large-v3-turbo-q5_0.bin".to_string()),
            language: Some("en".to_string()),
            language_requested: Some("en".to_string()),
            language_detected: None,
            normalize: true,
            producer: Some("core 1.0.0 / asr-v1".to_string()),
            run_id: Some("run-1".to_string()),
            ..Stats::default()
        };
        let text = serde_json::to_string(&stats).unwrap();
        let back: Stats = serde_json::from_str(&text).unwrap();
        assert_eq!(back.model, stats.model);
        assert_eq!(back.language, stats.language);
        assert_eq!(back.run_id, stats.run_id);
        assert!(back.normalize);
    }
}
