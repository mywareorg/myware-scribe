//! Model downloads (all non-gated, direct files).
//!
//! This is the only code path that touches the network. The GUI/pipeline never
//! download at runtime; `transiel --cli models download` (also used by
//! `deploy.bat`) fetches everything once.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::models;

struct ModelFile {
    label: &'static str,
    path: PathBuf,
    url: &'static str,
}

fn files() -> Vec<ModelFile> {
    let dir = models::models_dir();
    vec![
        ModelFile {
            label: "segmentation",
            path: models::segmentation_model(),
            url: "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/main/model.onnx",
        },
        ModelFile {
            label: "embedding",
            path: models::embedding_model(),
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx",
        },
        ModelFile {
            label: "whisper-speed",
            path: dir.join("ggml-base.en-q5_1.bin"),
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en-q5_1.bin",
        },
        ModelFile {
            label: "whisper-accuracy",
            path: dir.join("ggml-large-v3-turbo-q5_0.bin"),
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
        },
    ]
}

fn size(path: &std::path::Path) -> Option<u64> {
    fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len())
}

/// Download every model. Existing non-empty files are skipped unless `force`.
pub fn download(force: bool, verify: bool) -> i32 {
    for model in files() {
        if !force && size(&model.path).is_some_and(|n| n > 0) {
            println!("[ok] {:16} {}", model.label, model.path.display());
            continue;
        }
        if let Err(error) = fetch(&model) {
            eprintln!("[!] {}: {error}", model.label);
            return 1;
        }
        println!("[ok] {:16} {}", model.label, model.path.display());
    }
    if verify {
        verify_all()
    } else {
        0
    }
}

/// Report which models are present (exit 1 if anything is missing/empty).
pub fn verify_all() -> i32 {
    let mut missing = 0;
    for model in files() {
        match size(&model.path) {
            Some(n) if n > 0 => println!(
                "[ok] {:16} {} ({:.1} MB)",
                model.label,
                model.path.display(),
                n as f64 / (1024.0 * 1024.0)
            ),
            _ => {
                println!("[missing] {:16} {}", model.label, model.path.display());
                missing += 1;
            }
        }
    }
    if missing > 0 {
        eprintln!(
            "\n{missing} model file(s) missing under {}",
            models::models_dir().display()
        );
        1
    } else {
        0
    }
}

fn fetch(model: &ModelFile) -> Result<(), String> {
    if let Some(parent) = model.path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let name = model
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model");
    let part = model.path.with_file_name(format!("{name}.part"));

    println!("Downloading {} ...", model.label);
    let response = ureq::get(model.url)
        .call()
        .map_err(|e| format!("request: {e}"))?;
    let total = response
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());

    let mut reader = response.into_reader();
    let mut file = File::create(&part).map_err(|e| format!("create {}: {e}", part.display()))?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let read = reader.read(&mut buffer).map_err(|e| format!("read: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("write: {e}"))?;
        done += read as u64;
        progress(model.label, done, total);
    }
    file.flush().map_err(|e| format!("flush: {e}"))?;
    drop(file);
    eprintln!();

    fs::rename(&part, &model.path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

fn progress(label: &str, done: u64, total: Option<u64>) {
    let mb = |n: u64| n as f64 / (1024.0 * 1024.0);
    match total {
        Some(total) if total > 0 => {
            let percent = (done as f64 / total as f64 * 100.0).min(100.0);
            eprint!(
                "\r  {label}: {:.1}/{:.1} MB ({percent:.0}%)",
                mb(done),
                mb(total)
            );
        }
        _ => eprint!("\r  {label}: {:.1} MB", mb(done)),
    }
    let _ = std::io::stderr().flush();
}
