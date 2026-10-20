//! Transcript export formats.
//!
//! Plain text is written unchanged, while subtitle formats are derived from
//! the `[MM:SS] SPEAKER: text` (or `[HH:MM:SS] ...`) lines produced by the
//! pipeline.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::{json, Value};

use crate::event::Turn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Txt,
    Srt,
    Vtt,
    Json,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 4] = [
        ExportFormat::Txt,
        ExportFormat::Srt,
        ExportFormat::Vtt,
        ExportFormat::Json,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Txt => "Plain text",
            ExportFormat::Srt => "SubRip subtitles",
            ExportFormat::Vtt => "WebVTT subtitles",
            ExportFormat::Json => "JSON",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Txt => "txt",
            ExportFormat::Srt => "srt",
            ExportFormat::Vtt => "vtt",
            ExportFormat::Json => "json",
        }
    }

    pub fn from_extension(extension: &str) -> Option<Self> {
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        match extension.as_str() {
            "txt" => Some(ExportFormat::Txt),
            "srt" => Some(ExportFormat::Srt),
            "vtt" => Some(ExportFormat::Vtt),
            "json" => Some(ExportFormat::Json),
            _ => None,
        }
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Cue {
    start: f32,
    speaker: String,
    text: String,
}

/// Write `transcript_text` to `path` in the requested format.
pub fn write(
    path: &Path,
    format: ExportFormat,
    transcript_text: &str,
    turns: &[Turn],
    duration: Option<f32>,
) -> io::Result<()> {
    match format {
        ExportFormat::Txt => fs::write(path, transcript_text.as_bytes()),
        ExportFormat::Srt => fs::write(path, render_srt(transcript_text, duration).as_bytes()),
        ExportFormat::Vtt => fs::write(path, render_vtt(transcript_text, duration).as_bytes()),
        ExportFormat::Json => {
            let json = render_json(transcript_text, turns, duration)?;
            fs::write(path, json.as_bytes())
        }
    }
}

fn render_srt(transcript_text: &str, duration: Option<f32>) -> String {
    let cues = parse_cues(transcript_text);
    let mut out = String::new();
    for (index, cue) in cues.iter().enumerate() {
        let end = cue_end(&cues, index, duration);
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            srt_timestamp(cue.start),
            srt_timestamp(end),
            cue_text(cue)
        ));
    }
    out
}

fn render_vtt(transcript_text: &str, duration: Option<f32>) -> String {
    let cues = parse_cues(transcript_text);
    let mut out = String::from("WEBVTT\n\n");
    for (index, cue) in cues.iter().enumerate() {
        let end = cue_end(&cues, index, duration);
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            vtt_timestamp(cue.start),
            vtt_timestamp(end),
            cue_text(cue)
        ));
    }
    out
}

fn render_json(transcript_text: &str, turns: &[Turn], duration: Option<f32>) -> io::Result<String> {
    let cues = parse_cues(transcript_text);
    let use_hours = duration.is_some_and(|seconds| seconds >= 3600.0)
        || cues.iter().any(|cue| cue.start >= 3600.0);
    let lines: Vec<Value> = cues
        .iter()
        .map(|cue| {
            json!({
                "time": clock_timestamp(cue.start, use_hours),
                "speaker": cue.speaker,
                "text": cue.text,
            })
        })
        .collect();

    let mut root = json!({ "lines": lines });
    if let Some(object) = root.as_object_mut() {
        object.insert(
            "turns".to_string(),
            serde_json::to_value(turns).unwrap_or(Value::Array(Vec::new())),
        );
        if let Some(duration) = duration {
            object.insert("duration".to_string(), json!(duration));
        }
    }
    serde_json::to_string_pretty(&root).map_err(io::Error::other)
}

fn parse_cues(transcript_text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    for line in transcript_text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((timestamp, rest)) = rest.split_once(']') else {
            continue;
        };
        let Some(start) = parse_timestamp(timestamp) else {
            continue;
        };
        let rest = rest.trim_start();
        let (speaker, text) = match rest.split_once(": ") {
            Some((speaker, text)) => (speaker.trim().to_string(), text.trim().to_string()),
            None => (String::new(), rest.trim().to_string()),
        };
        cues.push(Cue {
            start,
            speaker,
            text,
        });
    }
    cues
}

fn parse_timestamp(timestamp: &str) -> Option<f32> {
    let parts: Vec<f32> = timestamp
        .trim()
        .split(':')
        .map(|part| part.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .ok()?;
    match parts.as_slice() {
        [minutes, seconds] => Some(minutes * 60.0 + seconds),
        [hours, minutes, seconds] => Some(hours * 3600.0 + minutes * 60.0 + seconds),
        _ => None,
    }
}

fn cue_end(cues: &[Cue], index: usize, duration: Option<f32>) -> f32 {
    match cues.get(index + 1) {
        Some(next) => next.start,
        None => duration.unwrap_or_else(|| cues[index].start + 5.0),
    }
}

fn cue_text(cue: &Cue) -> String {
    match (cue.speaker.is_empty(), cue.text.is_empty()) {
        (true, _) => cue.text.clone(),
        (false, true) => cue.speaker.clone(),
        (false, false) => format!("{}: {}", cue.speaker, cue.text),
    }
}

fn clock_timestamp(seconds: f32, use_hours: bool) -> String {
    let total = seconds.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if use_hours || hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

fn srt_timestamp(seconds: f32) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let millis = total_ms % 1000;
    let secs = (total_ms / 1000) % 60;
    let minutes = (total_ms / 60_000) % 60;
    let hours = total_ms / 3_600_000;
    format!("{hours:02}:{minutes:02}:{secs:02},{millis:03}")
}

fn vtt_timestamp(seconds: f32) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let millis = total_ms % 1000;
    let secs = (total_ms / 1000) % 60;
    let minutes = (total_ms / 60_000) % 60;
    let hours = total_ms / 3_600_000;
    format!("{hours:02}:{minutes:02}:{secs:02}.{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SAMPLE: &str = "[00:00] SPEAKER 1: Hello there\n\
[00:07] SPEAKER 2: Hi: how are you?\n\
[01:02:03] SPEAKER 1: Long recording\n";

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("transiel-export-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn turns() -> Vec<Turn> {
        vec![
            Turn {
                start: 0.0,
                end: 7.0,
                speaker: "SPEAKER 1".to_string(),
            },
            Turn {
                start: 7.0,
                end: 10.0,
                speaker: "SPEAKER 2".to_string(),
            },
        ]
    }

    #[test]
    fn parses_mm_ss_and_hh_mm_ss_with_colons_in_text() {
        let cues = parse_cues(SAMPLE);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].start, 0.0);
        assert_eq!(cues[0].speaker, "SPEAKER 1");
        assert_eq!(cues[0].text, "Hello there");
        assert_eq!(cues[1].start, 7.0);
        assert_eq!(cues[1].text, "Hi: how are you?");
        assert_eq!(cues[2].start, 3723.0);
        assert_eq!(cues[2].text, "Long recording");
    }

    #[test]
    fn txt_is_unchanged() {
        let dir = temp_dir("txt");
        let path = dir.join("out.txt");
        write(&path, ExportFormat::Txt, SAMPLE, &turns(), None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SAMPLE);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn srt_uses_next_start_and_final_duration() {
        let dir = temp_dir("srt");
        let path = dir.join("out.srt");
        write(&path, ExportFormat::Srt, SAMPLE, &turns(), Some(3800.0)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("1\n00:00:00,000 --> 00:00:07,000\nSPEAKER 1: Hello there\n\n"));
        assert!(text.contains("2\n00:00:07,000 --> 01:02:03,000\nSPEAKER 2: Hi: how are you?\n\n"));
        assert!(text.contains("3\n01:02:03,000 --> 01:03:20,000\nSPEAKER 1: Long recording"));
        assert_eq!(
            ExportFormat::from_extension(".SRT"),
            Some(ExportFormat::Srt)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn vtt_has_header_and_dot_timestamps() {
        let dir = temp_dir("vtt");
        let path = dir.join("out.vtt");
        write(&path, ExportFormat::Vtt, SAMPLE, &turns(), None).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("WEBVTT\n\n"));
        assert!(text.contains("00:00:00.000 --> 00:00:07.000"));
        // No duration: the last cue gets a five second window.
        assert!(text.contains("01:02:03.000 --> 01:02:08.000"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_matches_the_import_shape() {
        let dir = temp_dir("json");
        let path = dir.join("out.json");
        write(&path, ExportFormat::Json, SAMPLE, &turns(), Some(3800.0)).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let lines = value.get("lines").unwrap().as_array().unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["time"], "00:00:00");
        assert_eq!(lines[0]["speaker"], "SPEAKER 1");
        assert_eq!(lines[0]["text"], "Hello there");
        assert_eq!(lines[1]["time"], "00:00:07");
        assert_eq!(lines[2]["time"], "01:02:03");
        assert_eq!(value.get("turns").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(value.get("duration").unwrap().as_f64(), Some(3800.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_keeps_mm_ss_for_short_recordings() {
        let dir = temp_dir("json-short");
        let path = dir.join("out.json");
        write(
            &path,
            ExportFormat::Json,
            "[00:05] SPEAKER 1: short\n",
            &[],
            Some(60.0),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["lines"][0]["time"], "00:05");
        let _ = fs::remove_dir_all(&dir);
    }
}
