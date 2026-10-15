use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Turn {
    pub start: f32,
    pub end: f32,
    pub speaker: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct SpeakerStat {
    pub seconds: f32,
    pub segments: usize,
    pub words: usize,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Stats {
    pub audio_seconds: f64,
    pub run_seconds: f64,
    pub speakers: usize,
    pub num_speakers: Option<i32>,
    pub cache_hit: bool,
    pub turns: usize,
    pub lines: usize,
    pub preset: String,
    pub diarizer: String,
    pub embedding: String,
    pub transcriber: String,
    pub device: String,
    pub transcription_device: String,
    pub speaker_stats: HashMap<String, SpeakerStat>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "event")]
#[allow(dead_code)]
pub enum Event {
    #[serde(rename = "stage")]
    Stage { stage: String, label: String, progress: f32 },
    #[serde(rename = "meta")]
    Meta { audio: String, duration: f32 },
    #[serde(rename = "turns")]
    Turns {
        speakers: HashMap<String, String>,
        turns: Vec<Turn>,
    },
    #[serde(rename = "line")]
    Line {
        start: f32,
        end: f32,
        speaker: String,
        text: String,
        confidence: f32,
    },
    #[serde(rename = "done")]
    Done {
        txt: String,
        stats: Stats,
    },
    #[serde(rename = "log")]
    Log { message: String },
    #[serde(rename = "error")]
    Error { message: String },
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

/// Probe the duration (seconds) of an audio file using the venv's soundfile.
pub fn probe_duration(path: &Path) -> Option<f64> {
    probe_audio(path, 1).map(|(duration, _)| duration)
}

/// Probe duration (seconds) and a downsampled waveform (RMS peaks) for an audio file.
pub fn probe_audio(path: &Path, buckets: usize) -> Option<(f64, Vec<f32>)> {
    let root = repo_root()?;
    let python = venv_python(&root);
    if !python.is_file() {
        return None;
    }
    let script = "import sys, numpy as np, soundfile as sf\n\
d, sr = sf.read(sys.argv[1], dtype='float32', always_2d=True)\n\
m = d.mean(axis=1)\n\
n = int(sys.argv[2])\n\
edges = np.linspace(0, len(m), n + 1).astype(int)\n\
rms = [float(np.sqrt(np.mean(np.square(m[edges[i]:max(edges[i]+1, edges[i+1])])))) for i in range(n)]\n\
print(len(m) / sr)\n\
print(','.join(str(x) for x in rms))";
    let mut command = Command::new(&python);
    command
        .args(["-c", script])
        .arg(path)
        .arg(buckets.to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let duration: f64 = lines.next()?.trim().parse().ok()?;
    let peaks: Vec<f32> = lines
        .next()?
        .split(',')
        .filter_map(|value| value.trim().parse().ok())
        .collect();
    Some((duration, peaks))
}

/// Locate the repository root (directory containing scripts/transiel.py).
pub fn repo_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("TRANSIEL_ROOT") {
        let path = PathBuf::from(root);
        if path.join("scripts").join("transiel.py").is_file() {
            return Some(path);
        }
    }

    let mut starts = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            starts.push(dir.to_path_buf());
        }
    }

    for start in starts {
        let mut dir = start;
        loop {
            if dir.join("scripts").join("transiel.py").is_file() {
                return Some(dir);
            }
            if !dir.pop() {
                break;
            }
        }
    }
    None
}

pub fn venv_python(root: &Path) -> PathBuf {
    if cfg!(windows) {
        root.join(".venv").join("Scripts").join("python.exe")
    } else {
        root.join(".venv").join("bin").join("python")
    }
}

pub fn output_base(output_dir: Option<&Path>, input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_string());
    let dir = output_dir
        .map(|p| p.to_path_buf())
        .or_else(|| input.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!("{stem}-transcript"))
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

/// Spawn the Python pipeline and stream its JSON progress events.
pub fn spawn(
    python: PathBuf,
    root: PathBuf,
    input: PathBuf,
    base: PathBuf,
    preset: &'static str,
    timestamps: bool,
    join_speaker: bool,
    num_speakers: Option<i32>,
    cache: bool,
    events: Arc<Mutex<VecDeque<Event>>>,
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
) {
    let script = root.join("scripts").join("transiel.py");
    std::thread::spawn(move || {
        let mut command = Command::new(&python);
        command
            .arg(&script)
            .arg(&input)
            .arg("--preset")
            .arg(preset)
            .arg("--output")
            .arg(&base)
            .arg("--progress-json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if !timestamps {
            command.arg("--no-timestamps");
        }
        if join_speaker {
            command.arg("--join-speaker");
        }
        match num_speakers {
            Some(count) => {
                command.arg("--num-speakers").arg(count.to_string());
            }
            None => {
                command.arg("--auto-speakers");
            }
        }
        if !cache {
            command.arg("--no-cache");
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                push(&events, Event::Error { message: format!("failed to start python: {error}") });
                return;
            }
        };

        pid.store(child.id(), Ordering::SeqCst);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let Some(stderr) = stderr {
            let events = events.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(stderr);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim();
                    if !line.is_empty() {
                        push(&events, Event::Log { message: line.to_string() });
                    }
                }
            });
        }

        let mut saw_done = false;
        if let Some(stdout) = stdout {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<Event>(line) {
                    if matches!(event, Event::Done { .. }) {
                        saw_done = true;
                    }
                    push(&events, event);
                } else {
                    push(&events, Event::Log { message: line.to_string() });
                }
            }
        }

        let status = child.wait();
        pid.store(0, Ordering::SeqCst);

        if !saw_done && !cancelled.load(Ordering::SeqCst) {
            let message = match status {
                Ok(s) if s.success() => "process exited unexpectedly".to_string(),
                Ok(s) => format!("process exited with {s}"),
                Err(e) => format!("process error: {e}"),
            };
            push(&events, Event::Error { message });
        }
    });
}

fn push(events: &Arc<Mutex<VecDeque<Event>>>, event: Event) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
}
