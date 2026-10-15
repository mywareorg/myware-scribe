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
    pub preset: String,
    pub speaker_stats: HashMap<String, SpeakerStat>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    #[serde(rename = "stage")]
    Stage { stage: String, label: String, progress: f32 },
    #[serde(rename = "file")]
    File { audio: String },
    #[serde(rename = "meta")]
    Meta { audio: String, duration: f32 },
    #[serde(rename = "turns")]
    Turns {
        turns: Vec<Turn>,
    },
    #[serde(rename = "line")]
    Line {
        start: f32,
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
    #[serde(rename = "all_done")]
    AllDone,
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
    let script = r#"import sys, numpy as np, soundfile as sf
path = sys.argv[1]
n = int(sys.argv[2])
info = sf.info(path)
sr = info.samplerate
frames = info.frames
sumsq = np.zeros(n, dtype='float64')
counts = np.zeros(n, dtype='int64')
pos = 0
for block in sf.blocks(path, blocksize=65536, dtype='float32', always_2d=True):
    m = block.mean(axis=1).astype('float64')
    k = len(m)
    if k:
        idx = (np.arange(pos, pos + k) * n) // max(1, frames)
        sumsq += np.bincount(idx, weights=m * m, minlength=n)
        counts += np.bincount(idx, minlength=n)
        pos += k
rms = [float(np.sqrt(sumsq[i] / counts[i])) if counts[i] else 0.0 for i in range(n)]
print(frames / sr if sr else 0.0)
print(','.join(str(x) for x in rms))
"#;
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
        let venv = root.join(".venv").join("Scripts").join("python.exe");
        if venv.is_file() {
            return venv;
        }
        root.join("runtime").join("python.exe")
    } else {
        let venv = root.join(".venv").join("bin").join("python");
        if venv.is_file() {
            return venv;
        }
        root.join("runtime").join("bin").join("python3")
    }
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
    inputs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
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
        command.arg(&script);
        for input in &inputs {
            command.arg(input);
        }
        command
            .arg("--preset")
            .arg(preset)
            .arg("--progress-json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(dir) = &output_dir {
            command.arg("--output-dir").arg(dir);
        }
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

        let mut saw_all_done = false;
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
                    if matches!(event, Event::AllDone) {
                        saw_all_done = true;
                    }
                    push(&events, event);
                } else {
                    push(&events, Event::Log { message: line.to_string() });
                }
            }
        }

        let status = child.wait();
        pid.store(0, Ordering::SeqCst);

        if !saw_all_done && !cancelled.load(Ordering::SeqCst) {
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
