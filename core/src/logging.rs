//! Rolling file log and crash reports.
//!
//! The Windows GUI has no console, so a failure leaves nothing behind unless it
//! is written to a file. This keeps one active log plus four rotated copies of
//! two megabytes each, and at most five crash reports, so the folder offered to
//! a user by "Open log folder" stays small enough to read.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Active log plus four rotated copies.
const LOG_FILES: usize = 5;
const CRASH_FILES: usize = 5;

static SINK: OnceLock<Mutex<File>> = OnceLock::new();

/// Where the logs live: `<data dir>/logs`, the same data directory the cache
/// and models use.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = crate::env_compat("MYWARE_SCRIBE_LOG_DIR", "TRANSIEL_LOG_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            let base = PathBuf::from(base);
            let root = crate::adopt_dir(base.join("myware-scribe"), base.join("transiel"));
            return root.join("logs");
        }
    } else if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        let root = crate::adopt_dir(
            home.join(".local/share/myware-scribe"),
            home.join(".local/share/transiel"),
        );
        return root.join("logs");
    }
    PathBuf::from("logs")
}

/// Open the log, rotating it first when the previous run filled it. Returns the
/// active file, or `None` when it could not be created; logging is never fatal.
pub fn init() -> Option<PathBuf> {
    if SINK.get().is_some() {
        return Some(active_path());
    }
    let dir = log_dir();
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let active = dir.join("myware.log");
    rotate(&dir, &active);

    let file = OpenOptions::new().create(true).append(true).open(&active).ok()?;
    SINK.set(Mutex::new(file)).ok()?;
    info(format!(
        "MyWare Scribe {} starting ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    Some(active)
}

fn active_path() -> PathBuf {
    log_dir().join("myware.log")
}

/// Shift `myware.log` to `myware.1.log`, dropping the oldest copy.
fn rotate(dir: &std::path::Path, active: &std::path::Path) {
    let full = fs::metadata(active).map(|meta| meta.len()).unwrap_or(0);
    if full < MAX_BYTES {
        return;
    }
    let name = |index: usize| {
        if index == 0 {
            dir.join("myware.log")
        } else {
            dir.join(format!("myware.{index}.log"))
        }
    };
    for index in (1..LOG_FILES).rev() {
        let from = name(index);
        if from.exists() {
            let _ = fs::rename(&from, name(index + 1));
        }
    }
    let _ = fs::remove_file(name(LOG_FILES));
    let _ = fs::rename(active, name(1));
}

pub fn info(message: impl std::fmt::Display) {
    write("INFO", &message.to_string());
}

pub fn warn(message: impl std::fmt::Display) {
    write("WARN", &message.to_string());
}

pub fn error(message: impl std::fmt::Display) {
    write("ERROR", &message.to_string());
}

fn write(level: &str, message: &str) {
    let Some(sink) = SINK.get() else { return };
    let Ok(mut file) = sink.lock() else { return };
    // One line per entry: a multi-line message would break the log's shape.
    let flattened = message.replace(['\r', '\n'], " | ");
    let _ = writeln!(file, "{} {level} {flattened}", timestamp());
    let _ = file.flush();
}

/// Save a crash report beside the logs, keeping only the newest few.
pub fn crash(report: &str) -> Option<PathBuf> {
    let dir = log_dir();
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("crash-{}.txt", compact_timestamp()));
    fs::write(&path, report).ok()?;
    prune_crashes(&dir);
    Some(path)
}

fn prune_crashes(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut crashes: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("crash-") && name.ends_with(".txt"))
        })
        .collect();
    if crashes.len() <= CRASH_FILES {
        return;
    }
    // The names embed a sortable timestamp, so the oldest sort first.
    crashes.sort();
    let excess = crashes.len() - CRASH_FILES;
    for path in crashes.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

fn now_parts() -> (i64, u32, u32, u32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Days to civil date (Howard Hinnant's algorithm), so no date crate is needed.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month as u32,
        day as u32,
        (rest / 3_600) as u32,
        ((rest % 3_600) / 60) as u32,
        (rest % 60) as u32,
    )
}

/// `2026-10-29 13:22:01Z`, UTC so it cannot lie about the offset.
fn timestamp() -> String {
    let (year, month, day, hour, minute, second) = now_parts();
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

/// `20261029-132201`, safe in a file name and sorting chronologically.
fn compact_timestamp() -> String {
    let (year, month, day, hour, minute, second) = now_parts();
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_a_calendar_date() {
        let text = timestamp();
        assert_eq!(text.len(), 20, "{text}");
        assert!(text.ends_with('Z'));
        let year: i64 = text[..4].parse().unwrap();
        assert!(year >= 2024, "{text}");
    }
}
