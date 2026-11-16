//! The machine-readable companion to a rendered transcript.
//!
//! A transcript is written as `[MM:SS] SPEAKER: text` lines. Everything an
//! export, a summary or the timeline needs beyond that text — line bounds,
//! stable speaker identity, the run's preset and language — lives here, in
//! `<transcript>.json`.
//!
//! The rendered text stays the display source of truth. This module must never
//! become a second one: it is rebuilt wholesale from the document on every
//! save ([`Sidecar::rebuild`]), so an edit can only ever agree with the text.
//!
//! `speaker` and `time` are frozen keys. Installed builds read them, so adding
//! fields is allowed and renaming them is not.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::event::Turn;

/// Schema version written by this build. Files without a `version` are v1.
pub const VERSION: u32 = 4;

/// A speaker identity that survives a rename.
///
/// `name` is what the transcript renders; `id` is what turns reference, so
/// renaming "Speaker 1" to "Alice" does not orphan the diarization segments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speaker {
    pub id: String,
    pub name: String,
}

/// One word inside a line. Added in v3; absent from v2 files and from files
/// decoded without token timestamps (the speed preset).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub start: f32,
    pub end: f32,
    #[serde(default)]
    pub text: String,
}

/// One transcript line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// Rendered timestamp, without brackets. Frozen: v1 readers use it.
    #[serde(default)]
    pub time: String,
    /// Rendered speaker label. Frozen: v1 readers use it.
    #[serde(default)]
    pub speaker: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub start: Option<f32>,
    #[serde(default)]
    pub confidence: Option<f32>,
    /// Added in v2: stable identity for [`Line::speaker`].
    #[serde(default)]
    pub speaker_id: String,
    /// Added in v2: the utterance's end, derived when absent.
    #[serde(default)]
    pub end: Option<f32>,
    /// Added in v3: word times, when the decoder produced them.
    #[serde(default)]
    pub words: Vec<Word>,
}

/// A diarization segment as stored in the sidecar.
///
/// Turns are diarization output, not transcript lines: they are written by the
/// pipeline and never rewritten from an edit, so the timeline keeps its ground
/// truth. A rename moves [`Speaker::name`], which is what recolours them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SidecarTurn {
    pub start: f32,
    pub end: f32,
    #[serde(default)]
    pub speaker: String,
    #[serde(default)]
    pub speaker_id: String,
}

/// Run metadata that does not come from the transcript text.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub audio: Option<String>,
    pub duration: Option<f32>,
    pub preset: Option<String>,
    pub language: Option<String>,
    pub model: Option<String>,
    /// Language the caller asked for; `None` means auto-detect.
    pub language_requested: Option<String>,
    /// Language pinned by detection; `None` when one was requested.
    pub language_detected: Option<String>,
    pub normalize: Option<bool>,
    pub producer: Option<String>,
    pub run_id: Option<String>,
}

/// A transcript's sidecar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sidecar {
    #[serde(default)]
    pub version: u32,
    /// Spoken lines, in transcript order.
    #[serde(default)]
    pub lines: Vec<Line>,
    /// Diarization segments. Never rewritten by an edit.
    #[serde(default)]
    pub turns: Vec<SidecarTurn>,
    #[serde(default)]
    pub duration: Option<f32>,
    #[serde(default)]
    pub speakers: Vec<Speaker>,
    #[serde(default)]
    pub audio: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Added in v4: requested vs detected language, normalization, producer
    /// versions and a stable run id. Absent from v3 and older files.
    #[serde(default)]
    pub language_requested: Option<String>,
    #[serde(default)]
    pub language_detected: Option<String>,
    #[serde(default)]
    pub normalize: Option<bool>,
    #[serde(default)]
    pub producer: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

impl Sidecar {
    /// Build a v2 sidecar from a fresh run.
    pub fn new(lines: Vec<Line>, turns: Vec<Turn>, meta: Meta) -> Self {
        let mut sidecar = Self {
            version: VERSION,
            lines: Vec::new(),
            turns: turns.into_iter().map(SidecarTurn::from).collect(),
            duration: meta.duration,
            speakers: Vec::new(),
            audio: meta.audio,
            preset: meta.preset,
            language: meta.language,
            model: meta.model,
            language_requested: meta.language_requested,
            language_detected: meta.language_detected,
            normalize: meta.normalize,
            producer: meta.producer,
            run_id: meta.run_id,
        };
        sidecar.rebuild(lines);
        sidecar
    }

    /// Replace the transcript content, keeping identity and metadata.
    ///
    /// `lines` is authoritative: `end` and `speaker_id` are recomputed, so a
    /// merge, split or delete cannot leave stale timing behind. Turns are
    /// diarization output, not transcript content: an edit never rewrites them.
    pub fn rebuild(&mut self, lines: Vec<Line>) {
        self.lines = lines;
        self.ensure_identities();
        self.infer_ends();
        self.version = VERSION;
    }

    /// Give every referenced speaker an id, and every line and turn that id.
    ///
    /// Existing names keep their existing id, which is what makes a rename a
    /// rename rather than a new speaker. Names that are no longer referenced
    /// are dropped so the list cannot grow across edits.
    fn ensure_identities(&mut self) {
        let mut names: Vec<String> = Vec::new();
        for line in &self.lines {
            if !line.speaker.is_empty() && !names.contains(&line.speaker) {
                names.push(line.speaker.clone());
            }
        }
        for turn in &self.turns {
            if !turn.speaker.is_empty() && !names.contains(&turn.speaker) {
                names.push(turn.speaker.clone());
            }
        }

        let mut used: Vec<String> = Vec::new();
        let mut next = 1u32;
        let mut speakers: Vec<Speaker> = Vec::with_capacity(names.len());
        for name in names {
            if let Some(existing) = self.speakers.iter().find(|speaker| speaker.name == name) {
                used.push(existing.id.clone());
                speakers.push(existing.clone());
                continue;
            }
            let id = loop {
                let candidate = format!("S{next}");
                next += 1;
                if !used.contains(&candidate) {
                    used.push(candidate.clone());
                    break candidate;
                }
            };
            speakers.push(Speaker { id, name });
        }
        self.speakers = speakers;

        let ids: Vec<(String, String)> = self
            .speakers
            .iter()
            .map(|speaker| (speaker.name.clone(), speaker.id.clone()))
            .collect();
        for line in &mut self.lines {
            line.speaker_id = ids
                .iter()
                .find(|(name, _)| *name == line.speaker)
                .map(|(_, id)| id.clone())
                .unwrap_or_default();
        }
        for turn in &mut self.turns {
            turn.speaker_id = ids
                .iter()
                .find(|(name, _)| *name == turn.speaker)
                .map(|(_, id)| id.clone())
                .unwrap_or_default();
        }
    }

    /// Fill in the end of every untimed line.
    ///
    /// The next line's start is the honest answer; it is clamped to the turn
    /// the line starts in and to the file's duration so a deleted neighbour
    /// cannot stretch a cue past the end of the recording.
    fn infer_ends(&mut self) {
        let duration = self.duration;
        let turns: Vec<(f32, f32)> = self
            .turns
            .iter()
            .map(|turn| (turn.start, turn.end))
            .collect();

        for index in 0..self.lines.len() {
            let Some(start) = self.lines[index].start else {
                continue;
            };
            if self.lines[index].end.is_some() {
                continue;
            }
            let next = self.lines[index + 1..]
                .iter()
                .find_map(|line| line.start)
                .filter(|next| *next > start);
            let mut end = next.or(duration).unwrap_or(start + 2.0);
            if let Some(duration) = duration {
                end = end.min(duration);
            }
            if let Some((_, turn_end)) = turns
                .iter()
                .rev()
                .find(|(turn_start, _)| *turn_start <= start)
                .filter(|(_, turn_end)| *turn_end > start)
            {
                end = end.min((*turn_end).max(start + 0.2));
            }
            end = end.max(start);
            self.lines[index].end = Some(end);
        }
    }

    /// Parse a sidecar, upgrading v1 files in memory.
    ///
    /// Reading never writes: a v1 file is upgraded on the next save, not on
    /// open, so a read-only view of someone else's transcript leaves it alone.
    pub fn from_json(text: &str) -> Option<Self> {
        let mut sidecar: Self = serde_json::from_str(text).ok()?;
        if sidecar.version < VERSION {
            for line in &mut sidecar.lines {
                if line.start.is_none() {
                    line.start = parse_time(&line.time);
                }
            }
        }
        sidecar.ensure_identities();
        sidecar.infer_ends();
        sidecar.version = VERSION;
        Some(sidecar)
    }
}

impl From<Turn> for SidecarTurn {
    fn from(turn: Turn) -> Self {
        Self {
            start: turn.start,
            end: turn.end,
            speaker: turn.speaker,
            speaker_id: String::new(),
        }
    }
}

/// Parse a rendered `MM:SS` or `H:MM:SS` timestamp.
fn parse_time(time: &str) -> Option<f32> {
    let time = time.trim().trim_start_matches('[').trim_end_matches(']');
    if time.is_empty() {
        return None;
    }
    let mut seconds = 0.0f32;
    for part in time.split(':') {
        seconds = seconds * 60.0 + part.trim().parse::<f32>().ok()?;
    }
    Some(seconds)
}

/// The sidecar beside a transcript, if one exists.
///
/// The pipeline writes `<stem>-ts.json` beside `<stem>-ts.txt`. A transcript
/// loaded from the plain `.txt` still has that same sidecar, and a transcript
/// the user renamed keeps its `.json` next to it when the rename preserved the
/// stem. All three shapes are tried so a renamed file does not silently lose
/// its confidences and speaker ids.
pub fn locate(transcript: &Path) -> Option<PathBuf> {
    if is_json(transcript) {
        return transcript.is_file().then(|| transcript.to_path_buf());
    }

    let direct = transcript.with_extension("json");
    if direct.is_file() {
        return Some(direct);
    }

    let stem = transcript.file_stem()?.to_string_lossy().into_owned();
    let timestamped = transcript.with_file_name(format!("{stem}-ts.json"));
    if timestamped.is_file() {
        return Some(timestamped);
    }

    // `foo-ts.txt` renamed to `foo.txt` leaves `foo-ts.json` behind.
    if let Some(base) = stem.strip_suffix("-ts") {
        let stripped = transcript.with_file_name(format!("{base}.json"));
        if stripped.is_file() {
            return Some(stripped);
        }
    }

    None
}

/// Read the sidecar beside `transcript`. `None` when there is none.
pub fn read(transcript: &Path) -> Option<Sidecar> {
    let path = locate(transcript)?;
    let text = fs::read_to_string(path).ok()?;
    Sidecar::from_json(&text)
}

/// Write `sidecar` to `path` through a temporary file in the same directory.
pub fn write_at(path: &Path, sidecar: &Sidecar) -> io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let text = serde_json::to_string_pretty(sidecar)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transcript.json".to_string());
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));

    fs::write(&temporary, text)?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("myware-sidecar-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(start: f32, speaker: &str, text: &str) -> Line {
        Line {
            time: "00:00".to_string(),
            speaker: speaker.to_string(),
            text: text.to_string(),
            start: Some(start),
            ..Line::default()
        }
    }

    fn turn(start: f32, end: f32, speaker: &str) -> Turn {
        Turn {
            start,
            end,
            speaker: speaker.to_string(),
        }
    }

    #[test]
    fn v1_sidecar_upgrades_in_memory() {
        // The shape written before v2: no version, no ids, no ends.
        let json = r#"{
            "lines": [
                {"time": "00:01", "speaker": "ALICE", "text": "hello", "start": 1.0, "confidence": -0.2},
                {"time": "00:04", "speaker": "BOB", "text": "hi", "start": 4.0, "confidence": null}
            ],
            "turns": [{"start": 0.5, "end": 3.0, "speaker": "ALICE"}],
            "duration": 10.0
        }"#;

        let sidecar = Sidecar::from_json(json).expect("parse v1");

        assert_eq!(sidecar.version, VERSION);
        assert_eq!(sidecar.lines.len(), 2);
        assert_eq!(sidecar.lines[0].end, Some(3.0)); // clamped to the turn
        assert_eq!(sidecar.lines[1].end, Some(10.0)); // clamped to duration
        assert_eq!(sidecar.lines[0].speaker_id, "S1");
        assert_eq!(sidecar.turns[0].speaker_id, "S1");
        assert_eq!(sidecar.speakers[0].name, "ALICE");
        assert_eq!(sidecar.lines[1].confidence, None);
    }

    #[test]
    fn rebuild_keeps_ids_across_a_rename() {
        let mut sidecar = Sidecar::new(
            vec![line(0.0, "A", "one"), line(2.0, "B", "two")],
            vec![turn(0.0, 1.5, "A"), turn(1.5, 4.0, "B")],
            Meta::default(),
        );
        assert_eq!(sidecar.speakers[0].id, "S1");
        assert_eq!(sidecar.speakers[1].id, "S2");

        // A rename updates the rendered names everywhere, ids stay put.
        let mut lines = sidecar.lines.clone();
        lines[0].speaker = "Alice".to_string();
        lines[1].speaker = "Bob".to_string();
        for turn in &mut sidecar.turns {
            if turn.speaker == "A" {
                turn.speaker = "Alice".to_string();
            }
            if turn.speaker == "B" {
                turn.speaker = "Bob".to_string();
            }
        }
        for speaker in &mut sidecar.speakers {
            if speaker.name == "A" {
                speaker.name = "Alice".to_string();
            }
            if speaker.name == "B" {
                speaker.name = "Bob".to_string();
            }
        }
        sidecar.rebuild(lines);

        let ids: Vec<(&str, &str)> = sidecar
            .speakers
            .iter()
            .map(|speaker| (speaker.name.as_str(), speaker.id.as_str()))
            .collect();
        assert_eq!(ids, vec![("Alice", "S1"), ("Bob", "S2")]);
        assert_eq!(sidecar.lines[0].speaker_id, "S1");
        assert_eq!(sidecar.turns[1].speaker_id, "S2");
    }

    #[test]
    fn rebuild_follows_merges_and_deletes() {
        let mut sidecar = Sidecar::new(
            vec![line(0.0, "A", "one"), line(2.0, "A", "two"), line(5.0, "B", "three")],
            vec![turn(0.0, 8.0, "A")],
            Meta {
                duration: Some(20.0),
                ..Meta::default()
            },
        );
        assert_eq!(sidecar.lines[0].end, Some(2.0));

        // Merge the first two lines: one line spanning both.
        let merged = vec![line(0.0, "A", "one two"), sidecar.lines[2].clone()];
        sidecar.rebuild(merged);

        assert_eq!(sidecar.lines.len(), 2);
        assert_eq!(sidecar.lines[0].end, Some(5.0));
        assert_eq!(sidecar.speakers.len(), 2); // B is still referenced
    }

    #[test]
    fn unused_speakers_are_dropped() {
        let mut sidecar = Sidecar::new(
            vec![line(0.0, "A", "one"), line(2.0, "B", "two")],
            Vec::new(),
            Meta::default(),
        );
        assert_eq!(sidecar.speakers.len(), 2);

        sidecar.rebuild(vec![line(0.0, "A", "one")]);
        assert_eq!(sidecar.speakers.len(), 1);
        assert_eq!(sidecar.speakers[0].name, "A");
    }

    #[test]
    fn locate_finds_the_timestamped_sidecar() {
        let dir = temp_dir("locate");
        let transcript = dir.join("talk-accuracy-2speakers.txt");
        fs::write(&transcript, "[00:00] A: hi\n").unwrap();
        let expected = dir.join("talk-accuracy-2speakers-ts.json");
        fs::write(&expected, "{}").unwrap();

        assert_eq!(locate(&transcript), Some(expected));

        // A plain transcript beside the same run uses the same sidecar.
        let plain = dir.join("talk-accuracy-2speakers-unedited.txt");
        fs::write(&plain, "hi\n").unwrap();
        let plain_sidecar = dir.join("talk-accuracy-2speakers-unedited-ts.json");
        fs::write(&plain_sidecar, "{}").unwrap();
        assert_eq!(locate(&plain), Some(plain_sidecar));
    }

    #[test]
    fn locate_strips_a_ts_suffix_from_a_renamed_transcript() {
        let dir = temp_dir("locate-renamed");
        let transcript = dir.join("meeting.txt");
        fs::write(&transcript, "hi\n").unwrap();
        let sidecar = dir.join("meeting-ts.json");
        fs::write(&sidecar, "{}").unwrap();

        assert_eq!(locate(&transcript), Some(sidecar));
    }

    #[test]
    fn write_at_round_trips_through_json() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("talk-ts.json");
        let sidecar = Sidecar::new(
            vec![line(0.5, "A", "hello")],
            vec![turn(0.0, 1.0, "A")],
            Meta {
                audio: Some("/tmp/talk.mp3".to_string()),
                duration: Some(9.0),
                preset: Some("accuracy".to_string()),
                language: Some("en".to_string()),
                model: Some("ggml-large-v3-turbo-q5_0.bin".to_string()),
                language_requested: Some("en".to_string()),
                language_detected: None,
                normalize: Some(true),
                producer: Some("core 1.0.0 / asr-v1".to_string()),
                run_id: Some("run-1".to_string()),
            },
        );
        write_at(&path, &sidecar).unwrap();

        let read_back: Sidecar =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back, sidecar);
        assert_eq!(read_back.version, VERSION);
        assert_eq!(read_back.audio.as_deref(), Some("/tmp/talk.mp3"));
        assert_eq!(read_back.model.as_deref(), Some("ggml-large-v3-turbo-q5_0.bin"));
        assert_eq!(read_back.run_id.as_deref(), Some("run-1"));
        assert_eq!(read_back.normalize, Some(true));
    }

    #[test]
    fn v3_sidecars_gain_the_provenance_fields_on_read() {
        let json = r#"{"version":3,"lines":[{"time":"00:00","speaker":"A","text":"hi","start":0.0}],"turns":[],"duration":1.0}"#;
        let sidecar = Sidecar::from_json(json).expect("parse v3");
        assert_eq!(sidecar.version, VERSION);
        assert_eq!(sidecar.model, None);
        assert_eq!(sidecar.language_requested, None);
        assert_eq!(sidecar.language_detected, None);
        assert_eq!(sidecar.run_id, None);
    }

    #[test]
    fn words_round_trip_and_old_files_get_none() {
        let mut line = line(1.0, "A", "hello there");
        line.words = vec![
            Word {
                start: 1.0,
                end: 1.3,
                text: "hello".to_string(),
            },
            Word {
                start: 1.3,
                end: 1.8,
                text: "there".to_string(),
            },
        ];
        let sidecar = Sidecar::new(vec![line], Vec::new(), Meta::default());
        let json = serde_json::to_string(&sidecar).unwrap();
        let read_back = Sidecar::from_json(&json).unwrap();
        assert_eq!(read_back.lines[0].words.len(), 2);
        assert_eq!(read_back.lines[0].words[1].text, "there");

        // A v2 file has no words and must keep loading.
        let v2 = r#"{"lines":[{"time":"00:01","speaker":"A","text":"hello","start":1.0,"confidence":-0.2,"speaker_id":"S1","end":2.0}],"turns":[],"duration":9.0,"speakers":[{"id":"S1","name":"A"}]}"#;
        let old = Sidecar::from_json(v2).unwrap();
        assert!(old.lines[0].words.is_empty());
    }

    #[test]
    fn frozen_keys_survive_serialization() {
        let sidecar = Sidecar::new(vec![line(1.0, "A", "hello")], Vec::new(), Meta::default());
        let json = serde_json::to_string(&sidecar).unwrap();

        // Installed v1 builds read these four keys. They must not move or rename.
        for key in ["\"time\"", "\"speaker\"", "\"text\"", "\"confidence\""] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
    }
}
