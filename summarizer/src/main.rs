//! `myware-summarize`: the briefing worker.
//!
//! Runs in its own process because whisper.cpp and llama.cpp each vendor ggml;
//! one binary linking both invites a symbol clash. The GUI writes a request,
//! spawns this program, reads progress lines from stdout, and kills it to
//! cancel — the same shape as the transcription worker.
//!
//! Protocol: one JSON object per line on stdout.
//! `{"event":"stage","label":"...","fraction":0.4}`
//! `{"event":"part","briefing":{...}}` after each transcribed chunk
//! `{"event":"done"}` or `{"event":"error","message":"..."}`
//!
//! The result is written to the response file, not to stdout, so progress and
//! the payload cannot be confused.

use std::io::Write;
use std::num::NonZeroU32;
use std::path::PathBuf;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use myware_briefing::{
    chunk_lines, merge, messages_for, overview_from_parts, parse_reply, Briefing, BriefingLine
};

/// Context size. Chunks are sized for a large prompt; the rest is reply room.
const CTX: u32 = 8192;
/// Tokens one answer may take. The prompt caps the item counts, so this is
/// enough to close the JSON object plus the overview; a larger ceiling mostly
/// buys longer pauses and more near-duplicates.
const MAX_NEW: usize = 600;

fn main() {
    if let Err(message) = run() {
        event(serde_json::json!({ "event": "error", "message": message }));
        std::process::exit(1);
    }
    event(serde_json::json!({ "event": "done" }));
}

#[derive(serde::Deserialize)]
struct Request {
    lines: Vec<BriefingLine>,
    /// The model file the worker should load.
    model: String,
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut request_path: Option<PathBuf> = None;
    let mut response_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--request" => request_path = args.next().map(PathBuf::from),
            "--response" => response_path = args.next().map(PathBuf::from),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let request_path = request_path.ok_or("missing --request")?;
    let response_path = response_path.ok_or("missing --response")?;

    let request: Request = serde_json::from_str(
        &std::fs::read_to_string(&request_path)
            .map_err(|error| format!("could not read the request: {error}"))?,
    )
    .map_err(|error| format!("invalid request: {error}"))?;
    if request.lines.is_empty() {
        return Err("the transcript is empty".to_string());
    }

    let model_path = PathBuf::from(&request.model);
    if !model_path.is_file() {
        return Err(format!(
            "the briefing model is not installed ({})",
            model_path.display()
        ));
    }

    event(serde_json::json!({ "event": "stage", "label": "Loading the briefing model", "fraction": 0.0 }));
    let backend = LlamaBackend::init().map_err(|error| format!("llama backend: {error}"))?;
    // Offload only when the caller asks and the build has a GPU backend.
    // A CPU-only llama.cpp ignores the request, so the knob is safe by default.
    let gpu_layers = std::env::var("MYWARE_BRIEFING_GPU_LAYERS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    if gpu_layers > 0 {
        eprintln!("offloading {gpu_layers} layers to the GPU");
    }
    let params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
    let model = LlamaModel::load_from_file(&backend, &model_path, &params)
        .map_err(|error| format!("could not load {}: {error}", model_path.display()))?;
    let template = model
        .chat_template(None)
        .map_err(|error| format!("the model has no chat template: {error}"))?;

    let threads = std::thread::available_parallelism()
        .map(|count| count.get() as i32)
        .unwrap_or(4);
    let context_params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(CTX))
        // The prompt is decoded in one batch. llama.cpp aborts the process when
        // a decode carries more tokens than n_batch, and a 6,500-character
        // chunk is roughly 2,700 tokens — above llama.cpp's 2,048 default.
        // Size the batch to the context so a legal chunk can never abort the
        // helper.
        .with_n_batch(CTX)
        .with_n_threads(threads)
        .with_n_threads_batch(threads);
    let mut ctx = model
        .new_context(&backend, context_params)
        .map_err(|error| format!("could not create a context: {error}"))?;

    let chunks = chunk_lines(&request.lines);
    let total = chunks.len();
    let mut parts: Vec<Briefing> = Vec::new();
    let mut last_error: Option<String> = None;

    for (index, chunk) in chunks.iter().enumerate() {
        event(serde_json::json!({
            "event": "stage",
            "label": format!("Reading part {} of {total}", index + 1),
            "fraction": 0.05 + 0.85 * (index as f32 / total as f32),
        }));

        let (system, user) = messages_for(chunk);
        // A chunk the model cannot take must not take the whole briefing down:
        // skip it, remember why, and let the other chunks still produce a
        // result. If every chunk fails, the last reason is the error the user
        // sees instead of a generic one.
        let reply = match generate(&model, &mut ctx, &template, &system, &user) {
            Ok(reply) => reply,
            Err(message) => {
                last_error = Some(message.clone());
                event(serde_json::json!({
                    "event": "stage",
                    "label": format!("Part {} was unusable: {message}", index + 1),
                    "fraction": 0.05 + 0.85 * ((index + 1) as f32 / total as f32),
                }));
                continue;
            }
        };
        let mut parsed = parse_reply(&reply, chunk);
        if parsed.is_err() {
            // One retry with the constraint restated; a small model sometimes
            // wraps the object in prose.
            let reply = match generate(
                &model,
                &mut ctx,
                &template,
                &system,
                &format!("{user}\n\nReturn only the JSON object, with no other text."),
            ) {
                Ok(reply) => reply,
                Err(message) => {
                    last_error = Some(message.clone());
                    event(serde_json::json!({
                        "event": "stage",
                        "label": format!("Part {} was unusable: {message}", index + 1),
                        "fraction": 0.05 + 0.85 * ((index + 1) as f32 / total as f32),
                    }));
                    continue;
                }
            };
            parsed = parse_reply(&reply, chunk);
        }
        match parsed {
            Ok(part) => {
                parts.push(part);
                // Show the accumulated briefing while later chunks run. The
                // final response file still carries the merged result; this
                // only stops a long transcript from looking stalled.
                event(serde_json::json!({
                    "event": "part",
                    "briefing": merge(parts.clone(), overview_from_parts(&parts)),
                }));
            }
            Err(message) => {
                last_error = Some(message.clone());
                crate::event(serde_json::json!({
                    "event": "stage",
                    "label": format!("Part {} was unusable: {message}", index + 1),
                    "fraction": 0.05 + 0.85 * ((index + 1) as f32 / total as f32),
                }));
            }
        }
    }

    if parts.is_empty() {
        return Err(last_error.unwrap_or_else(|| "the model produced no usable summary".to_string()));
    }

    // The per-chunk overviews already say what each part was about. Composing
    // the final paragraph from them removes a whole extra model pass.
    let overview = overview_from_parts(&parts);
    let briefing = merge(parts, overview);
    let text = serde_json::to_string(&briefing)
        .map_err(|error| format!("could not encode the briefing: {error}"))?;
    std::fs::write(&response_path, text)
        .map_err(|error| format!("could not write the briefing: {error}"))?;
    Ok(())
}

fn generate(
    model: &LlamaModel,
    ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
    template: &LlamaChatTemplate,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let messages = [
        LlamaChatMessage::new("system".to_string(), system.to_string())
            .map_err(|error| format!("system message: {error}"))?,
        LlamaChatMessage::new("user".to_string(), user.to_string())
            .map_err(|error| format!("user message: {error}"))?,
    ];
    let prompt = model
        .apply_chat_template(template, &messages, true)
        .map_err(|error| format!("chat template: {error}"))?;
    let tokens = model.vocab().tokenize(prompt.as_bytes(), true, true);
    if std::env::var_os("MYWARE_BRIEFING_DEBUG").is_some() {
        eprintln!("MYWARE prompt tokens: {}", tokens.len());
    }
    // Reserve the reply as well as the prompt: a chunk that only fits because
    // the check ignored MAX_NEW would overflow context mid-answer.
    if tokens.len() + MAX_NEW + 16 >= CTX as usize {
        return Err(format!(
            "a transcript chunk is too long for the briefing model ({} tokens)",
            tokens.len()
        ));
    }

    ctx.clear_kv_cache();
    let mut batch = LlamaBatch::new(tokens.len().max(64), 1);
    for (index, token) in tokens.iter().enumerate() {
        batch
            .add(
                *token,
                index as i32,
                &[0],
                index + 1 == tokens.len(),
            )
            .map_err(|error| format!("batch: {error}"))?;
    }
    ctx.decode(&mut batch)
        .map_err(|error| format!("decode: {error}"))?;

    let mut sampler = LlamaSampler::greedy();
    let mut output: Vec<u8> = Vec::new();
    let mut position = tokens.len() as i32;
    let mut last = batch.n_tokens() - 1;
    for _ in 0..MAX_NEW {
        let token = sampler.sample(ctx, last);
        sampler.accept(token);
        if model.vocab().is_eog(token) {
            break;
        }
        output.extend(model.vocab().token_to_piece(token, false, None));
        batch.clear();
        batch
            .add(token, position, &[0], true)
            .map_err(|error| format!("batch: {error}"))?;
        ctx.decode(&mut batch)
            .map_err(|error| format!("decode: {error}"))?;
        position += 1;
        last = 0;
    }
    let text = String::from_utf8_lossy(&output).into_owned();
    if std::env::var_os("MYWARE_BRIEFING_DEBUG").is_some() {
        eprintln!("--- prompt ---\n{prompt}\n--- reply ---\n{text}");
    }
    Ok(text)
}

fn event(value: serde_json::Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}
