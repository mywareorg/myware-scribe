//! CPU-vs-GPU calibration for the selected Whisper preset.
//!
//! The benchmark is deliberately small and self-contained: it decodes the
//! bundled `sample-short.mp3`, keeps the first twelve seconds, and runs the
//! same ASR call on both backends. It never touches the user's queue or the
//! normal transcription progress stream.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::{asr, audio, models};

/// Seconds of speech used for calibration. Long enough to dominate model
/// startup, short enough to keep the modal responsive.
const CLIP_SECONDS: f32 = 12.0;

#[derive(Debug, Clone)]
pub enum BenchmarkEvent {
    CpuStarted,
    CpuFinished { seconds: f64 },
    GpuStarted,
    GpuFinished { seconds: f64 },
    GpuFailed { message: String },
    /// CPU setup or transcription failed; calibration cannot continue.
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub cpu_seconds: f64,
    pub gpu_seconds: Option<f64>,
    pub gpu_error: Option<String>,
    pub backend: Option<asr::Backend>,
    pub sample_seconds: f32,
}

impl BenchmarkResult {
    /// CPU time divided by GPU time. Values above 1 mean the GPU is faster.
    pub fn multiplier(&self) -> Option<f32> {
        self.gpu_seconds
            .filter(|seconds| *seconds > 0.0)
            .map(|seconds| (self.cpu_seconds / seconds) as f32)
    }
}

/// Find the bundled calibration clip. Release builds stage it beside the
/// executable; development builds keep it under `audio/`.
pub fn sample_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("sample-short.mp3"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("sample-short.mp3"));
        candidates.push(cwd.join("audio").join("sample-short.mp3"));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("audio")
            .join("sample-short.mp3"),
    );
    candidates.into_iter().find(|path| path.is_file())
}

/// Run CPU first, then GPU. GPU failures are returned inside the result so
/// the UI can show the reason and continue on the CPU.
pub fn run<F>(preset: &str, sample: &Path, notify: &mut F) -> Result<BenchmarkResult, String>
where
    F: FnMut(BenchmarkEvent),
{
    let (samples, rate) = audio::decode(sample)?;
    let wanted = (rate as f32 * CLIP_SECONDS) as usize;
    let clip = &samples[..samples.len().min(wanted)];
    if clip.len() < (rate as usize).saturating_mul(2) {
        return Err("The calibration sample is too short".to_string());
    }

    let model = models::whisper_model(preset);
    let fast = preset.eq_ignore_ascii_case("speed");
    let best_of = if fast { 1 } else { 2 };

    notify(BenchmarkEvent::CpuStarted);
    let cpu = asr::Transcriber::load_cpu(&model)?;
    let start = Instant::now();
    cpu.transcribe_with_threads_and_language(
        clip,
        cpu.threads(),
        Some("en"),
        fast,
        best_of,
    )?;
    let cpu_seconds = start.elapsed().as_secs_f64();
    notify(BenchmarkEvent::CpuFinished {
        seconds: cpu_seconds,
    });

    let gpu = match asr::Transcriber::load_gpu(&model) {
        Ok(gpu) => gpu,
        Err(message) => {
            notify(BenchmarkEvent::GpuFailed {
                message: message.clone(),
            });
            return Ok(BenchmarkResult {
                cpu_seconds,
                gpu_seconds: None,
                gpu_error: Some(message),
                backend: None,
                sample_seconds: clip.len() as f32 / rate as f32,
            });
        }
    };

    notify(BenchmarkEvent::GpuStarted);
    let start = Instant::now();
    if let Err(message) = gpu.transcribe_with_threads_and_language(
        clip,
        gpu.threads(),
        Some("en"),
        fast,
        best_of,
    ) {
        notify(BenchmarkEvent::GpuFailed {
            message: message.clone(),
        });
        return Ok(BenchmarkResult {
            cpu_seconds,
            gpu_seconds: None,
            gpu_error: Some(message),
            backend: Some(gpu.backend()),
            sample_seconds: clip.len() as f32 / rate as f32,
        });
    }
    let gpu_seconds = start.elapsed().as_secs_f64();
    notify(BenchmarkEvent::GpuFinished {
        seconds: gpu_seconds,
    });
    Ok(BenchmarkResult {
        cpu_seconds,
        gpu_seconds: Some(gpu_seconds),
        gpu_error: None,
        backend: Some(gpu.backend()),
        sample_seconds: clip.len() as f32 / rate as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplier_is_cpu_time_over_gpu_time() {
        let result = BenchmarkResult {
            cpu_seconds: 4.0,
            gpu_seconds: Some(2.0),
            gpu_error: None,
            backend: Some(asr::Backend::Vulkan),
            sample_seconds: 12.0,
        };
        assert_eq!(result.multiplier(), Some(2.0));

        let failed = BenchmarkResult {
            gpu_seconds: None,
            gpu_error: Some("no device".to_string()),
            ..result
        };
        assert_eq!(failed.multiplier(), None);
    }
}
