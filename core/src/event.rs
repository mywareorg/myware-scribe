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
