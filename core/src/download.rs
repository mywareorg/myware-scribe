//! Model downloads (all non-gated, direct files).
//!
//! This is the only code path that touches the network. The same implementation
//! backs `transiel --cli models download` (also used by `deploy.bat`) and the
//! GUI's first-run download card.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::models;

struct ModelFile {
    label: &'static str,
    path: PathBuf,
    url: &'static str,
    /// Exact byte length of the upstream file.
    size: u64,
    /// SHA-256 of the upstream file, lowercase hex.
    sha256: &'static str,
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
            sha256: "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
        },
        ModelFile {
            label: "embedding",
            path: models::embedding_model(),
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx",
            size: 26_530_550,
            sha256: "e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012",
        },
        ModelFile {
            label: "whisper-speed",
            path: dir.join("ggml-base.en-q5_1.bin"),
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en-q5_1.bin",
            size: 59_721_011,
            sha256: "4baf70dd0d7c4247ba2b81fafd9c01005ac77c2f9ef064e00dcf195d0e2fdd2f",
        },
        ModelFile {
            label: "whisper-accuracy",
            path: dir.join("ggml-large-v3-turbo-q5_0.bin"),
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
            size: 574_041_195,
            sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        },
    ]
}

fn size(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
}

fn part_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("model");
    path.with_file_name(format!("{name}.part"))
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

/// Download every model. Existing complete files are skipped unless `force`
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
    // The GUI's download path never cancels today; callers that need
    // cancellation use `download_with_cancel`.
    let cancel = Arc::new(AtomicBool::new(false));
    download_with_cancel(force, verify, cancel, notify)
}

/// Download every model, stopping promptly when `cancel` becomes true.
///
/// A partially downloaded file is left behind as `<name>.part` so a later
/// call resumes with an HTTP `Range` request.
pub fn download_with_cancel<F>(
    force: bool,
    verify: bool,
    cancel: Arc<AtomicBool>,
    notify: &mut F,
) -> i32
where
    F: FnMut(DownloadEvent),
{
    for model in files() {
        if cancel.load(Ordering::SeqCst) {
            notify(DownloadEvent::Finished { ok: false });
            return 1;
        }
        // Only a file with the expected size is considered complete. This
        // also means a truncated file left by an older run gets repaired.
        if !force && size(&model.path) == Some(model.size) {
            notify(DownloadEvent::Completed {
                label: model.label,
                path: model.path,
            });
            continue;
        }
        notify(DownloadEvent::Started { label: model.label });
        if let Err(error) = fetch(&model, &cancel, &mut |done, total| {
            notify(DownloadEvent::Progress {
                label: model.label,
                done,
                total,
            })
        }) {
            let cancelled = cancel.load(Ordering::SeqCst);
            notify(DownloadEvent::Failed {
                label: model.label,
                message: if cancelled {
                    "cancelled".to_string()
                } else {
                    error
                },
            });
            notify(DownloadEvent::Finished { ok: false });
            return 1;
        }
        notify(DownloadEvent::Completed {
            label: model.label,
            path: model.path,
        });
    }
    if cancel.load(Ordering::SeqCst) {
        notify(DownloadEvent::Finished { ok: false });
        return 1;
    }
    let code = if verify { verify_all() } else { 0 };
    notify(DownloadEvent::Finished { ok: code == 0 });
    code
}

/// Report which models are present and complete (exit 1 if anything is
/// missing, the wrong size, or has the wrong checksum).
pub fn verify_all() -> i32 {
    let mut missing = 0;
    let mut wrong = 0;
    for model in files() {
        match size(&model.path) {
            Some(n) if n != model.size => {
                println!(
                    "[bad] {:16} {} ({n} bytes, expected {} bytes)",
                    model.label,
                    model.path.display(),
                    model.size
                );
                wrong += 1;
            }
            Some(n) => match sha256_file(&model.path) {
                Ok(hash) if hash == model.sha256 => println!(
                    "[ok] {:16} {} ({:.1} MB)",
                    model.label,
                    model.path.display(),
                    n as f64 / (1024.0 * 1024.0)
                ),
                Ok(hash) => {
                    println!(
                        "[bad] {:16} {} (sha256 {hash}, expected {})",
                        model.label,
                        model.path.display(),
                        model.sha256
                    );
                    wrong += 1;
                }
                Err(error) => {
                    println!(
                        "[bad] {:16} {} ({error})",
                        model.label,
                        model.path.display()
                    );
                    wrong += 1;
                }
            },
            None => {
                println!("[missing] {:16} {}", model.label, model.path.display());
                missing += 1;
            }
        }
    }
    if missing + wrong > 0 {
        eprintln!(
            "\n{missing} model file(s) missing, {wrong} with the wrong size or checksum under {}",
            models::models_dir().display()
        );
        eprintln!("Run: transiel --cli models download");
        1
    } else {
        0
    }
}

fn whisper_label(preset: &str) -> &'static str {
    if preset.eq_ignore_ascii_case("speed") {
        "whisper-speed"
    } else {
        "whisper-accuracy"
    }
}

fn required_for(preset: &str) -> [&'static str; 3] {
    ["segmentation", "embedding", whisper_label(preset)]
}

/// True when every file needed to run `preset` exists at the expected size.
pub fn ready_for(preset: &str) -> bool {
    missing_for(preset).is_empty()
}

/// Every file needed by `preset` that is absent or the wrong size.
pub fn missing_for(preset: &str) -> Vec<ModelStatus> {
    let required = required_for(preset);
    files()
        .into_iter()
        .filter(|model| required.contains(&model.label))
        .filter(|model| size(&model.path) != Some(model.size))
        .map(|model| ModelStatus {
            label: model.label,
            bytes: size(&model.path).unwrap_or(0),
            path: model.path,
        })
        .collect()
}

/// Remove any `<name>.part` files left by interrupted downloads.
pub fn clear_partial() -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for model in files() {
        let part = part_path(&model.path);
        if part.is_file() && fs::remove_file(&part).is_ok() {
            removed.push(part);
        }
    }
    removed
}

fn fetch(
    model: &ModelFile,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    if let Some(parent) = model.path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let part = part_path(&model.path);
    let mut resume_from = if part.is_file() {
        size(&part).unwrap_or(0)
    } else {
        0
    };
    if resume_from > model.size {
        // A stale/oversized partial cannot be resumed; start clean.
        let _ = fs::remove_file(&part);
        resume_from = 0;
    }
    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled".to_string());
    }

    let mut request = ureq::get(model.url);
    if resume_from > 0 {
        request = request.set("Range", &format!("bytes={resume_from}-"));
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(416, _)) if resume_from > 0 => {
            // The server rejected the range. If the partial is in fact a
            // complete file (some servers do this at EOF), finish it.
            if size(&part) == Some(model.size) {
                return finalize(model, &part);
            }
            let _ = fs::remove_file(&part);
            return Err(format!("server rejected resume at byte {resume_from}"));
        }
        Err(error) => return Err(format!("request: {error}")),
    };

    let status = response.status();
    let (mut file, mut done, total) = if status == 206 {
        let total = content_range_total(&response).or_else(|| {
            response
                .header("Content-Length")
                .and_then(|value| value.parse::<u64>().ok())
                .map(|length| resume_from + length)
        });
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&part)
            .map_err(|e| format!("open {}: {e}", part.display()))?;
        (file, resume_from, total)
    } else {
        // 200 means the server ignored our Range header: restart from zero.
        let total = response
            .header("Content-Length")
            .and_then(|value| value.parse::<u64>().ok());
        let file = File::create(&part).map_err(|e| format!("create {}: {e}", part.display()))?;
        (file, 0, total)
    };

    on_progress(done, total);
    let mut reader = response.into_reader();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        let read = reader.read(&mut buffer).map_err(|e| format!("read: {e}"))?;
        if read == 0 {
            break;
        }
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("write: {e}"))?;
        done += read as u64;
        on_progress(done, total);
    }
    file.flush().map_err(|e| format!("flush: {e}"))?;
    drop(file);

    if let Some(total) = total {
        if done != total {
            return Err(format!("incomplete download: got {done} bytes of {total}"));
        }
    }
    if done != model.size {
        return Err(format!(
            "incomplete download: got {done} bytes, expected {}",
            model.size
        ));
    }
    finalize(model, &part)
}

fn content_range_total(response: &ureq::Response) -> Option<u64> {
    let value = response.header("Content-Range")?;
    let (_, total) = value.rsplit_once('/')?;
    total.trim().parse().ok()
}

fn finalize(model: &ModelFile, part: &Path) -> Result<(), String> {
    let actual = size(part).ok_or_else(|| format!("missing {}", part.display()))?;
    if actual != model.size {
        return Err(format!(
            "incomplete download: got {actual} bytes, expected {}",
            model.size
        ));
    }
    if let Err(error) = verify_hash(part, model.sha256) {
        // A bad checksum cannot be resumed meaningfully.
        let _ = fs::remove_file(part);
        return Err(error);
    }
    if model.path.exists() {
        let _ = fs::remove_file(&model.path);
    }
    fs::rename(part, &model.path).map_err(|e| format!("rename: {e}"))
}

fn verify_hash(path: &Path, expected: &str) -> Result<(), String> {
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(format!(
            "sha256 mismatch: got {actual}, expected {expected}"
        ));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| format!("read: {e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_files_match_the_preset() {
        assert_eq!(
            required_for("speed"),
            ["segmentation", "embedding", "whisper-speed"]
        );
        assert_eq!(
            required_for("accuracy"),
            ["segmentation", "embedding", "whisper-accuracy"]
        );
        // Unknown presets fall back to the accuracy model, matching
        // `models::whisper_model`.
        assert_eq!(whisper_label("balanced"), "whisper-accuracy");
    }

    #[test]
    fn sha256_of_known_content() {
        let path = std::env::temp_dir().join(format!("transiel-sha-{}", std::process::id()));
        fs::write(&path, b"abc").unwrap();
        let hash = sha256_file(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
