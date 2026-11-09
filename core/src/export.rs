//! Transcript export formats.
//!
//! Plain text is written unchanged. Everything else is derived from cues: one
//! per transcript line, carrying the speaker and the line's start and end. A
//! caller that only has the rendered `[MM:SS] SPEAKER: text` lines can build
//! them with [`cues_from_text`]; the GUI prefers the sidecar's exact bounds and
//! falls back to the neighbouring line's start.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::{json, Value};

use crate::event::Turn;
use crate::sidecar::Word;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Txt,
    Srt,
    Vtt,
    Json,
    Markdown,
    Csv,
    Docx,
}

impl ExportFormat {
    /// The formats the export lists offer. JSON stays a valid target when the
    /// user types a `.json` name; it is not a menu choice.
    pub const ALL: [ExportFormat; 6] = [
        ExportFormat::Txt,
        ExportFormat::Srt,
        ExportFormat::Vtt,
        ExportFormat::Markdown,
        ExportFormat::Csv,
        ExportFormat::Docx,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Txt => "Plain text",
            ExportFormat::Srt => "SubRip subtitles",
            ExportFormat::Vtt => "WebVTT subtitles",
            ExportFormat::Json => "JSON",
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Csv => "CSV",
            ExportFormat::Docx => "Word document",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Txt => "txt",
            ExportFormat::Srt => "srt",
            ExportFormat::Vtt => "vtt",
            ExportFormat::Json => "json",
            ExportFormat::Markdown => "md",
            ExportFormat::Csv => "csv",
            ExportFormat::Docx => "docx",
        }
    }

    pub fn from_extension(extension: &str) -> Option<Self> {
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        match extension.as_str() {
            "txt" => Some(ExportFormat::Txt),
            "srt" => Some(ExportFormat::Srt),
            "vtt" => Some(ExportFormat::Vtt),
            "json" => Some(ExportFormat::Json),
            "md" | "markdown" => Some(ExportFormat::Markdown),
            "csv" => Some(ExportFormat::Csv),
            "docx" => Some(ExportFormat::Docx),
            _ => None,
        }
    }

    /// Subtitle formats need a timestamp on every cue.
    fn needs_timing(self) -> bool {
        matches!(self, ExportFormat::Srt | ExportFormat::Vtt)
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One transcript line as an export sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    /// `None` for a transcript that carries no timestamps.
    pub start: Option<f32>,
    /// A precise end, when the sidecar has one.
    pub end: Option<f32>,
    pub speaker: String,
    pub text: String,
    /// Word times, when the sidecar has them (v3, quality presets).
    pub words: Vec<Word>,
}

/// Build cues from rendered `[MM:SS] SPEAKER: text` lines.
///
/// A plain transcript yields cues with `start: None`, so a document format can
/// still be produced while a subtitle export refuses rather than inventing
/// times.
pub fn cues_from_text(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (start, rest) = match line.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
            Some((timestamp, rest)) => (parse_timestamp(timestamp), rest.trim_start()),
            None => (None, line),
        };
        let (speaker, text) = match rest.split_once(": ") {
            Some((speaker, text)) => (speaker.trim().to_string(), text.trim().to_string()),
            None => (String::new(), rest.trim().to_string()),
        };
        cues.push(Cue {
            start,
            end: None,
            speaker,
            text,
            words: Vec::new(),
        });
    }
    cues
}

/// Write a transcript in the requested format.
///
/// `transcript_text` is the rendered document and is written verbatim for
/// [`ExportFormat::Txt`]. `cues` is the structured view, `turns` the
/// diarization segments for the JSON export, and `duration` the file's length
/// when known.
pub fn write(
    path: &Path,
    format: ExportFormat,
    transcript_text: &str,
    cues: &[Cue],
    turns: &[Turn],
    duration: Option<f32>,
) -> io::Result<()> {
    if format.needs_timing() {
        require_timing(cues)?;
    }
    // A subtitle cue may cover a whole sentence; word times let it be cut into
    // readable pieces without splitting a word.
    let timed = match format {
        ExportFormat::Srt | ExportFormat::Vtt => split_cues(cues),
        _ => cues.to_vec(),
    };
    match format {
        ExportFormat::Txt => fs::write(path, transcript_text.as_bytes()),
        ExportFormat::Srt => fs::write(path, render_srt(&timed, duration).as_bytes()),
        ExportFormat::Vtt => fs::write(path, render_vtt(&timed, duration).as_bytes()),
        ExportFormat::Json => {
            let json = render_json(cues, turns, duration)?;
            fs::write(path, json.as_bytes())
        }
        ExportFormat::Markdown => fs::write(path, render_markdown(cues, duration).as_bytes()),
        ExportFormat::Csv => fs::write(path, render_csv(cues, duration).as_bytes()),
        ExportFormat::Docx => crate::docx::write(path, cues, duration),
    }
}

/// Refuse a subtitle export of a transcript that has no timestamps.
fn require_timing(cues: &[Cue]) -> io::Result<()> {
    if cues.is_empty() || cues.iter().any(|cue| cue.start.is_none()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "this transcript has no timestamps, so it cannot be exported as subtitles",
        ));
    }
    Ok(())
}

fn render_srt(cues: &[Cue], duration: Option<f32>) -> String {
    let mut out = String::new();
    for (index, cue) in cues.iter().enumerate() {
        let start = cue.start.unwrap_or(0.0);
        let end = cue_end(cues, index, duration);
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            srt_timestamp(start),
            srt_timestamp(end),
            cue_text(cue)
        ));
    }
    out
}

fn render_vtt(cues: &[Cue], duration: Option<f32>) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for (index, cue) in cues.iter().enumerate() {
        let start = cue.start.unwrap_or(0.0);
        let end = cue_end(cues, index, duration);
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            vtt_timestamp(start),
            vtt_timestamp(end),
            cue_text(cue)
        ));
    }
    out
}

/// Split cues that carry word times into short subtitle cues.
///
/// Anything without words, or short enough already, is passed through.
fn split_cues(cues: &[Cue]) -> Vec<Cue> {
    const MAX_WORDS: usize = 8;
    const MAX_SECONDS: f32 = 5.0;
    let mut out = Vec::with_capacity(cues.len());
    for cue in cues {
        let Some(start) = cue.start else {
            out.push(cue.clone());
            continue;
        };
        if cue.words.is_empty() {
            out.push(cue.clone());
            continue;
        }
        let mut groups: Vec<&[Word]> = Vec::new();
        let mut first = 0usize;
        for (index, word) in cue.words.iter().enumerate() {
            let group_start = cue.words[first].start;
            let too_many = index - first + 1 > MAX_WORDS;
            let too_long = word.end - group_start > MAX_SECONDS && index > first;
            if too_many || too_long {
                groups.push(&cue.words[first..index]);
                first = index;
            }
        }
        groups.push(&cue.words[first..]);

        if groups.len() <= 1 {
            out.push(cue.clone());
            continue;
        }
        for group in groups {
            let Some(first_word) = group.first() else {
                continue;
            };
            let Some(last_word) = group.last() else {
                continue;
            };
            let text = group
                .iter()
                .map(|word| word.text.trim())
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() {
                continue;
            }
            out.push(Cue {
                start: Some(first_word.start.max(start)),
                end: Some(last_word.end.max(first_word.start)),
                speaker: cue.speaker.clone(),
                text,
                words: group.to_vec(),
            });
        }
    }
    out
}

fn render_json(cues: &[Cue], turns: &[Turn], duration: Option<f32>) -> io::Result<String> {
    let use_hours = duration.is_some_and(|seconds| seconds >= 3600.0)
        || cues
            .iter()
            .any(|cue| cue.start.is_some_and(|start| start >= 3600.0));
    let lines: Vec<Value> = cues
        .iter()
        .map(|cue| {
            json!({
                "time": cue.start.map(|start| clock_timestamp(start, use_hours)).unwrap_or_default(),
                "speaker": cue.speaker,
                "text": cue.text,
                "start": cue.start,
                "end": cue.end,
                "words": cue.words,
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

fn render_markdown(cues: &[Cue], duration: Option<f32>) -> String {
    let use_hours = duration.is_some_and(|seconds| seconds >= 3600.0)
        || cues
            .iter()
            .any(|cue| cue.start.is_some_and(|start| start >= 3600.0));
    let mut out = String::new();
    for cue in cues {
        let speaker = escape_markdown(&cue.speaker);
        let text = escape_markdown(&cue.text);
        match (speaker.is_empty(), cue.start) {
            (true, Some(start)) => {
                out.push_str(&format!("**{}**\n\n{}\n\n", clock_timestamp(start, use_hours), text))
            }
            (true, None) => out.push_str(&format!("{text}\n\n")),
            (false, Some(start)) => out.push_str(&format!(
                "**{speaker}** — {}\n\n{}\n\n",
                clock_timestamp(start, use_hours),
                text
            )),
            (false, None) => out.push_str(&format!("**{speaker}**\n\n{text}\n\n")),
        }
    }
    out
}

fn render_csv(cues: &[Cue], duration: Option<f32>) -> String {
    let use_hours = duration.is_some_and(|seconds| seconds >= 3600.0)
        || cues
            .iter()
            .any(|cue| cue.start.is_some_and(|start| start >= 3600.0));
    let mut out = String::from("time,speaker,text\n");
    for cue in cues {
        let time = cue
            .start
            .map(|start| clock_timestamp(start, use_hours))
            .unwrap_or_default();
        out.push_str(&format!(
            "{},{},{}\n",
            csv_field(&time),
            csv_field(&cue.speaker),
            csv_field(&cue.text)
        ));
    }
    out
}

fn cue_end(cues: &[Cue], index: usize, duration: Option<f32>) -> f32 {
    let start = cues[index].start.unwrap_or(0.0);
    if let Some(end) = cues[index].end.filter(|end| *end > start) {
        return end;
    }
    match cues.get(index + 1).and_then(|next| next.start) {
        Some(next) if next > start => next,
        _ => duration.unwrap_or(start + 5.0),
    }
}

fn cue_text(cue: &Cue) -> String {
    match (cue.speaker.is_empty(), cue.text.is_empty()) {
        (true, _) => cue.text.clone(),
        (false, true) => cue.speaker.clone(),
        (false, false) => format!("{}: {}", cue.speaker, cue.text),
    }
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

/// Escape the characters that would otherwise start Markdown emphasis.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '*' | '_' | '`' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// RFC 4180 field: quote when the value contains a comma, quote or newline.
fn csv_field(text: &str) -> String {
    if text.contains(|character| matches!(character, ',' | '"' | '\n' | '\r')) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SAMPLE: &str = "[00:00] SPEAKER 1: Hello there\n\
[00:07] SPEAKER 2: Hi: how are you?\n\
[01:02:03] SPEAKER 1: Long recording\n";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hippo-transcribe-export-{}-{name}",
            std::process::id()
        ));
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
        let cues = cues_from_text(SAMPLE);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].start, Some(0.0));
        assert_eq!(cues[0].speaker, "SPEAKER 1");
        assert_eq!(cues[0].text, "Hello there");
        assert_eq!(cues[1].start, Some(7.0));
        assert_eq!(cues[1].text, "Hi: how are you?");
        assert_eq!(cues[2].start, Some(3723.0));
        assert_eq!(cues[2].text, "Long recording");
    }

    #[test]
    fn txt_is_unchanged() {
        let dir = temp_dir("txt");
        let path = dir.join("out.txt");
        let cues = cues_from_text(SAMPLE);
        write(&path, ExportFormat::Txt, SAMPLE, &cues, &turns(), None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SAMPLE);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn srt_uses_next_start_and_final_duration() {
        let dir = temp_dir("srt");
        let path = dir.join("out.srt");
        let cues = cues_from_text(SAMPLE);
        write(
            &path,
            ExportFormat::Srt,
            SAMPLE,
            &cues,
            &turns(),
            Some(3800.0),
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("1\n00:00:00,000 --> 00:00:07,000\nSPEAKER 1: Hello there\n\n"));
        assert!(text.contains("2\n00:00:07,000 --> 01:02:03,000\nSPEAKER 2: Hi: how are you?\n\n"));
        assert!(text.contains("3\n01:02:03,000 --> 01:03:20,000\nSPEAKER 1: Long recording"));
        assert_eq!(
            ExportFormat::from_extension(".SRT"),
            Some(ExportFormat::Srt)
        );
        assert_eq!(
            ExportFormat::from_extension(".DOCX"),
            Some(ExportFormat::Docx)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stored_end_wins_over_the_next_start() {
        let dir = temp_dir("srt-end");
        let path = dir.join("out.srt");
        let cues = vec![
            Cue {
                start: Some(0.0),
                end: Some(2.5),
                speaker: "A".to_string(),
                text: "one".to_string(),
                words: Vec::new(),
            },
            Cue {
                start: Some(5.0),
                end: None,
                speaker: "B".to_string(),
                text: "two".to_string(),
                words: Vec::new(),
            },
        ];
        write(&path, ExportFormat::Srt, "", &cues, &[], Some(9.0)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("00:00:00,000 --> 00:00:02,500"));
        assert!(text.contains("00:00:05,000 --> 00:00:09,000"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn vtt_has_header_and_dot_timestamps() {
        let dir = temp_dir("vtt");
        let path = dir.join("out.vtt");
        let cues = cues_from_text(SAMPLE);
        write(&path, ExportFormat::Vtt, SAMPLE, &cues, &turns(), None).unwrap();
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
        let cues = cues_from_text(SAMPLE);
        write(
            &path,
            ExportFormat::Json,
            SAMPLE,
            &cues,
            &turns(),
            Some(3800.0),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let lines = value.get("lines").unwrap().as_array().unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["time"], "00:00:00");
        assert_eq!(lines[0]["speaker"], "SPEAKER 1");
        assert_eq!(lines[0]["text"], "Hello there");
        assert_eq!(lines[0]["start"], 0.0);
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
        let cues = cues_from_text("[00:05] SPEAKER 1: short\n");
        write(
            &path,
            ExportFormat::Json,
            "",
            &cues,
            &[],
            Some(60.0),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["lines"][0]["time"], "00:05");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn markdown_is_readable_prose() {
        let dir = temp_dir("md");
        let path = dir.join("out.md");
        let cues = cues_from_text("[00:01] SPEAKER 1: He said *this*\n");
        write(&path, ExportFormat::Markdown, "", &cues, &[], Some(30.0)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("**SPEAKER 1** — 00:01"));
        assert!(text.contains("He said \\*this\\*"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn csv_quotes_commas_and_quotes() {
        let dir = temp_dir("csv");
        let path = dir.join("out.csv");
        let cues = cues_from_text("[00:01] SPEAKER 1: Hi, \"you\"\n");
        write(&path, ExportFormat::Csv, "", &cues, &[], Some(30.0)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("time,speaker,text\n"));
        assert!(text.contains("00:01,SPEAKER 1,\"Hi, \"\"you\"\"\""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_cues_split_at_word_times() {
        let dir = temp_dir("srt-split");
        let path = dir.join("out.srt");
        let words: Vec<Word> = (0..12)
            .map(|index| Word {
                start: index as f32 * 0.5,
                end: index as f32 * 0.5 + 0.4,
                text: format!("w{index}"),
            })
            .collect();
        let cues = vec![Cue {
            start: Some(0.0),
            end: Some(6.0),
            speaker: "A".to_string(),
            text: (0..12).map(|index| format!("w{index}")).collect::<Vec<_>>().join(" "),
            words,
        }];
        write(&path, ExportFormat::Srt, "", &cues, &[], Some(10.0)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(" --> ").count(), 2, "{text}");
        assert!(text.contains("00:00:00,000 --> 00:00:03,900"), "{text}");
        assert!(text.contains("w0 w1 w2 w3 w4 w5 w6 w7"), "{text}");
        assert!(text.contains("w8 w9 w10 w11"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn subtitles_refuse_an_untimed_transcript() {
        let dir = temp_dir("untimed");
        let path = dir.join("out.srt");
        let cues = vec![Cue {
            start: None,
            end: None,
            speaker: "A".to_string(),
            text: "hello".to_string(),
            words: Vec::new(),
        }];
        let error = write(&path, ExportFormat::Srt, "hello\n", &cues, &[], None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn untimed_text_still_exports_as_markdown_and_csv() {
        let dir = temp_dir("untimed-docs");
        let cues = vec![Cue {
            start: None,
            end: None,
            speaker: "A".to_string(),
            text: "hello".to_string(),
            words: Vec::new(),
        }];
        let md = dir.join("out.md");
        write(&md, ExportFormat::Markdown, "", &cues, &[], None).unwrap();
        assert!(fs::read_to_string(&md).unwrap().contains("**A**\n\nhello"));
        let csv = dir.join("out.csv");
        write(&csv, ExportFormat::Csv, "", &cues, &[], None).unwrap();
        assert!(fs::read_to_string(&csv).unwrap().contains(",A,hello"));
        let _ = fs::remove_dir_all(&dir);
    }
}
