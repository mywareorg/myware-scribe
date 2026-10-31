//! Speech recognition via whisper.cpp (whisper-rs, GGML models).

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    pub start: f32,
    pub end: f32,
    pub text: String,
    pub confidence: f32,
}

/// Compute backend whisper.cpp was built with, or is using at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Cpu,
    Metal,
    CoreMl,
    Vulkan,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Cpu => "cpu",
            Backend::Metal => "metal",
            Backend::CoreMl => "coreml",
            Backend::Vulkan => "vulkan",
        }
    }
}

/// Backend compiled into this build. Metal is enabled for every macOS build;
/// Vulkan only when the `gpu` feature is on (Windows/Linux release builds).
pub fn preferred_backend() -> Backend {
    if cfg!(target_os = "macos") {
        if cfg!(feature = "coreml") {
            Backend::CoreMl
        } else {
            Backend::Metal
        }
    } else if cfg!(feature = "gpu") {
        Backend::Vulkan
    } else {
        Backend::Cpu
    }
}

/// `HIPPO_TRANSCRIBE_ACCEL=cpu` forces the CPU path for troubleshooting.
fn force_cpu() -> bool {
    crate::env_compat("HIPPO_TRANSCRIBE_ACCEL", "TRANSIEL_ACCEL")
        .is_some_and(|value| value.eq_ignore_ascii_case("cpu"))
}

fn context_params(use_gpu: bool) -> WhisperContextParameters<'static> {
    WhisperContextParameters {
        use_gpu,
        // Flash attention pairs with the GPU backends; keep the CPU path on
        // the conservative default.
        flash_attn: use_gpu,
        ..WhisperContextParameters::default()
    }
}

pub struct Transcriber {
    ctx: WhisperContext,
    threads: i32,
    backend: Backend,
    fallback: Option<String>,
}

impl Transcriber {
    pub fn load(model: &Path) -> Result<Self, String> {
        if !model.is_file() {
            return Err(format!("whisper model not found: {}", model.display()));
        }
        // Route whisper.cpp's logging through the (no-op by default) hook so it
        // stops dumping token traces to stderr.
        whisper_rs::install_logging_hooks();
        let preferred = preferred_backend();
        let try_gpu = preferred != Backend::Cpu && !force_cpu();
        let (ctx, backend, fallback) =
            match WhisperContext::new_with_params(model, context_params(try_gpu)) {
                Ok(ctx) => (ctx, if try_gpu { preferred } else { Backend::Cpu }, None),
                Err(first) if try_gpu => {
                    // A GPU/driver failure must never stop a transcription: retry
                    // on the CPU and report why in the console.
                    let ctx = WhisperContext::new_with_params(model, context_params(false))
                        .map_err(|error| {
                            format!(
                                "load whisper {}: {error} ({} attempt: {first})",
                                model.display(),
                                preferred.label()
                            )
                        })?;
                    (ctx, Backend::Cpu, Some(first.to_string()))
                }
                Err(error) => return Err(format!("load whisper {}: {error}", model.display())),
            };
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4);
        Ok(Self {
            ctx,
            threads,
            backend,
            fallback,
        })
    }

    /// Backend actually in use for this context.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Why the GPU path failed, when it did.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback.as_deref()
    }

    /// Transcribe 16 kHz mono samples using the default thread count.
    pub fn transcribe(&self, samples: &[f32]) -> Result<Vec<Segment>, String> {
        self.transcribe_with_threads(samples, self.threads)
    }

    /// Transcribe with the default thread count and an explicit language.
    ///
    /// `None` leaves Whisper's language auto-detection enabled.
    pub fn transcribe_with_language(
        &self,
        samples: &[f32],
        language: Option<&str>,
    ) -> Result<Vec<Segment>, String> {
        self.transcribe_with_threads_and_language(samples, self.threads, language, false, 1)
    }

    /// Transcribe 16 kHz mono samples, returning one entry per Whisper segment.
    ///
    /// Keeps the historical English default for callers that do not specify a
    /// language.
    pub fn transcribe_with_threads(
        &self,
        samples: &[f32],
        threads: i32,
    ) -> Result<Vec<Segment>, String> {
        self.transcribe_with_threads_and_language(samples, threads, Some("en"), false, 1)
    }

    /// Transcribe 16 kHz mono samples with a specific thread count and language.
    ///
    /// Each call creates its own decode state, so one `Transcriber` (and model)
    /// can be shared across threads. `fast` disables the temperature fallback
    /// and suppresses non-speech tokens for the speed preset.
    pub fn transcribe_with_threads_and_language(
        &self,
        samples: &[f32],
        threads: i32,
        language: Option<&str>,
        fast: bool,
        best_of: i32,
    ) -> Result<Vec<Segment>, String> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| format!("whisper state: {e}"))?;

        // `best_of` trades decode time for quality: 1 for speed, more for the
        // higher tiers.
        let mut params = FullParams::new(SamplingStrategy::Greedy {
            best_of: best_of.max(1),
        });
        params.set_language(language);
        params.set_translate(false);
        params.set_no_context(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_n_threads(threads);
        if fast {
            // The speed preset trades the temperature fallback for latency.
            params.set_temperature_inc(0.0);
            params.set_suppress_nst(true);
        } else {
            // Cap fallback retries: 0.4 increments stop at 0.8 instead of
            // walking all the way to 1.0 on noisy windows.
            params.set_temperature_inc(0.4);
        }

        state
            .full(params, samples)
            .map_err(|e| format!("transcribe: {e}"))?;

        let count = state.full_n_segments();
        let mut segments = Vec::new();
        for index in 0..count {
            let Some(segment) = state.get_segment(index) else {
                continue;
            };
            let text = segment
                .to_str_lossy()
                .map_err(|e| e.to_string())?
                .trim()
                .to_string();
            if text.is_empty() {
                continue;
            }

            let mut log_sum = 0.0f64;
            let mut tokens = 0usize;
            for token_index in 0..segment.n_tokens() {
                if let Some(token) = segment.get_token(token_index) {
                    let probability = token.token_probability() as f64;
                    if probability > 0.0 {
                        log_sum += probability.ln();
                        tokens += 1;
                    }
                }
            }
            let confidence = if tokens > 0 {
                (log_sum / tokens as f64) as f32
            } else {
                0.0
            };

            segments.push(Segment {
                start: segment.start_timestamp() as f32 / 100.0,
                end: segment.end_timestamp() as f32 / 100.0,
                text,
                confidence,
            });
        }
        Ok(segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_backend_matches_the_compiled_features() {
        #[cfg(target_os = "macos")]
        {
            if cfg!(feature = "coreml") {
                assert_eq!(preferred_backend(), Backend::CoreMl);
            } else {
                assert_eq!(preferred_backend(), Backend::Metal);
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            if cfg!(feature = "gpu") {
                assert_eq!(preferred_backend(), Backend::Vulkan);
            } else {
                assert_eq!(preferred_backend(), Backend::Cpu);
            }
        }
    }

    #[test]
    fn gpu_context_enables_gpu_and_flash_attention() {
        let gpu = context_params(true);
        assert!(gpu.use_gpu);
        assert!(gpu.flash_attn);

        let cpu = context_params(false);
        assert!(!cpu.use_gpu);
        assert!(!cpu.flash_attn);
    }

    #[test]
    fn backend_labels_are_stable() {
        assert_eq!(Backend::Cpu.label(), "cpu");
        assert_eq!(Backend::Metal.label(), "metal");
        assert_eq!(Backend::CoreMl.label(), "coreml");
        assert_eq!(Backend::Vulkan.label(), "vulkan");
    }
}
