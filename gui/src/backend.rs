use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use hippo_transcribe_core::event::{Event, SpeakerStat, Stats, Turn};

/// Stop Windows from giving a console-subsystem helper its own console window.
///
/// The GUI has no console, so spawning `taskkill.exe` without this flag makes
/// Windows allocate one, which the user sees as a terminal flashing up.
fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// CREATE_NO_WINDOW: run the process without a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// Load a transcript from a .txt or .json file as "[time] speaker: text" lines.
pub fn read_transcript(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let is_json = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if !is_json {
        return Some(contents);
    }
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let lines = value.get("lines")?.as_array()?;
    let mut out = String::new();
    for line in lines {
        let time = line.get("time").and_then(|v| v.as_str()).unwrap_or("");
        let speaker = line.get("speaker").and_then(|v| v.as_str()).unwrap_or("");
        let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if speaker.is_empty() {
            out.push_str(&format!("{text}\n"));
        } else {
            out.push_str(&format!("[{time}] {speaker}: {text}\n"));
        }
    }
    Some(out)
}

/// Probe the duration (seconds) of an audio file.
pub fn probe_duration(path: &Path) -> Option<f64> {
    hippo_transcribe_core::audio::duration(path).ok()
}

/// Re-derived diarization turns and waveform for a completed item.
pub type CompletedDetails = (Option<Vec<Turn>>, Option<(f64, Vec<f32>)>);

/// Read a completed item's diarization turns back out of the cache.
pub fn load_turns(path: &Path, preset: &str, num_speakers: Option<i32>) -> Option<Vec<Turn>> {
    let diarizer = hippo_transcribe_core::models::diarizer_id(preset);
    hippo_transcribe_core::cache::key(path, &diarizer, num_speakers)
        .and_then(|key| hippo_transcribe_core::cache::read(&key))
}

/// Re-derive a completed item's diarization turns (from the cache) and
/// waveform (by probing the audio), used when a queue is restored at startup.
pub fn load_completed(
    path: PathBuf,
    preset: String,
    num_speakers: Option<i32>,
    buckets: usize,
) -> CompletedDetails {
    let turns = load_turns(&path, &preset, num_speakers);
    let probe = hippo_transcribe_core::audio::waveform(&path, buckets).ok();
    (turns, probe)
}

/// Fast waveform-only probe for backfilling restored queue items.
pub fn probe_waveform(path: &Path, buckets: usize) -> Option<(f64, Vec<f32>)> {
    hippo_transcribe_core::audio::waveform(path, buckets).ok()
}

pub fn kill(pid: u32) {
    #[cfg(windows)]
    {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/F"]);
        hide_console(&mut command);
        let _ = command.status();
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("kill");
        command.args(["-TERM", &pid.to_string()]);
        hide_console(&mut command);
        let _ = command.status();
    }
}

/// Run the pipeline as a child process, streaming events into the shared queue.
///
/// The GUI binary re-invokes itself with `--cli`, which keeps the heavy
/// whisper/diarization state out of the UI process and lets cancellation be
/// handled with a real process kill.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    inputs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    preset: &'static str,
    num_speakers: Option<i32>,
    language: Option<String>,
    gpu_acceleration: bool,
    cache: bool,
    // False runs ASR only: no diarization, no speaker labels.
    diarized: bool,
    events: Arc<Mutex<VecDeque<Event>>>,
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        if cancelled.load(Ordering::SeqCst) {
            push(&events, Event::AllDone);
            pid.store(0, Ordering::SeqCst);
            return;
        }

        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                push(
                    &events,
                    Event::Error {
                        message: format!(
                            "could not locate the Hippo Transcribe executable: {error}"
                        ),
                    },
                );
                push(&events, Event::AllDone);
                pid.store(0, Ordering::SeqCst);
                return;
            }
        };

        let mut command = Command::new(exe);
        command.arg("--cli");
        for input in &inputs {
            command.arg(input);
        }
        command.arg("--preset").arg(preset);
        // Users always pick a speaker count; the CLI default applies otherwise.
        if let Some(count) = num_speakers {
            command.arg("--num-speakers").arg(count.to_string());
        }
        if !diarized {
            command.arg("--asr-only");
        }
        if let Some(language) = &language {
            command.arg("--language").arg(language);
        }
        if !gpu_acceleration {
            // The core reads this at model-load time. Setting it here only
            // affects the pipeline child, not the GUI process.
            command.env("HIPPO_TRANSCRIBE_ACCEL", "cpu");
        }
        // Pin the data directories to the ones this process resolved. In an
        // MSIX the packaged parent sees its AppData redirected into the package
        // LocalCache, and a spawned child does not necessarily inherit that, so
        // the GUI could have the models downloaded while the worker looked in an
        // empty folder and failed with "whisper model not found".
        command.env(
            "HIPPO_TRANSCRIBE_MODELS_DIR",
            hippo_transcribe_core::models::models_dir(),
        );
        command.env(
            "HIPPO_TRANSCRIBE_CACHE_DIR",
            hippo_transcribe_core::cache::cache_dir(),
        );
        command.env(
            "HIPPO_TRANSCRIBE_LOG_DIR",
            hippo_transcribe_core::logging::log_dir(),
        );
        if let Some(dir) = &output_dir {
            command.arg("--output-dir").arg(dir);
        }
        if !cache {
            command.arg("--no-cache");
        }
        command.arg("--progress-json");
        // The child re-runs this same GUI binary, but a stray console must not
        // appear if the subsystem attribute is ever lost.
        hide_console(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                push(
                    &events,
                    Event::Error {
                        message: format!("could not start the transcription process: {error}"),
                    },
                );
                push(&events, Event::AllDone);
                pid.store(0, Ordering::SeqCst);
                return;
            }
        };

        let child_pid = child.id();
        pid.store(child_pid, Ordering::SeqCst);

        // Cancellation may have arrived between the initial check and spawn.
        let mut killed = cancelled.load(Ordering::SeqCst);
        if killed {
            kill(child_pid);
        }

        let stdout_handle = {
            let events = events.clone();
            let stdout = child.stdout.take();
            std::thread::spawn(move || read_stdout(stdout, &events))
        };
        let stderr_handle = {
            let stderr = child.stderr.take();
            std::thread::spawn(move || read_stderr(stderr))
        };

        // Poll for exit so cancellation can be observed without waiting for
        // the child to finish. Escalate to a hard kill if it ignores the
        // terminate request for too long.
        let mut kill_deadline = killed.then(|| Instant::now() + Duration::from_secs(2));
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if cancelled.load(Ordering::SeqCst) {
                        if !killed {
                            kill(child_pid);
                            killed = true;
                            kill_deadline = Some(Instant::now() + Duration::from_secs(2));
                        } else if kill_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            let _ = child.kill();
                            kill_deadline = None;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break None,
            }
        };

        // A wait error leaves the process state unknown; make sure it cannot
        // outlive us (and get reaped below).
        if status.is_none() {
            let _ = child.kill();
        }
        let status = status.or_else(|| child.wait().ok());

        let (saw_error, saw_all_done) = stdout_handle.join().unwrap_or((false, false));
        let stderr_lines = stderr_handle.join().unwrap_or_default();
        pid.store(0, Ordering::SeqCst);

        let was_cancelled = cancelled.load(Ordering::SeqCst);
        let mut stderr_lines: Vec<String> = stderr_lines
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let last_stderr = stderr_lines.pop();

        for line in stderr_lines {
            push(&events, Event::Log { message: line });
        }

        let failed = status.is_some_and(|status| !status.success());
        if failed && !was_cancelled && !saw_error {
            let message = last_stderr.unwrap_or_else(|| match status.and_then(|s| s.code()) {
                Some(code) => format!("transcription failed (exit code {code})"),
                None => "transcription failed".to_string(),
            });
            push(&events, Event::Error { message });
        } else if let Some(line) = last_stderr {
            push(&events, Event::Log { message: line });
        }

        // The CLI normally emits AllDone itself; only synthesize it when the
        // process died before doing so (cancellation, crash, ...).
        if !saw_all_done {
            push(&events, Event::AllDone);
        }
    });
}

fn read_stdout(stdout: Option<ChildStdout>, events: &Arc<Mutex<VecDeque<Event>>>) -> (bool, bool) {
    let mut saw_error = false;
    let mut saw_all_done = false;
    let Some(stdout) = stdout else {
        return (saw_error, saw_all_done);
    };
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        // A broken pipe or invalid UTF-8 simply ends the stream.
        let Ok(line) = line else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(event) => {
                match &event {
                    Event::Error { .. } => saw_error = true,
                    Event::AllDone => saw_all_done = true,
                    _ => {}
                }
                push(events, event);
            }
            // Non-JSON output is surfaced as a log instead of being dropped.
            Err(_) => push(
                events,
                Event::Log {
                    message: line.to_string(),
                },
            ),
        }
    }
    (saw_error, saw_all_done)
}

fn read_stderr(stderr: Option<ChildStderr>) -> Vec<String> {
    let Some(stderr) = stderr else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        lines.push(line);
    }
    lines
}

fn push(events: &Arc<Mutex<VecDeque<Event>>>, event: Event) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
}
