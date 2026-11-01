//! Every path the app resolves at runtime, in one table.
//!
//! A plain build finds everything beside the executable; the packaged build
//! finds the same files inside `C:\Program Files\WindowsApps\...` and writes to
//! a redirected AppData folder. When the GUI and the worker disagree about that,
//! the failure surfaces as "model not found" or "DLL not found" and says nothing
//! about which of the two processes was looking where. `--paths` prints this
//! table for a single process, and the GUI logs it at startup, so the two can be
//! compared directly.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One resolved path and what the file system says about it.
pub struct Entry {
    pub label: &'static str,
    pub path: Option<PathBuf>,
    pub state: String,
}

fn describe(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => format!("{} KiB", meta.len() / 1024),
        Ok(_) => "directory".to_string(),
        Err(_) => "MISSING".to_string(),
    }
}

fn entry(label: &'static str, path: Option<PathBuf>) -> Entry {
    let state = match &path {
        Some(path) => describe(path),
        None => "not found".to_string(),
    };
    Entry { label, path, state }
}

/// Directory holding the executable and the files shipped beside it.
pub fn install_root() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// The sherpa-onnx DLL this build would load, if it exists.
#[cfg(windows)]
fn sherpa_dll() -> Option<PathBuf> {
    crate::sherpa_dynamic::resolved_path()
}

#[cfg(not(windows))]
fn sherpa_dll() -> Option<PathBuf> {
    None
}

/// Every row of the table, in resolution order.
pub fn entries() -> Vec<Entry> {
    let mut rows = Vec::new();
    rows.push(entry("exe", std::env::current_exe().ok()));
    rows.push(entry("install root", install_root()));
    rows.push(entry("sherpa dll", sherpa_dll()));
    rows.push(entry("models", Some(crate::models::models_dir())));
    rows.push(entry("bundled models", crate::models::bundled_dir()));
    rows.push(entry("calibration clip", crate::benchmark::sample_path()));
    rows.push(entry("cache", Some(crate::cache::cache_dir())));
    rows.push(entry("logs", Some(crate::logging::log_dir())));
    rows.push(entry(
        "settings",
        crate::license::config_dir().map(|dir| dir.join("settings.json")),
    ));
    for model in crate::download::status() {
        let state = if model.present() {
            format!("{} KiB ({})", model.bytes / 1024, model.source)
        } else {
            "MISSING".to_string()
        };
        rows.push(Entry {
            label: model.label,
            path: Some(model.path),
            state,
        });
    }
    rows
}

/// The table as plain text.
pub fn report() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "cwd: {}", std::env::current_dir().unwrap_or_default().display());
    for row in entries() {
        let path = match &row.path {
            Some(path) => path.display().to_string(),
            None => "-".to_string(),
        };
        let _ = writeln!(out, "{:<18} {:<10} {}", row.label, row.state, path);
    }
    out
}

/// Write the table to the log so a support report contains the paths that were
/// actually in effect.
pub fn log_all() {
    for line in report().lines() {
        crate::logging::info(format!("paths: {line}"));
    }
}
