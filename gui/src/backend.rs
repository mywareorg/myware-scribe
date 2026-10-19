use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use transiel_core::pipeline::{self, PipelineOptions};

pub use transiel_core::event::{Event, SpeakerStat, Stats, Turn};

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
    transiel_core::audio::duration(path).ok()
}

/// Re-derived diarization turns and waveform for a completed item.
pub type CompletedDetails = (Option<Vec<Turn>>, Option<(f64, Vec<f32>)>);

/// Read a completed item's diarization turns back out of the cache.
pub fn load_turns(path: &Path, num_speakers: Option<i32>) -> Option<Vec<Turn>> {
    let diarizer = transiel_core::models::diarizer_id();
    transiel_core::cache::key(path, &diarizer, num_speakers)
        .and_then(|key| transiel_core::cache::read(&key))
}

/// Re-derive a completed item's diarization turns (from the cache) and
/// waveform (by probing the audio), used when a queue is restored at startup.
pub fn load_completed(
    path: PathBuf,
    num_speakers: Option<i32>,
    buckets: usize,
) -> CompletedDetails {
    let turns = load_turns(&path, num_speakers);
    let probe = transiel_core::audio::waveform(&path, buckets).ok();
    (turns, probe)
}

/// Fast waveform-only probe for backfilling restored queue items.
pub fn probe_waveform(path: &Path, buckets: usize) -> Option<(f64, Vec<f32>)> {
    transiel_core::audio::waveform(path, buckets).ok()
}

pub fn kill(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    }
}

/// Run the pipeline in-process, streaming events into the shared queue.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    inputs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    preset: &'static str,
    num_speakers: Option<i32>,
    cache: bool,
    events: Arc<Mutex<VecDeque<Event>>>,
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let options = PipelineOptions {
            inputs,
            output_dir,
            preset: preset.to_string(),
            num_speakers,
            timestamps: true,
            cache,
            cancel: Some(cancelled.clone()),
        };
        pipeline::run(&options, &mut |event| {
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            push(&events, event);
        });
        pid.store(0, Ordering::SeqCst);
    });
}

fn push(events: &Arc<Mutex<VecDeque<Event>>>, event: Event) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
}
