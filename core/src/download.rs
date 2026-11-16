//! Model downloads (all non-gated, direct files).
//!
//! This is the only code path that touches the network. The same implementation
//! backs `myware-scribe --cli models download` (also used by `deploy.bat`) and the
//! GUI's first-run download card.
//!
//! Files, hashes and mirrors come from `models.lock.json` (see [`models`]).
//! When a release package ships a model beside the executable, that copy is used
//! as-is and nothing is downloaded.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::models::{self, ModelSpec};

/// Environment variable holding an extra mirror base URL. Files are fetched as
/// `{base}/{file_name}` before the pinned upstream mirrors are tried.
const MIRROR_ENV: (&str, &str) = ("MYWARE_SCRIBE_MODEL_MIRROR", "TRANSIEL_MODEL_MIRROR");

struct ModelFile {
    spec: &'static ModelSpec,
}

impl ModelFile {
    fn install_path(&self) -> PathBuf {
        models::models_dir().join(&self.spec.file)
    }

    fn resolved(&self) -> PathBuf {
        models::resolve(&self.spec.file)
    }
}

/// Presence and on-disk size of one model file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStatus {
    pub label: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
    /// `bundled`, `user` or `override`; empty when the file is missing.
    pub source: &'static str,
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
    models::manifest()
        .models
        .iter()
        .map(|spec| ModelFile { spec })
        .collect()
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

fn file_name(spec: &ModelSpec) -> &str {
    spec.file.rsplit('/').next().unwrap_or(&spec.file)
}

/// Mirror URLs for one model: the configured mirror base first, then the
/// pinned upstream mirrors. Duplicates are removed.
fn mirrors(spec: &ModelSpec, base: Option<&str>) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(base) = base.map(str::trim).filter(|base| !base.is_empty()) {
        urls.push(format!(
            "{}/{}",
            base.trim_end_matches('/'),
            file_name(spec)
        ));
    }
    for url in &spec.mirrors {
        if !urls.iter().any(|existing| existing == url) {
            urls.push(url.clone());
        }
    }
    urls
}

fn mirror_base() -> Option<String> {
    crate::env_compat(MIRROR_ENV.0, MIRROR_ENV.1)
}

/// Presence and size of every model file, without touching the network.
pub fn status() -> Vec<ModelStatus> {
    files()
        .into_iter()
        .map(|model| {
            let path = model.resolved();
            let bytes = size(&path).unwrap_or(0);
            ModelStatus {
                label: model.spec.id.as_str(),
                source: if bytes > 0 {
                    models::source_of(&path)
                } else {
                    ""
                },
                bytes,
                path,
            }
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

/// Download only the models with the given ids (used to fetch just the files a
/// tier and preset actually need).
pub fn download_ids<F>(
    ids: &[&str],
    force: bool,
    verify: bool,
    cancel: Arc<AtomicBool>,
    notify: &mut F,
) -> i32
where
    F: FnMut(DownloadEvent),
{
    download_selected(Some(ids), force, verify, cancel, notify)
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
    download_selected(None, force, verify, cancel, notify)
}

fn download_selected<F>(
    ids: Option<&[&str]>,
    force: bool,
    verify: bool,
    cancel: Arc<AtomicBool>,
    notify: &mut F,
) -> i32
where
    F: FnMut(DownloadEvent),
{
    let base = mirror_base();
    for model in files() {
        if let Some(ids) = ids {
            if !ids.contains(&model.spec.id.as_str()) {
                continue;
            }
        }
        if cancel.load(Ordering::SeqCst) {
            notify(DownloadEvent::Finished { ok: false });
            return 1;
        }
        // A file of the expected size is considered complete, whether it came
        // from a previous download or from the release package. This also
        // means a truncated file left by an older run gets repaired.
        let resolved = model.resolved();
        if !force && size(&resolved) == Some(model.spec.size) {
            notify(DownloadEvent::Completed {
                label: model.spec.id.as_str(),
                path: resolved,
            });
            continue;
        }
        crate::logging::info(format!(
            "download: {} ({} bytes) from {}",
            model.spec.id,
            model.spec.size,
            base.as_deref().unwrap_or("the compiled-in mirrors")
        ));
        notify(DownloadEvent::Started {
            label: model.spec.id.as_str(),
        });
        if let Err(error) = fetch(&model, base.as_deref(), &cancel, &mut |done, total| {
            notify(DownloadEvent::Progress {
                label: model.spec.id.as_str(),
                done,
                total,
            })
        }) {
            let cancelled = cancel.load(Ordering::SeqCst);
            if !cancelled {
                crate::logging::error(format!("download failed for {}: {error}", model.spec.id));
            }
            notify(DownloadEvent::Failed {
                label: model.spec.id.as_str(),
                message: if cancelled {
                    "cancelled".to_string()
                } else {
                    error
                },
            });
            notify(DownloadEvent::Finished { ok: false });
            return 1;
        }
        crate::logging::info(format!("download finished: {}", model.spec.id));
        notify(DownloadEvent::Completed {
            label: model.spec.id.as_str(),
            path: model.install_path(),
        });
    }
    if cancel.load(Ordering::SeqCst) {
        notify(DownloadEvent::Finished { ok: false });
        return 1;
    }
    let code = if verify { verify_selected(ids) } else { 0 };
    notify(DownloadEvent::Finished { ok: code == 0 });
    code
}

/// Report which models are present and complete, where each came from, and
/// whether the bundled set is installed (exit 1 if anything is missing, the
/// wrong size, or has the wrong checksum).
pub fn verify_all() -> i32 {
    verify_selected(None)
}

fn verify_selected(ids: Option<&[&str]>) -> i32 {
    let mut missing = 0;
    let mut wrong = 0;
    println!(
        "model set v{} (manifest models.lock.json)",
        models::set_version()
    );
    if let Some(dir) = models::bundled_dir() {
        println!("bundled models: {}", dir.display());
    }
    for model in files() {
        if let Some(ids) = ids {
            if !ids.contains(&model.spec.id.as_str()) {
                continue;
            }
        }
        let path = model.resolved();
        let label = model.spec.id.as_str();
        match size(&path) {
            Some(n) if n != model.spec.size => {
                println!(
                    "[bad] {label:16} {} ({n} bytes, expected {} bytes)",
                    path.display(),
                    model.spec.size
                );
                wrong += 1;
            }
            Some(n) => match sha256_file(&path) {
                Ok(hash) if hash == model.spec.sha256 => println!(
                    "[ok] {label:16} {} ({:.1} MB, {})",
                    path.display(),
                    n as f64 / (1024.0 * 1024.0),
                    models::source_of(&path)
                ),
                Ok(hash) => {
                    println!(
                        "[bad] {label:16} {} (sha256 {hash}, expected {})",
                        path.display(),
                        model.spec.sha256
                    );
                    wrong += 1;
                }
                Err(error) => {
                    println!("[bad] {label:16} {} ({error})", path.display());
                    wrong += 1;
                }
            },
            None => {
                println!("[missing] {label:16} {}", path.display());
                missing += 1;
            }
        }
    }
    if missing + wrong > 0 {
        eprintln!(
            "\n{missing} model file(s) missing, {wrong} with the wrong size or checksum under {}",
            models::models_dir().display()
        );
        eprintln!("Run: myware-scribe --cli models download");
        1
    } else {
        0
    }
}

/// Attribution/provenance for every model, as Markdown.
pub fn notice() -> String {
    let mut out = String::from("# Model attribution\n\n");
    out.push_str(&format!(
        "Model set v{} (pinned in `models.lock.json`).\n\n",
        models::set_version()
    ));
    for model in &models::manifest().models {
        out.push_str(&format!("## {}\n\n", model.id));
        out.push_str(&format!("- File: `{}`\n", model.file));
        out.push_str(&format!("- Size: {} bytes\n", model.size));
        out.push_str(&format!("- SHA-256: `{}`\n", model.sha256));
        out.push_str(&format!("- License: {}\n", model.license));
        out.push_str(&format!("- Source: {}\n", model.source));
        out.push_str(&format!(
            "- Shipped in the release package: {}\n",
            if model.bundled { "yes" } else { "no" }
        ));
        out.push('\n');
    }
    out
}

/// Model ids required to run `preset` in the given pipeline mode.
pub fn required_for(preset: &str, diarized: bool) -> Vec<&'static str> {
    let mut ids = Vec::with_capacity(3);
    if diarized {
        ids.push("segmentation");
        ids.push("embedding");
    }
    ids.push(models::whisper_id(preset));
    ids
}

/// True when the briefing model is installed at the pinned size.
pub fn briefing_ready() -> bool {
    models::spec("briefing").is_some_and(|spec| {
        let path = models::resolve(&spec.file);
        std::fs::metadata(&path)
            .map(|meta| meta.len() == spec.size)
            .unwrap_or(false)
    })
}

/// True when every file needed to run `preset` exists at the expected size.
pub fn ready_for(preset: &str, diarized: bool) -> bool {
    missing_for(preset, diarized).is_empty()
}

/// Every file needed by `preset` that is absent or the wrong size.
pub fn missing_for(preset: &str, diarized: bool) -> Vec<ModelStatus> {
    let required = required_for(preset, diarized);
    files()
        .into_iter()
        .filter(|model| required.contains(&model.spec.id.as_str()))
        .filter_map(|model| {
            let path = model.resolved();
            let bytes = size(&path).unwrap_or(0);
            (bytes != model.spec.size).then_some(ModelStatus {
                label: model.spec.id.as_str(),
                source: if bytes > 0 {
                    models::source_of(&path)
                } else {
                    ""
                },
                bytes,
                path,
            })
        })
        .collect()
}

/// Remove any `<name>.part` files left by interrupted downloads.
pub fn clear_partial() -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for model in files() {
        let part = part_path(&model.install_path());
        if part.is_file() && fs::remove_file(&part).is_ok() {
            removed.push(part);
        }
    }
    removed
}

/// Fetch one model, trying each mirror in order. A checksum mismatch discards
/// the partial file so the next mirror starts fresh.
fn fetch(
    model: &ModelFile,
    base: Option<&str>,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let urls = mirrors(model.spec, base);
    if urls.is_empty() {
        return Err("no download mirrors configured".to_string());
    }
    let mut failures = Vec::new();
    for url in &urls {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        match fetch_from(model, url, cancel, on_progress) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if cancel.load(Ordering::SeqCst) {
                    return Err("cancelled".to_string());
                }
                failures.push(format!("{url}: {error}"));
            }
        }
    }
    Err(format!("all mirrors failed:\n  {}", failures.join("\n  ")))
}

fn fetch_from(
    model: &ModelFile,
    url: &str,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let path = model.install_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let part = part_path(&path);
    let mut resume_from = if part.is_file() {
        size(&part).unwrap_or(0)
    } else {
        0
    };
    if resume_from > model.spec.size {
        // A stale/oversized partial cannot be resumed; start clean.
        let _ = fs::remove_file(&part);
        resume_from = 0;
    }
    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled".to_string());
    }

    let mut request = ureq::get(url);
    if resume_from > 0 {
        request = request.set("Range", &format!("bytes={resume_from}-"));
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(416, _)) if resume_from > 0 => {
            // The server rejected the range. If the partial is in fact a
            // complete file (some servers do this at EOF), finish it.
            if size(&part) == Some(model.spec.size) {
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
    if done != model.spec.size {
        return Err(format!(
            "incomplete download: got {done} bytes, expected {}",
            model.spec.size
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
    if actual != model.spec.size {
        return Err(format!(
            "incomplete download: got {actual} bytes, expected {}",
            model.spec.size
        ));
    }
    if let Err(error) = verify_hash(part, &model.spec.sha256) {
        // A bad checksum cannot be resumed meaningfully.
        let _ = fs::remove_file(part);
        return Err(error);
    }
    let path = model.install_path();
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    fs::rename(part, &path).map_err(|e| format!("rename: {e}"))
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
            required_for("speed", true),
            ["segmentation", "embedding", "whisper-speed"]
        );
        assert_eq!(
            required_for("balanced", true),
            ["segmentation", "embedding", "whisper-balanced"]
        );
        assert_eq!(
            required_for("accuracy", true),
            ["segmentation", "embedding", "whisper-accuracy"]
        );
        // ASR-only needs no diarization models.
        assert_eq!(required_for("speed", false), ["whisper-speed"]);
        assert_eq!(required_for("balanced", false), ["whisper-balanced"]);
        // Unknown presets fall back to the accuracy model, matching
        // `models::whisper_model`.
        assert_eq!(models::whisper_id("unknown"), "whisper-accuracy");
    }

    #[test]
    fn mirrors_put_the_configured_base_first() {
        let spec = models::spec("whisper-accuracy").unwrap();
        let urls = mirrors(
            spec,
            Some("https://example.test/packages/hippo-models/1.0.0/"),
        );
        assert_eq!(
            urls.first().unwrap(),
            "https://example.test/packages/hippo-models/1.0.0/ggml-large-v3-turbo-q5_0.bin"
        );
        assert_eq!(urls.len(), spec.mirrors.len() + 1);
    }

    #[test]
    fn mirrors_without_a_base_are_the_pinned_upstream_list() {
        let spec = models::spec("whisper-speed").unwrap();
        assert_eq!(mirrors(spec, None), spec.mirrors);
        assert_eq!(mirrors(spec, Some("  ")), spec.mirrors);
    }

    #[test]
    fn mirror_file_names_drop_the_relative_directory() {
        let spec = models::spec("segmentation").unwrap();
        let urls = mirrors(spec, Some("https://example.test/packages"));
        assert!(urls[0].ends_with("/packages/model.onnx"), "{}", urls[0]);
    }

    #[test]
    fn notice_lists_every_model_with_its_license() {
        let text = notice();
        for model in &models::manifest().models {
            assert!(text.contains(&format!("## {}", model.id)));
            assert!(text.contains(&model.license));
            assert!(text.contains(&model.sha256));
        }
    }

    #[test]
    fn sha256_of_known_content() {
        let path =
            std::env::temp_dir().join(format!("myware-scribe-sha-{}", std::process::id()));
        fs::write(&path, b"abc").unwrap();
        let hash = sha256_file(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
