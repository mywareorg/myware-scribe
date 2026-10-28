use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Backup kept next to a transcript before it is rewritten in place.
pub(crate) fn transcript_backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript".to_string());
    path.with_file_name(format!("{name}.hippo-transcribe.bak"))
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

/// The AUMID Windows attributes a toast to. Under MSIX this must be the
/// package's own `PackageFamilyName!ApplicationId`, otherwise the shell drops
/// the notification. Unpackaged builds keep notify-rust's default.
#[cfg(target_os = "windows")]
fn packaged_app_id() -> Option<String> {
    use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentApplicationUserModelId;

    // An AUMID is capped at 130 characters.
    let mut buffer = vec![0u16; 130];
    let mut length = buffer.len() as u32;
    let status = unsafe { GetCurrentApplicationUserModelId(&mut length, buffer.as_mut_ptr()) };
    if status != 0 || length == 0 {
        return None;
    }
    // The returned length includes the terminating null.
    let end = (length as usize).saturating_sub(1).min(buffer.len());
    let id = String::from_utf16_lossy(&buffer[..end]);
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// Fire a best-effort desktop notification (non-blocking).
pub(crate) fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let mut notification = notify_rust::Notification::new();
        notification.summary(&title).body(&body);
        #[cfg(target_os = "windows")]
        if let Some(id) = packaged_app_id() {
            notification.app_id(&id);
        }
        let _ = notification.show();
    });
}
