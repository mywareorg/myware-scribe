use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::documents::{line_prefix, line_text, DocLine};

use hippo_transcribe_core::sidecar;

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

/// Per-line confidences from the JSON sidecar of a transcript, if present.
///
/// Empty when there is no sidecar. The caller drops the values when the line
/// count no longer matches the document.
pub fn read_confidences(path: &Path) -> Vec<Option<f32>> {
    sidecar::read(path)
        .map(|sidecar| {
            sidecar
                .lines
                .into_iter()
                .map(|line| line.confidence)
                .collect()
        })
        .unwrap_or_default()
}

/// Rewrite a transcript's sidecar from the edited document.
///
/// The sidecar is the metadata companion to the rendered text: this is the only
/// writer, and it replaces `lines` wholesale so a merge, split, delete or undo
/// cannot leave stale bounds behind. `turns` are diarization output and are
/// preserved untouched; a missing sidecar is a no-op, so hand-written
/// transcripts never gain one.
pub fn write_sidecar(path: &Path, lines: &[DocLine]) {
    write_sidecar_renamed(path, lines, &[]);
}

/// As [`write_sidecar`], also renaming the stored speaker identities.
///
/// A rename moves [`sidecar::Speaker::name`] rather than minting a new speaker,
/// so the ids that turns reference survive it.
pub fn write_sidecar_renamed(path: &Path, lines: &[DocLine], mapping: &[(String, String)]) {
    let Some(json) = sidecar::locate(path) else {
        return;
    };
    let Some(mut sidecar) = sidecar::read(path) else {
        return;
    };
    for speaker in &mut sidecar.speakers {
        if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == speaker.name) {
            speaker.name = new.clone();
        }
    }
    // Diarization segments keep their geometry but follow the rename, so the
    // timeline and the stored ids agree with what is on screen.
    for turn in &mut sidecar.turns {
        if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == turn.speaker) {
            turn.speaker = new.clone();
        }
    }
    // A document loaded from a plain transcript has no timings of its own.
    // While the line counts still agree, the old sidecar's timing is the only
    // honest source, so an edit to such a document does not erase it.
    let previous: Vec<sidecar::Line> = if sidecar.lines.len() == lines.len() {
        sidecar.lines.clone()
    } else {
        Vec::new()
    };
    sidecar.rebuild(
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| sidecar_line(line, previous.get(index)))
            .collect(),
    );
    let _ = sidecar::write_at(&json, &sidecar);
}

/// Word times for one transcript line, from its sidecar.
pub fn read_line_words(path: &Path, index: usize) -> Vec<hippo_transcribe_core::sidecar::Word> {
    sidecar::read(path)
        .and_then(|sidecar| sidecar.lines.get(index).map(|line| line.words.clone()))
        .unwrap_or_default()
}

/// Build export cues from the shown document.
///
/// `source` is the transcript file whose sidecar holds exact line ends. When
/// the sidecar is missing or no longer matches the document, the ends are left
/// to the renderer, which falls back to the next line's start.
pub fn export_cues(
    source: Option<&Path>,
    lines: &[DocLine],
) -> Vec<hippo_transcribe_core::export::Cue> {
    let stored = source
        .and_then(|path| sidecar::read(path))
        .filter(|sidecar| sidecar.lines.len() == lines.len());
    let ends: Vec<Option<f32>> = stored
        .as_ref()
        .map(|sidecar| sidecar.lines.iter().map(|line| line.end).collect())
        .unwrap_or_default();
    let words: Vec<Vec<hippo_transcribe_core::sidecar::Word>> = stored
        .as_ref()
        .map(|sidecar| sidecar.lines.iter().map(|line| line.words.clone()).collect())
        .unwrap_or_default();
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| hippo_transcribe_core::export::Cue {
            start: line.start,
            end: ends.get(index).copied().flatten(),
            speaker: line.speaker.clone(),
            text: line_text(line),
            words: words.get(index).cloned().unwrap_or_default(),
        })
        .collect()
}

/// One document line as the sidecar stores it.
///
/// `previous` is the stored line at the same index when the document and the
/// sidecar still line up; it supplies timing the document cannot (a plain
/// transcript has none) and keeps the pipeline's timestamp shape.
fn sidecar_line(line: &DocLine, previous: Option<&sidecar::Line>) -> sidecar::Line {
    // Reuse the stored line when the document still points at the same place:
    // the same start, the same spoken text (a transcript parsed from rendered
    // text only has whole seconds, so its start will not equal the stored
    // one), or a document with no timestamps at all, where the index is all
    // there is.
    let text = line_text(line);
    let reuse = previous.is_some_and(|previous| {
        line.start.is_none() || previous.start == line.start || previous.text == text
    });
    let (start, time) = match (line.start, previous) {
        (_, Some(previous)) if reuse => (line.start.or(previous.start), previous.time.clone()),
        (Some(_), _) => (line.start, sidecar_time(line)),
        (None, _) => (None, String::new()),
    };
    sidecar::Line {
        time,
        speaker: line.speaker.clone(),
        text,
        start,
        // An untouched line keeps the ASR confidence it was stored with; an
        // edited one carries its own (the editor stamps human confidence).
        confidence: line.confidence.or_else(|| {
            previous
                .filter(|_| reuse)
                .and_then(|previous| previous.confidence)
        }),
        // Word times belong to the text they were decoded from; an edit drops
        // them rather than attaching them to different words.
        words: previous
            .filter(|_| reuse)
            .map(|previous| previous.words.clone())
            .unwrap_or_default(),
        speaker_id: String::new(),
        end: None,
    }
}

/// The sidecar's `time` field for a line: the timestamp without brackets, the
/// same shape the pipeline writes.
fn sidecar_time(line: &DocLine) -> String {
    if let Some(stamp) = line_prefix(&line.raw)
        .trim()
        .strip_prefix('[')
        .and_then(|stamp| stamp.strip_suffix(']'))
    {
        return stamp.to_string();
    }
    match line.start {
        Some(start) => crate::format::format_timestamp(start as f64)
            .trim_matches(['[', ']'])
            .to_string(),
        None => String::new(),
    }
}

/// Probe the duration (seconds) of an audio file.
pub fn probe_duration(path: &Path) -> Option<f64> {
    hippo_transcribe_core::audio::duration(path).ok()
}

/// Re-derived diarization turns and waveform for a completed item.
pub type CompletedDetails = (Option<Vec<Turn>>, Option<(f64, Vec<f32>)>);

/// Read a completed item's diarization turns back out of the cache.
///
/// Keys on a hash of the file bytes rather than the decoded samples: a queue
/// restored at startup has the file, not the samples. The pipeline writes the
/// turns under both keys for that reason (see `cache_keys` in
/// `core/src/pipeline.rs`).
pub fn load_turns(
    path: &Path,
    num_speakers: Option<i32>,
    normalize: bool,
) -> Option<Vec<Turn>> {
    // Same salt the pipeline writes with: model pair, turn geometry, and the
    // preprocessing that produced the audio.
    let diarizer = format!(
        "{}-{}",
        hippo_transcribe_core::diarize::cache_salt(),
        hippo_transcribe_core::audio::preprocess_tag(normalize)
    );
    hippo_transcribe_core::cache::key(path, &diarizer, num_speakers)
        .and_then(|key| hippo_transcribe_core::cache::read(&key))
}

/// Re-derive a completed item's diarization turns (from the cache) and
/// waveform (by probing the audio), used when a queue is restored at startup.
pub fn load_completed(
    path: PathBuf,
    num_speakers: Option<i32>,
    buckets: usize,
    normalize: bool,
) -> CompletedDetails {
    let turns = load_turns(&path, num_speakers, normalize);
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
    // Lift quiet recordings before ASR and diarization.
    normalize: bool,
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
        command.arg(if normalize {
            "--normalize"
        } else {
            "--no-normalize"
        });
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

// ---- briefing worker ----

use hippo_briefing::BriefingLine;

/// Progress from the briefing helper, in the order it happened.
#[derive(Debug, Clone)]
pub enum BriefingEvent {
    Stage { label: String, fraction: f32 },
    Done,
    Failed(String),
}

/// Spawn `hippo-summarize` for `lines`.
///
/// The helper runs in its own process: llama.cpp and whisper.cpp each vendor
/// ggml, and one binary linking both is a symbol clash waiting to happen. It
/// writes the result to a response file; the returned path is where the caller
/// reads it after `BriefingEvent::Done`.
pub fn spawn_briefing(
    lines: Vec<BriefingLine>,
    model: std::path::PathBuf,
    events: Arc<Mutex<VecDeque<BriefingEvent>>>,
    pid: Arc<AtomicU32>,
) -> Result<PathBuf, String> {
    let helper = briefing_helper()?;
    let dir = hippo_transcribe_core::license::config_dir()
        .ok_or_else(|| "no configuration directory".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;

    let stamp = std::process::id();
    let request = dir.join(format!("briefing-request-{stamp}.json"));
    let response = dir.join(format!("briefing-response-{stamp}.json"));
    let _ = std::fs::remove_file(&response);
    let body = serde_json::json!({
        "lines": lines,
        "model": model.display().to_string(),
    });
    std::fs::write(&request, body.to_string())
        .map_err(|error| format!("write {}: {error}", request.display()))?;

    let mut command = Command::new(&helper);
    command
        .arg("--request")
        .arg(&request)
        .arg("--response")
        .arg(&response)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {}: {error}", helper.display()))?;
    pid.store(child.id(), Ordering::SeqCst);

    let output_events = events.clone();
    std::thread::spawn(move || {
        let stderr = child.stderr.take().map(|stderr| {
            let sink = Arc::new(Mutex::new(String::new()));
            let text = sink.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Ok(mut text) = text.lock() {
                        text.push_str(&line);
                        text.push('\n');
                        // Keep only the tail: llama.cpp can be chatty.
                        if text.len() > 4000 {
                            let keep = text.len() - 2000;
                            *text = text[keep..].to_string();
                        }
                    }
                }
            });
            sink
        });

        let mut reported_failure = false;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match value.get("event").and_then(|event| event.as_str()) {
                    Some("stage") => {
                        let label = value
                            .get("label")
                            .and_then(|label| label.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let fraction = value
                            .get("fraction")
                            .and_then(|fraction| fraction.as_f64())
                            .unwrap_or(0.0) as f32;
                        if let Ok(mut queue) = output_events.lock() {
                            queue.push_back(BriefingEvent::Stage { label, fraction });
                        }
                    }
                    Some("done") => {}
                    Some("error") => {
                        let message = value
                            .get("message")
                            .and_then(|message| message.as_str())
                            .unwrap_or("the briefing failed")
                            .to_string();
                        reported_failure = true;
                        if let Ok(mut queue) = output_events.lock() {
                            queue.push_back(BriefingEvent::Failed(message));
                        }
                    }
                    _ => {}
                }
            }
        }

        let status = child.wait().ok();
        let _ = std::fs::remove_file(&request);
        if !reported_failure {
            let ok = status.as_ref().is_some_and(|status| status.success());
            if ok {
                if let Ok(mut queue) = output_events.lock() {
                    queue.push_back(BriefingEvent::Done);
                }
            } else {
                let detail = stderr
                    .as_ref()
                    .and_then(|text| text.lock().ok().map(|text| text.trim().to_string()))
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| {
                        format!("the briefing helper exited with {status:?}")
                    });
                if let Ok(mut queue) = output_events.lock() {
                    queue.push_back(BriefingEvent::Failed(detail));
                }
            }
        }
    });

    Ok(response)
}

/// The helper beside this executable.
fn briefing_helper() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not locate the executable: {error}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "the executable has no directory".to_string())?;
    let name = if cfg!(windows) {
        "hippo-summarize.exe"
    } else {
        "hippo-summarize"
    };
    let path = dir.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "the briefing helper is missing from {}",
            dir.display()
        ))
    }
}

fn push(events: &Arc<Mutex<VecDeque<Event>>>, event: Event) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hippo-backend-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn doc_line(raw: &str, speaker: &str, start: f32) -> DocLine {
        DocLine {
            raw: raw.to_string(),
            speaker: speaker.to_string(),
            start: Some(start),
            confidence: None,
        }
    }

    fn seed(dir: &PathBuf) -> PathBuf {
        let transcript = dir.join("talk.txt");
        let sidecar = dir.join("talk-ts.json");
        std::fs::write(&transcript, "[00:01] ALICE: hello\n[00:04] BOB: hi\n").unwrap();
        std::fs::write(
            &sidecar,
            r#"{
                "lines": [
                    {"time": "00:01", "speaker": "ALICE", "text": "hello", "start": 1.0, "confidence": -0.2},
                    {"time": "00:04", "speaker": "BOB", "text": "hi", "start": 4.0, "confidence": null}
                ],
                "turns": [
                    {"start": 0.5, "end": 3.0, "speaker": "ALICE"},
                    {"start": 3.0, "end": 8.0, "speaker": "BOB"}
                ],
                "duration": 10.0
            }"#,
        )
        .unwrap();
        transcript
    }

    #[test]
    fn saving_an_edit_rewrites_the_lines_and_keeps_the_turns() {
        let dir = scratch("edit");
        let transcript = seed(&dir);
        let lines = vec![
            doc_line("[00:01] ALICE: hello there", "ALICE", 1.0),
            doc_line("[00:04] BOB: hi", "BOB", 4.0),
        ];
        write_sidecar(&transcript, &lines);

        let saved = sidecar::read(&transcript).expect("sidecar");
        assert_eq!(saved.version, 3);
        assert_eq!(saved.lines[0].text, "hello there");
        // Clamped to the turn each line starts in.
        assert_eq!(saved.lines[0].end, Some(3.0));
        assert_eq!(saved.lines[1].end, Some(8.0));
        assert_eq!(saved.turns.len(), 2);
        assert_eq!(saved.speakers.len(), 2);
        // Confidences the edit did not touch survive.
        assert_eq!(saved.lines[0].confidence, Some(-0.2));
    }

    #[test]
    fn a_rename_moves_the_name_and_keeps_the_id() {
        let dir = scratch("rename");
        let transcript = seed(&dir);
        let before = sidecar::read(&transcript).unwrap();
        let alice_id = before.speakers[0].id.clone();

        let lines = vec![
            doc_line("[00:01] ALICE SMITH: hello", "ALICE SMITH", 1.0),
            doc_line("[00:04] BOB: hi", "BOB", 4.0),
        ];
        let mapping = vec![("ALICE".to_string(), "ALICE SMITH".to_string())];

        write_sidecar_renamed(&transcript, &lines, &mapping);

        let saved = sidecar::read(&transcript).unwrap();
        assert_eq!(saved.speakers[0].name, "ALICE SMITH");
        assert_eq!(saved.speakers[0].id, alice_id);
        assert_eq!(saved.turns[0].speaker, "ALICE SMITH");
        assert_eq!(saved.turns[0].speaker_id, alice_id);
        assert_eq!(saved.lines[0].speaker_id, alice_id);
    }

    #[test]
    fn editing_a_plain_transcript_keeps_the_stored_timing() {
        let dir = scratch("plain");
        let transcript = dir.join("notes.txt");
        std::fs::write(&transcript, "hello\nworld\n").unwrap();
        std::fs::write(
            dir.join("notes-ts.json"),
            r#"{
                "lines": [
                    {"time": "00:03", "speaker": "A", "text": "hello", "start": 3.0, "confidence": -0.1},
                    {"time": "00:07", "speaker": "A", "text": "world", "start": 7.0, "confidence": null}
                ],
                "turns": [],
                "duration": 20.0
            }"#,
        )
        .unwrap();

        // A document parsed from the plain text has no starts of its own.
        let lines = vec![
            DocLine {
                raw: "hello there".to_string(),
                speaker: "A".to_string(),
                start: None,
                confidence: None,
            },
            DocLine {
                raw: "world".to_string(),
                speaker: "A".to_string(),
                start: None,
                confidence: None,
            },
        ];

        write_sidecar(&transcript, &lines);

        let saved = sidecar::read(&transcript).unwrap();
        assert_eq!(saved.lines[0].start, Some(3.0));
        assert_eq!(saved.lines[0].time, "00:03");
        assert_eq!(saved.lines[0].end, Some(7.0));
    }

    #[test]
    fn sidecar_words_reach_the_exporter_and_survive_a_save() {
        let dir = scratch("words");
        let transcript = dir.join("talk.txt");
        std::fs::write(&transcript, "[00:00] A: hello there\n").unwrap();
        std::fs::write(
            dir.join("talk-ts.json"),
            r#"{
                "version": 3,
                "lines": [{
                    "time": "00:00", "speaker": "A", "text": "hello there",
                    "start": 0.0, "confidence": -0.2, "end": 2.0, "speaker_id": "S1",
                    "words": [
                        {"start": 0.0, "end": 0.8, "text": "hello"},
                        {"start": 0.8, "end": 1.4, "text": "there"}
                    ]
                }],
                "turns": [],
                "duration": 5.0,
                "speakers": [{"id": "S1", "name": "A"}]
            }"#,
        )
        .unwrap();

        let lines = vec![doc_line("[00:00] A: hello there", "A", 0.0)];
        let cues = export_cues(Some(&transcript), &lines);
        assert_eq!(cues[0].words.len(), 2);
        assert_eq!(cues[0].words[0].text, "hello");

        write_sidecar(&transcript, &lines);

        let saved = sidecar::read(&transcript).unwrap();
        assert_eq!(saved.lines[0].words.len(), 2);
        assert_eq!(saved.lines[0].words[0].text, "hello");
    }

    #[test]
    fn saving_without_a_sidecar_writes_nothing() {
        let dir = scratch("none");
        let transcript = dir.join("hand-written.txt");
        std::fs::write(&transcript, "hello\n").unwrap();

        write_sidecar(&transcript, &[doc_line("hello", "", 0.0)]);

        assert!(!dir.join("hand-written.json").exists());
        assert!(!dir.join("hand-written-ts.json").exists());
    }
}
