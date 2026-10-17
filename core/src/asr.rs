//! Speech recognition via whisper.cpp (whisper-rs, GGML models).

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[derive(Debug, Clone)]
pub struct Segment {
    pub start: f32,
    pub end: f32,
    pub text: String,
    pub confidence: f32,
}

pub struct Transcriber {
    ctx: WhisperContext,
    threads: i32,
}

impl Transcriber {
    pub fn load(model: &Path) -> Result<Self, String> {
        if !model.is_file() {
            return Err(format!("whisper model not found: {}", model.display()));
        }
        // Route whisper.cpp's logging through the (no-op by default) hook so it
        // stops dumping token traces to stderr.
        whisper_rs::install_logging_hooks();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(false);
        let ctx = WhisperContext::new_with_params(model, params)
            .map_err(|e| format!("load whisper {}: {e}", model.display()))?;
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4);
        Ok(Self { ctx, threads })
    }

    /// Transcribe 16 kHz mono samples using the default thread count.
    pub fn transcribe(&self, samples: &[f32]) -> Result<Vec<Segment>, String> {
        self.transcribe_with_threads(samples, self.threads)
    }

    /// Transcribe 16 kHz mono samples, returning one entry per Whisper segment.
    ///
    /// Each call creates its own decode state, so one `Transcriber` (and model)
    /// can be shared across threads.
    pub fn transcribe_with_threads(
        &self,
        samples: &[f32],
        threads: i32,
    ) -> Result<Vec<Segment>, String> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| format!("whisper state: {e}"))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_translate(false);
        params.set_no_context(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_n_threads(threads);

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
