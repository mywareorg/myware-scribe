//! Speaker diarization via sherpa-onnx (pyannote segmentation + speaker embedding).

use std::path::Path;

use sherpa_rs::diarize::{Diarize, DiarizeConfig};

use crate::event::Turn;

/// Clustering threshold used when the speaker count is auto-detected.
pub const CLUSTER_THRESHOLD: f32 = 0.5;

/// Diarize 16 kHz mono samples. `num_speakers` fixes the cluster count; `None`
/// lets the clustering decide using `threshold`.
pub fn diarize(
    samples: &[f32],
    segmentation: &Path,
    embedding: &Path,
    num_speakers: Option<i32>,
    threshold: f32,
) -> Result<Vec<Turn>, String> {
    let config = DiarizeConfig {
        num_clusters: Some(num_speakers.unwrap_or(-1)),
        threshold: Some(threshold),
        min_duration_on: Some(0.0),
        min_duration_off: Some(0.0),
        provider: None,
        debug: false,
    };

    let mut engine = Diarize::new(segmentation, embedding, config)
        .map_err(|e| format!("diarizer init: {e}"))?;
    let segments = engine
        .compute(samples.to_vec(), None)
        .map_err(|e| format!("diarize: {e}"))?;

    let mut turns: Vec<Turn> = segments
        .iter()
        .map(|s| Turn {
            start: s.start,
            end: s.end,
            speaker: format!("spk{}", s.speaker),
        })
        .collect();
    turns.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(turns)
}
