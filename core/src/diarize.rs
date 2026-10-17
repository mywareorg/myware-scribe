//! Speaker diarization via sherpa-onnx (pyannote segmentation + speaker embedding).
//!
//! Uses the raw sherpa-onnx C API directly (rather than `sherpa_rs::diarize`,
//! which hardcodes `num_threads = 1`) so diarization runs multi-threaded.

use std::ffi::CString;
use std::path::Path;

use sherpa_rs::sherpa_rs_sys as ffi;

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
    let threads = num_cpus::get_physical().max(1) as i32;

    let embedding = CString::new(embedding.to_string_lossy().as_bytes())
        .map_err(|e| format!("embedding path: {e}"))?;
    let segmentation = CString::new(segmentation.to_string_lossy().as_bytes())
        .map_err(|e| format!("segmentation path: {e}"))?;
    let provider = CString::new("cpu").unwrap();

    let config = ffi::SherpaOnnxOfflineSpeakerDiarizationConfig {
        embedding: ffi::SherpaOnnxSpeakerEmbeddingExtractorConfig {
            model: embedding.as_ptr(),
            num_threads: threads,
            debug: 0,
            provider: provider.as_ptr(),
        },
        clustering: ffi::SherpaOnnxFastClusteringConfig {
            num_clusters: num_speakers.unwrap_or(-1),
            threshold,
        },
        min_duration_off: 0.0,
        min_duration_on: 0.0,
        segmentation: ffi::SherpaOnnxOfflineSpeakerSegmentationModelConfig {
            pyannote: ffi::SherpaOnnxOfflineSpeakerSegmentationPyannoteModelConfig {
                model: segmentation.as_ptr(),
            },
            num_threads: threads,
            debug: 0,
            provider: provider.as_ptr(),
        },
    };

    let mut samples = samples.to_vec();
    let mut turns = Vec::new();

    unsafe {
        let sd = ffi::SherpaOnnxCreateOfflineSpeakerDiarization(&config);
        if sd.is_null() {
            return Err("failed to initialize offline speaker diarization".to_string());
        }

        let result = ffi::SherpaOnnxOfflineSpeakerDiarizationProcess(
            sd,
            samples.as_mut_ptr(),
            samples.len() as i32,
        );

        if !result.is_null() {
            let count = ffi::SherpaOnnxOfflineSpeakerDiarizationResultGetNumSegments(result);
            let segments = ffi::SherpaOnnxOfflineSpeakerDiarizationResultSortByStartTime(result);
            if !segments.is_null() && count > 0 {
                for segment in std::slice::from_raw_parts(segments, count as usize) {
                    turns.push(Turn {
                        start: segment.start,
                        end: segment.end,
                        speaker: format!("spk{}", segment.speaker),
                    });
                }
            }
            ffi::SherpaOnnxOfflineSpeakerDiarizationDestroySegment(segments);
            ffi::SherpaOnnxOfflineSpeakerDiarizationDestroyResult(result);
        }
        ffi::SherpaOnnxDestroyOfflineSpeakerDiarization(sd);
    }

    turns.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(turns)
}
