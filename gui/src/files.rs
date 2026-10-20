use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Backup kept next to a transcript before it is rewritten in place.
pub(crate) fn transcript_backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript".to_string());
    path.with_file_name(format!("{name}.transiel.bak"))
}

/// Write through a temporary file in the same directory, then rename over the
/// target so an interrupted write cannot leave a half-written transcript.
pub(crate) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript".to_string());
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = directory.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));

    let result = (|| {
        std::fs::write(&temp, text)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&temp, metadata.permissions());
        }
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Rewrite a transcript in place, keeping the previous contents as a `.bak`.
pub(crate) fn write_transcript_file(path: &Path, text: &str) -> std::io::Result<()> {
    if path.is_file() {
        let _ = std::fs::copy(path, transcript_backup_path(path));
    }
    write_atomic(path, text)
}

/// Open a file or folder with the platform's default handler.
pub(crate) fn open_path(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
}

/// Fire a best-effort desktop notification (non-blocking).
pub(crate) fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary(&title)
            .body(&body)
            .show();
    });
}
