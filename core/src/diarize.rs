//! Speaker diarization via sherpa-onnx (pyannote segmentation + speaker embedding).
//!
//! Uses the raw sherpa-onnx C API directly (rather than the high-level
//! `sherpa-rs` diarize helper, which hardcodes `num_threads = 1`) so
//! diarization runs multi-threaded, and wires the progress callback so the UI
//! can show live progress.

use std::ffi::{c_void, CString};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[cfg(windows)]
use crate::sherpa_dynamic as ffi;
#[cfg(not(windows))]
use sherpa_rs_sys as ffi;

use crate::event::Turn;

/// Clustering threshold used when the speaker count is auto-detected.
pub const CLUSTER_THRESHOLD: f32 = 0.5;

/// Opaque session handle. The Windows dynamic binding and the crate binding
/// disagree only in pointer type, so normalise it here.
#[cfg(windows)]
type SessionPtr = *const c_void;
#[cfg(not(windows))]
type SessionPtr = *const ffi::SherpaOnnxOfflineSpeakerDiarization;

/// Progress is stored as a fraction scaled by this constant.
const PROGRESS_SCALE: f32 = 1_000_000.0;

/// Verify that the diarization backend is available. On Windows this loads
/// the shipped sherpa-onnx DLL and resolves its symbols.
pub fn ensure_available() -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::sherpa_dynamic::ensure_loaded()?;
    }
    Ok(())
}

/// Threads used by the segmentation and embedding models. Defaults to every
/// physical core; `HIPPO_TRANSCRIBE_DIARIZE_THREADS` (or the legacy
/// `TRANSIEL_DIARIZE_THREADS`) overrides it, which matters once diarization
/// shares the machine with ASR workers.
fn diarize_threads() -> i32 {
    crate::env_compat(
        "HIPPO_TRANSCRIBE_DIARIZE_THREADS",
        "TRANSIEL_DIARIZE_THREADS",
    )
    .and_then(|value| value.parse::<usize>().ok())
    .filter(|threads| *threads > 0)
    .unwrap_or_else(|| num_cpus::get_physical().max(1)) as i32
}

/// A diarization session that can be reused across files.
///
/// Creating the sherpa-onnx session loads the pyannote segmentation model and
/// the speaker embedding model, so rebuilding it per file costs tens of
/// megabytes of initialisation. The clustering settings are baked into the
/// session, so a change in `num_speakers` or `threshold` requires a new one.
pub struct Diarizer {
    sd: SessionPtr,
    // The C strings must outlive the config pointers the session was built
    // from; sherpa keeps them for the session's lifetime.
    _embedding: CString,
    _segmentation: CString,
    _provider: CString,
    num_speakers: Option<i32>,
    threshold: f32,
    threads: i32,
}

// The session is only ever driven from one thread at a time.
unsafe impl Send for Diarizer {}
unsafe impl Sync for Diarizer {}

impl Drop for Diarizer {
    fn drop(&mut self) {
        unsafe { ffi::SherpaOnnxDestroyOfflineSpeakerDiarization(self.sd) };
    }
}

impl Diarizer {
    /// Build a session, or return `None` when the requested clustering matches
    /// `existing` so the caller can keep using it.
    pub fn compatible(&self, num_speakers: Option<i32>, threshold: f32) -> bool {
        self.num_speakers == num_speakers && (self.threshold - threshold).abs() < f32::EPSILON
    }

    pub fn new(
        segmentation: &Path,
        embedding: &Path,
        num_speakers: Option<i32>,
        threshold: f32,
    ) -> Result<Self, String> {
        Self::new_with_threads(segmentation, embedding, num_speakers, threshold, None)
    }

    /// Build a session that uses `threads` for both models. `None` keeps the
    /// default (all physical cores, or the environment override).
    pub fn new_with_threads(
        segmentation: &Path,
        embedding: &Path,
        num_speakers: Option<i32>,
        threshold: f32,
        threads: Option<i32>,
    ) -> Result<Self, String> {
        #[cfg(windows)]
        ffi::ensure_loaded()?;

        let threads = threads
            .filter(|threads| *threads > 0)
            .unwrap_or_else(diarize_threads);
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

        let sd: SessionPtr = unsafe { ffi::SherpaOnnxCreateOfflineSpeakerDiarization(&config) };
        if sd.is_null() {
            return Err("failed to initialize offline speaker diarization".to_string());
        }

        Ok(Self {
            sd,
            _embedding: embedding,
            _segmentation: segmentation,
            _provider: provider,
            num_speakers,
            threshold,
            threads,
        })
    }

    /// Diarize 16 kHz mono samples, reporting progress (0..1 scaled).
    pub fn run_with_progress(
        &self,
        samples: &[f32],
        progress: Option<Arc<AtomicU32>>,
    ) -> Result<Vec<Turn>, String> {
        let callback: Option<unsafe extern "C" fn(i32, i32, *mut c_void) -> i32> = progress
            .as_ref()
            .map(|_| diarize_progress as unsafe extern "C" fn(i32, i32, *mut c_void) -> i32);
        let arg = match &progress {
            Some(progress) => Arc::as_ptr(progress) as *mut c_void,
            None => std::ptr::null_mut(),
        };

        let mut turns = Vec::new();
        unsafe {
            let result = ffi::SherpaOnnxOfflineSpeakerDiarizationProcessWithCallback(
                self.sd,
                samples.as_ptr() as *mut f32,
                samples.len() as i32,
                callback,
                arg,
            );

            if !result.is_null() {
                let count = ffi::SherpaOnnxOfflineSpeakerDiarizationResultGetNumSegments(result);
                let segments =
                    ffi::SherpaOnnxOfflineSpeakerDiarizationResultSortByStartTime(result);
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
        }

        turns.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(turns)
    }

    /// Threads this session was configured with.
    pub fn threads(&self) -> i32 {
        self.threads
    }
}

/// Threads a new diarization session would use, for the CLI's timing line.
pub fn threads() -> i32 {
    diarize_threads()
}

/// Diarize 16 kHz mono samples. `num_speakers` fixes the cluster count; `None`
/// lets the clustering decide using `threshold`.
pub fn diarize(
    samples: &[f32],
    segmentation: &Path,
    embedding: &Path,
    num_speakers: Option<i32>,
    threshold: f32,
) -> Result<Vec<Turn>, String> {
    Diarizer::new(segmentation, embedding, num_speakers, threshold)?
        .run_with_progress(samples, None)
}

unsafe extern "C" fn diarize_progress(processed: i32, total: i32, arg: *mut c_void) -> i32 {
    if !arg.is_null() && total > 0 {
        let progress = &*(arg as *const AtomicU32);
        let fraction = (processed.max(0) as f32 / total as f32).clamp(0.0, 1.0);
        progress.store((fraction * PROGRESS_SCALE) as u32, Ordering::Relaxed);
    }
    0
}
