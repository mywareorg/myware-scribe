use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

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
/// holds the transcript as it was before any edit. It is never taken for the
/// transcript itself: adding the audio again reads `<stem>.txt` or the
/// pipeline's own output names, not this copy.
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

    /// Make `path` older or newer than the files around it without sleeping.
    fn set_modified(path: &Path, offset_secs: i64) {
        let time = if offset_secs >= 0 {
            SystemTime::now() + std::time::Duration::from_secs(offset_secs as u64)
        } else {
            SystemTime::now() - std::time::Duration::from_secs((-offset_secs) as u64)
        };
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn the_transcript_candidate_prefers_the_exact_name() {
        let dir = scratch("candidate");
        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let exact = dir.join("talk.txt");
        std::fs::write(&exact, "[00:00] A: one\n").unwrap();
        let pipeline = dir.join("talk-balanced-transcription-2speakers.txt");
        std::fs::write(&pipeline, "[00:00] A: two\n").unwrap();
        // The pipeline file is newer and still loses to the plain name.
        set_modified(&exact, -60);
        set_modified(&pipeline, 60);

        assert_eq!(
            find_transcripts(&[audio.clone()], None),
            vec![(audio.clone(), exact.clone())]
        );

        // Without the plain copy, the newest pipeline-named file wins.
        std::fs::remove_file(&exact).unwrap();
        set_modified(&pipeline, -60);
        let newer = dir.join("talk-fast-transcription-1speaker.txt");
        std::fs::write(&newer, "[00:00] A: three\n").unwrap();
        set_modified(&newer, 60);
        assert_eq!(
            find_transcripts(&[audio.clone()], None),
            vec![(audio.clone(), newer.clone())]
        );

        // The save location is the fallback when the audio's own directory has
        // nothing.
        let elsewhere = scratch("candidate-out");
        let moved = elsewhere.join("talk.txt");
        std::fs::write(&moved, "[00:00] A: four\n").unwrap();
        std::fs::remove_file(&pipeline).unwrap();
        std::fs::remove_file(&newer).unwrap();
        assert_eq!(
            find_transcripts(&[audio.clone()], Some(&elsewhere)),
            vec![(audio.clone(), moved)]
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn empty_files_and_edit_sidecars_are_not_transcripts() {
        let dir = scratch("candidate-empty");
        let audio = dir.join("empty.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        std::fs::write(dir.join("empty-unedited.txt"), "old\n").unwrap();
        std::fs::write(dir.join("empty.txt.hippo-transcribe.bak"), "backup\n").unwrap();

        // Only an empty plain copy and the app's own sidecars: no transcript.
        assert!(find_transcripts(&[audio.clone()], None).is_empty());

        // A pipeline file with content is taken once the plain copy is empty.
        let pipeline = dir.join("empty-balanced-transcription-2speakers.txt");
        std::fs::write(&pipeline, "[00:00] A: kept\n").unwrap();
        assert_eq!(
            find_transcripts(&[audio.clone()], None),
            vec![(audio.clone(), pipeline)]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_without_a_transcript_has_no_candidate() {
        let dir = scratch("candidate-none");
        let audio = dir.join("silent.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        std::fs::write(dir.join("silent-slow-transcription-2speakers.json"), "{}").unwrap();

        assert!(find_transcripts(&[audio], None).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A file in a directory that has already been read once.
struct ScannedFile {
    /// File name, lowercased so the comparisons are case-insensitive the way
    /// the filesystems this ships on usually are.
    name: String,
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

/// One directory's files, read once per add.
struct DirScan {
    files: Vec<ScannedFile>,
}

impl DirScan {
    fn read(dir: &Path) -> Option<DirScan> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_lowercase) else {
                continue;
            };
            files.push(ScannedFile {
                name,
                path: entry.path(),
                size: metadata.len(),
                modified: metadata.modified().ok(),
            });
        }
        Some(DirScan { files })
    }

    fn usable(&self) -> impl Iterator<Item = &ScannedFile> {
        self.files
            .iter()
            .filter(|file| file.size > 0 && !is_edit_sidecar(&file.name))
    }

    /// The transcript for `stem` in this directory: the exact `<stem>.txt`
    /// first, then the newest pipeline-named sibling.
    fn candidate(&self, stem: &str) -> Option<PathBuf> {
        let stem = stem.to_lowercase();
        let exact = format!("{stem}.txt");
        if let Some(file) = self.usable().find(|file| file.name == exact) {
            return Some(file.path.clone());
        }
        self.usable()
            .filter(|file| is_transcription_of(&file.name, &stem))
            .max_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.name.cmp(&b.name)))
            .map(|file| file.path.clone())
    }
}

/// Files the app writes beside a transcript that must never be taken for it.
fn is_edit_sidecar(name: &str) -> bool {
    name.ends_with(".hippo-transcribe.bak") || name.ends_with("-unedited.txt")
}

/// Whether `name` is the pipeline's own output for `stem`:
/// `<stem>-<preset>-transcription-<N>speakers.txt`.
///
/// Only the prefix and the marker are checked, so an audio file whose stem is a
/// prefix of another's (`talk` and `talk-interview`) would match the longer
/// one's output. Tightening that would mean guessing the preset list, which is
/// the worse trade.
fn is_transcription_of(name: &str, stem: &str) -> bool {
    name.strip_prefix(stem)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rest| rest.ends_with(".txt") && rest.contains("transcription"))
}

/// The transcript for each added audio file that already has one, as
/// `(audio, transcript)`.
///
/// The rule, applied to the audio's own directory first and then to the
/// configured save location:
///
/// 1. `<stem>.txt`, the name Save As defaults to.
/// 2. The most recently modified `<stem>-*transcription*.txt`, the pipeline's
///    own naming.
///
/// An exact `<stem>.txt` that is not one of the app's files is accepted as a
/// transcript anyway: a sidecar with that name is indistinguishable from one
/// the app wrote, and the alternative — parsing every candidate to guess — is
/// worse than the risk.
///
/// Directories are read once per call, so a folder add of `n` files costs one
/// `read_dir` per directory rather than `n`.
pub(crate) fn find_transcripts(
    added: &[PathBuf],
    output_dir: Option<&Path>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut scans: Vec<(PathBuf, Option<DirScan>)> = Vec::new();
    let mut found = Vec::new();

    for audio in added {
        let Some(stem) = audio.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let mut transcript = None;
        for dir in [audio.parent(), output_dir].into_iter().flatten() {
            let scan = match scans.iter().position(|(cached, _)| cached == dir) {
                Some(position) => scans[position].1.as_ref(),
                None => {
                    scans.push((dir.to_path_buf(), DirScan::read(dir)));
                    scans.last().and_then(|(_, scan)| scan.as_ref())
                }
            };
            if let Some(path) = scan.and_then(|scan| scan.candidate(stem)) {
                transcript = Some(path);
                break;
            }
        }
        if let Some(transcript) = transcript {
            found.push((audio.clone(), transcript));
        }
    }

    found
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
