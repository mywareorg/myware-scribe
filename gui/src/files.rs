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

/// Sibling copy of the transcript as it was before the first edit:
/// `talk.txt` -> `talk-unedited.txt`.
pub(crate) fn unedited_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript".to_string());
    path.with_file_name(format!("{stem}-unedited.txt"))
}

/// Rewrite a transcript in place, keeping the previous contents as a `.bak`.
///
/// The first rewrite also preserves the untouched original as
/// `<stem>-unedited.txt`; later writes leave that file alone, so it always
/// holds the transcript as it was before any edit. It can be re-imported from
/// the Import menu.
pub(crate) fn write_transcript_file(path: &Path, text: &str) -> std::io::Result<()> {
    if path.is_file() {
        let _ = std::fs::copy(path, transcript_backup_path(path));
        let unedited = unedited_path(path);
        if !unedited.exists() {
            let _ = std::fs::copy(path, unedited);
        }
    }
    write_atomic(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hippo-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_first_edit_keeps_an_unedited_copy() {
        let dir = scratch("unedited");
        let path = dir.join("talk.txt");
        std::fs::write(&path, "original\n").unwrap();

        write_transcript_file(&path, "edited once\n").unwrap();
        let unedited = unedited_path(&path);
        assert_eq!(unedited.file_name().unwrap(), "talk-unedited.txt");
        assert_eq!(std::fs::read_to_string(&unedited).unwrap(), "original\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited once\n");

        // A later edit rewrites the transcript but never the original copy.
        write_transcript_file(&path, "edited twice\n").unwrap();
        assert_eq!(std::fs::read_to_string(&unedited).unwrap(), "original\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited twice\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creating_a_new_transcript_makes_no_unedited_copy() {
        let dir = scratch("fresh");
        let path = dir.join("fresh.txt");
        write_transcript_file(&path, "first write\n").unwrap();
        assert!(!unedited_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
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
