//! Model downloads (all non-gated, direct files).
//!
//! This is the only code path that touches the network. The same implementation
//! backs `transiel --cli models download` (also used by `deploy.bat`) and the
//! GUI's first-run download card.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::models;

struct ModelFile {
    label: &'static str,
    path: PathBuf,
    url: &'static str,
    /// Exact byte length of the upstream file.
    size: u64,
}

/// Presence and on-disk size of one model file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStatus {
    pub label: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
}

impl ModelStatus {
    pub fn present(&self) -> bool {
        self.bytes > 0
    }
}

/// Progress emitted by [`download_with`]. The GUI consumes these off the UI
/// thread; the CLI prints them.
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Started {
        label: &'static str,
    },
    Progress {
        label: &'static str,
        done: u64,
        total: Option<u64>,
    },
    Completed {
        label: &'static str,
        path: PathBuf,
    },
    Failed {
        label: &'static str,
        message: String,
    },
    Finished {
        ok: bool,
    },
}

fn files() -> Vec<ModelFile> {
    let dir = models::models_dir();
    vec![
        ModelFile {
            label: "segmentation",
            path: models::segmentation_model(),
            url: "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/main/model.onnx",
            size: 5_992_913,
        },
        ModelFile {
            label: "embedding",
            path: models::embedding_model(),
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx",
            size: 26_530_550,
        },
        ModelFile {
            label: "whisper-speed",
            path: dir.join("ggml-base.en-q5_1.bin"),
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en-q5_1.bin",
            size: 59_721_011,
        },
        ModelFile {
            label: "whisper-accuracy",
            path: dir.join("ggml-large-v3-turbo-q5_0.bin"),
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
            size: 574_041_195,
        },
    ]
}

fn size(path: &std::path::Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
}

/// Presence and size of every model file, without touching the network.
pub fn status() -> Vec<ModelStatus> {
    files()
        .into_iter()
        .map(|model| ModelStatus {
            label: model.label,
            bytes: size(&model.path).unwrap_or(0),
            path: model.path,
        })
        .collect()
}

/// Download every model. Existing non-empty files are skipped unless `force`
/// is set; `verify` checks the resulting files afterwards.
pub fn download(force: bool, verify: bool) -> i32 {
    let mut downloading = false;
    download_with(force, verify, &mut |event| match event {
        DownloadEvent::Started { label } => {
            downloading = true;
            println!("Downloading {label} ...");
        }
        DownloadEvent::Progress { label, done, total } => progress(label, done, total),
        DownloadEvent::Completed { label, path } => {
            if downloading {
                eprintln!();
                downloading = false;
            }
            println!("[ok] {label:16} {}", path.display());
        }
        DownloadEvent::Failed { label, message } => {
            if downloading {
                eprintln!();
                downloading = false;
            }
            eprintln!("[!] {label}: {message}");
        }
        DownloadEvent::Finished { .. } => {}
    })
}

/// Download every model, reporting progress through `notify`.
///
/// This is safe to call from a background thread; no iced types are involved.
pub fn download_with<F>(force: bool, verify: bool, notify: &mut F) -> i32
where
    F: FnMut(DownloadEvent),
{
    for model in files() {
        if !force && size(&model.path).is_some_and(|n| n > 0) {
            notify(DownloadEvent::Completed {
                label: model.label,
                path: model.path,
            });
            continue;
        }
        notify(DownloadEvent::Started { label: model.label });
        if let Err(error) = fetch(&model, &mut |done, total| {
            notify(DownloadEvent::Progress {
                label: model.label,
                done,
                total,
            })
        }) {
            notify(DownloadEvent::Failed {
                label: model.label,
                message: error,
            });
            notify(DownloadEvent::Finished { ok: false });
            return 1;
        }
        notify(DownloadEvent::Completed {
            label: model.label,
            path: model.path,
        });
    }
    let code = if verify { verify_all() } else { 0 };
    notify(DownloadEvent::Finished { ok: code == 0 });
    code
}

/// Report which models are present and complete (exit 1 if anything is
/// missing or has the wrong size).
pub fn verify_all() -> i32 {
    let mut missing = 0;
    let mut wrong = 0;
    for model in files() {
        match size(&model.path) {
            Some(n) if n == model.size => println!(
                "[ok] {:16} {} ({:.1} MB)",
                model.label,
                model.path.display(),
                n as f64 / (1024.0 * 1024.0)
            ),
            Some(n) => {
                println!(
                    "[bad] {:16} {} ({n} bytes, expected {} bytes)",
                    model.label,
                    model.path.display(),
                    model.size
                );
                wrong += 1;
            }
            None => {
                println!("[missing] {:16} {}", model.label, model.path.display());
                missing += 1;
            }
        }
    }
    if missing + wrong > 0 {
        eprintln!(
            "\n{missing} model file(s) missing, {wrong} with the wrong size under {}",
            models::models_dir().display()
        );
        eprintln!("Run: transiel --cli models download");
        1
    } else {
        0
    }
}

fn fetch(model: &ModelFile, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), String> {
    if let Some(parent) = model.path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let name = model
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model");
    let part = model.path.with_file_name(format!("{name}.part"));

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
        on_progress(done, total);
    }
    file.flush().map_err(|e| format!("flush: {e}"))?;
    drop(file);

    if done != model.size {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "incomplete download: got {done} bytes, expected {}",
            model.size
        ));
    }
    if model.path.exists() {
        let _ = fs::remove_file(&model.path);
    }
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
