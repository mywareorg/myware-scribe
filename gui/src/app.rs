use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, Modifiers};
use serde::{Deserialize, Serialize};
use iced::widget::{
    button, canvas, column, container, horizontal_space, mouse_area, pick_list, progress_bar,
    rich_text, row, scrollable, stack, text, text_editor, text_input, tooltip,
};
use iced::{
    font, mouse, Color, Element, Font, Length, Pixels, Point, Rectangle, Renderer, Shadow, Size,
    Subscription, Task, Theme, Vector,
};
use rodio::{Decoder, OutputStream, Sink};

use crate::backend::{self, Event};

const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "wma", "mp4", "mkv", "mov", "webm", "avi",
];

/// Height of the waveform/speaker timeline strip.
const TIMELINE_HEIGHT: f32 = 72.0;

/// Number of RMS buckets requested when probing an audio file for the timeline.
const TIMELINE_BUCKETS: usize = 2000;

#[derive(Debug, Clone, PartialEq)]
enum ItemStatus {
    Pending,
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
struct QueueItem {
    /// Identity used to match backend events and to de-duplicate additions:
    /// the audio path when one is known, otherwise the transcript path.
    path: PathBuf,
    /// Audio file backing this item (None for transcript-only imports).
    audio: Option<PathBuf>,
    status: ItemStatus,
    duration: Option<f64>,
    /// Output transcript (None while pending, or for a missing file).
    transcript: Option<PathBuf>,
    /// Run statistics for transcripts produced by the pipeline.
    stats: Option<backend::Stats>,
    /// Diarization turns kept for the session; re-derived from the cache after
    /// a restart.
    turns: Vec<backend::Turn>,
    peaks: Arc<Vec<f32>>,
}

/// Persisted queue entry. Runtime-only data (turns, waveform) is re-derived
/// on demand and is intentionally not stored.
#[derive(Serialize, Deserialize)]
struct QueueState {
    #[serde(default)]
    items: Vec<QueueRecord>,
}

#[derive(Serialize, Deserialize)]
struct QueueRecord {
    audio: Option<String>,
    transcript: Option<String>,
    duration: Option<f64>,
    /// "pending" | "running" | "done" | "failed"
    #[serde(default)]
    status: String,
    message: Option<String>,
    stats: Option<backend::Stats>,
    /// Downsampled RMS waveform, so completed items render their timeline
    /// immediately after a restart.
    #[serde(default)]
    peaks: Vec<f32>,
}

/// A completed item's content while it is being viewed.
struct ItemView {
    index: usize,
    /// The file the transcript was loaded from (txt or json).
    source: PathBuf,
    transcript: String,
    lines: Vec<DocLine>,
    turns: Vec<backend::Turn>,
    peaks: Arc<Vec<f32>>,
    duration: Option<f32>,
    audio: Option<PathBuf>,
    /// Where edits are written; None means the view is read-only.
    output: Option<PathBuf>,
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.iter().any(|ext| e.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

fn audio_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_audio(path))
        .collect();
    files.sort();
    files
}

/// A single rendered transcript line.
#[derive(Debug, Clone)]
struct DocLine {
    raw: String,
    speaker: String,
    start: Option<f32>,
}

/// Right-click line editor: the line index plus its editable fields.
struct LineEdit {
    index: usize,
    speaker: String,
    content: text_editor::Content,
}

/// Cadence at which streamed utterances are revealed in the textbox.
const REVEAL_INTERVAL_S: f32 = 0.15;
/// Faster cadence used to catch up when the reveal backlog grows.
const REVEAL_CATCHUP_S: f32 = 0.04;
const REVEAL_BACKLOG: usize = 8;

/// Speaker-count picker options (a fixed 1..=5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakerOpt {
    One,
    Two,
    Three,
    Four,
    Five,
}

impl SpeakerOpt {
    const ALL: [SpeakerOpt; 5] = [
        SpeakerOpt::One,
        SpeakerOpt::Two,
        SpeakerOpt::Three,
        SpeakerOpt::Four,
        SpeakerOpt::Five,
    ];

    fn value(self) -> i32 {
        match self {
            SpeakerOpt::One => 1,
            SpeakerOpt::Two => 2,
            SpeakerOpt::Three => 3,
            SpeakerOpt::Four => 4,
            SpeakerOpt::Five => 5,
        }
    }

    fn from_value(value: Option<i32>) -> Self {
        match value {
            Some(1) => SpeakerOpt::One,
            Some(3) => SpeakerOpt::Three,
            Some(4) => SpeakerOpt::Four,
            Some(5) => SpeakerOpt::Five,
            _ => SpeakerOpt::Two,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SpeakerOpt::One => "1",
            SpeakerOpt::Two => "2",
            SpeakerOpt::Three => "3",
            SpeakerOpt::Four => "4",
            SpeakerOpt::Five => "5",
        }
    }
}

impl fmt::Display for SpeakerOpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Playback speed multiplier options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Half,
    One,
    OneHalf,
    Two,
}

impl Speed {
    const ALL: [Speed; 4] = [Speed::Half, Speed::One, Speed::OneHalf, Speed::Two];

    fn factor(self) -> f32 {
        match self {
            Speed::Half => 0.5,
            Speed::One => 1.0,
            Speed::OneHalf => 1.5,
            Speed::Two => 2.0,
        }
    }

    fn from_factor(factor: f32) -> Self {
        if (factor - 0.5).abs() < 0.01 {
            Speed::Half
        } else if (factor - 1.5).abs() < 0.01 {
            Speed::OneHalf
        } else if (factor - 2.0).abs() < 0.01 {
            Speed::Two
        } else {
            Speed::One
        }
    }

    fn label(self) -> &'static str {
        match self {
            Speed::Half => ".5x",
            Speed::One => "1x",
            Speed::OneHalf => "1.5x",
            Speed::Two => "2x",
        }
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Options in the "Import…" dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    Transcription,
    Audio,
}

impl ImportKind {
    const ALL: [ImportKind; 2] = [ImportKind::Transcription, ImportKind::Audio];

    fn label(self) -> &'static str {
        match self {
            ImportKind::Transcription => "Transcription",
            ImportKind::Audio => "Audio",
        }
    }
}

impl fmt::Display for ImportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Options in the "Help…" dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpKind {
    About,
    Shortcuts,
    Console,
}

impl HelpKind {
    const ALL: [HelpKind; 3] = [HelpKind::About, HelpKind::Shortcuts, HelpKind::Console];

    fn label(self) -> &'static str {
        match self {
            HelpKind::About => "About",
            HelpKind::Shortcuts => "Shortcuts",
            HelpKind::Console => "Console",
        }
    }
}

impl fmt::Display for HelpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    #[default]
    Speed,
    Accuracy,
}

impl Preset {
    pub const ALL: [Preset; 2] = [Preset::Speed, Preset::Accuracy];

    fn label(self) -> &'static str {
        match self {
            Preset::Speed => "Speed",
            Preset::Accuracy => "Accuracy",
        }
    }

    fn arg(self) -> &'static str {
        match self {
            Preset::Speed => "speed",
            Preset::Accuracy => "accuracy",
        }
    }

    fn from_arg(value: &str) -> Option<Preset> {
        match value {
            "speed" => Some(Preset::Speed),
            // Legacy saved setting: the old "balanced" now maps to accuracy.
            "balanced" | "accuracy" => Some(Preset::Accuracy),
            _ => None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Settings {
    preset: Option<String>,
    sms_mode: Option<bool>,
    num_speakers: Option<i32>,
    font_scale: Option<f32>,
    playback_speed: Option<f32>,
    waveform_zoom: Option<f32>,
    follow_playhead: Option<bool>,
    window_width: Option<f32>,
    window_height: Option<f32>,
    window_x: Option<f32>,
    window_y: Option<f32>,
    output_dir: Option<String>,
}

struct Scheme {
    panel_bg: Color,
    dialog_bg: Color,
    track: Color,
    bar_track: Color,
    timeline_bg: Color,
    blue: Color,
    pink: Color,
    green: Color,
    ink: Color,
    text: Color,
    sms0: Color,
    sms1: Color,
    warn_tint: Color,
    search_hl: Color,
    search_active: Color,
    tip_bg: Color,
    tip_text: Color,
}

impl Scheme {
    fn light() -> Scheme {
        Scheme {
            panel_bg: Color::from_rgb(0.96, 0.97, 0.995),
            dialog_bg: Color::from_rgb(0.99, 0.99, 1.0),
            track: Color::from_rgb(0.88, 0.88, 0.93),
            bar_track: Color::from_rgb(0.88, 0.88, 0.93),
            timeline_bg: Color::from_rgb(0.88, 0.88, 0.93),
            blue: Color::from_rgb(0.357, 0.808, 0.980),
            pink: Color::from_rgb(0.961, 0.663, 0.722),
            green: Color::from_rgb(0.30, 0.72, 0.42),
            ink: Color::from_rgb(0.13, 0.13, 0.17),
            text: Color::from_rgb(0.13, 0.13, 0.17),
            sms0: Color::from_rgb(0.357, 0.808, 0.980),
            sms1: Color::from_rgb(0.961, 0.663, 0.722),
            warn_tint: Color::from_rgba(0.529, 0.867, 1.0, 0.35),
            search_hl: Color::from_rgba(1.0, 0.85, 0.3, 0.5),
            search_active: Color::from_rgba(1.0, 0.72, 0.2, 0.8),
            tip_bg: Color::from_rgb(0.99, 0.99, 1.0),
            tip_text: Color::from_rgb(0.13, 0.13, 0.17),
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone)]
enum Status {
    Idle,
    Running,
    Done,
    Cancelled,
    Error(String),
}

/// One line in the completion summary: a file name (styled prominently) or
/// ordinary statistics text.
#[derive(Debug, Clone)]
enum SummaryLine {
    File(String),
    Body(String),
}

#[derive(Debug, Clone)]
pub enum Message {
    PickOutput,
    OutputPicked(Option<PathBuf>),
    PresetChanged(Preset),
    Start,
    Cancel,
    Tick,
    RenameName(usize, String),
    RenameSave,
    RenameCancel,
    RenameMove(bool),
    EnterSubmit,
    ShiftEnterSubmit,
    SelectLine(usize),
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFollowPlayhead,
    DismissToast,
    WaveZoom(f32),
    HelpChoice(HelpKind),
    CloseHelp,
    Noop,
    OpenSummary,
    SmsToggled(bool),
    SpeakerCountChanged(SpeakerOpt),
    TranscriptScrolled(scrollable::Viewport),
    WindowEvent(iced::window::Event),
    Seek(f32),
    PlayerToggle,
    FocusFind,
    SearchChanged(String),
    FindPrev,
    FindNext,
    SpeedChanged(Speed),
    Escape,
    ShortcutPlay,
    SeekBy(f32),
    ImportPicked(Option<PathBuf>),
    ImportLoaded(Option<(PathBuf, String)>),
    ImportChoice(ImportKind),
    CopyTranscript,
    SaveTranscript,
    RevealOutput,
    AudioPicked(Option<PathBuf>),
    OpenRename,
    EditLine(usize),
    LineEditSpeaker(String),
    LineEditAction(text_editor::Action),
    SaveLineEdit,
    CancelLineEdit,
    DeleteLineEdit,
    CloseSummary,
    AddFiles,
    FilesPicked(Option<Vec<PathBuf>>),
    AddFolder,
    FolderPicked(Option<PathBuf>),
    RemoveItem(usize),
    ClearQueue,
    ItemDurationProbed(PathBuf, Option<f64>),
    ViewItem(usize),
    ViewDetailsLoaded(
        usize,
        PathBuf,
        Option<Vec<backend::Turn>>,
        Option<(f64, Vec<f32>)>,
    ),
    QueueWaveformLoaded(PathBuf, Option<(f64, Vec<f32>)>),
    RequeueItem(usize),
}

pub struct App {
    output_dir: Option<PathBuf>,
    preset: Preset,
    stage: String,
    progress: f32,
    progress_shown: f32,
    overall_shown: f32,
    run_start: Option<Instant>,
    elapsed_final: Option<f64>,
    status: Status,
    transcript: String,
    lines: Vec<DocLine>,
    pending_lines: VecDeque<DocLine>,
    reveal_accum: f32,
    turns: Vec<backend::Turn>,
    peaks: Arc<Vec<f32>>,
    peaks_for: Option<PathBuf>,
    playhead: f32,
    timeline_duration: Option<f32>,
    player: Option<Player>,
    output_txt: Option<PathBuf>,
    summary_open: bool,
    pending_rename: bool,
    completion_pending: bool,
    rename_open: bool,
    rename_fields: Vec<(String, String)>,
    rename_focus: usize,
    sms_mode: bool,
    num_speakers: Option<i32>,
    transcript_viewport: f32,
    transcript_content: f32,
    transcript_at_bottom: bool,
    transcript_dirty: bool,
    follow_offset: Option<f32>,
    /// Horizontal target of the last programmatic transcript scroll, used to
    /// distinguish it from a user scroll.
    scroll_target: Option<f32>,
    /// True after the user scroll-seeks the live transcript: the highlight then
    /// follows the playhead instead of the newest streamed line.
    scroll_seek: bool,
    font_scale: f32,
    follow_playhead: bool,
    console_open: bool,
    console_lines: Vec<String>,
    toast: Option<(String, Instant)>,
    help_open: Option<HelpKind>,
    search_query: String,
    search_index: usize,
    playback_speed: f32,
    last_tick: Option<Instant>,
    activity_frame: usize,
    window_size: Size,
    window_position: Option<Point>,
    window_dirty: bool,
    last_window_change: Option<Instant>,
    waveform_zoom: f32,
    events: Arc<Mutex<VecDeque<Event>>>,
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
    queue: Vec<QueueItem>,
    batch: bool,
    current: Option<usize>,
    current_index: Option<usize>,
    /// Index of the completed queue item currently shown, if any.
    selected: Option<usize>,
    selected_view: Option<ItemView>,
    /// Error shown in the transcript pane when a completed item cannot load.
    view_error: Option<String>,
    /// Right-clicked line currently being edited.
    line_edit: Option<LineEdit>,
    /// Background waveform backfill for completed items without cached peaks.
    backfill_active: bool,
    waveform_tried: HashSet<PathBuf>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let settings = Self::load_settings();
        Self {
            output_dir: settings.output_dir.as_ref().map(PathBuf::from),
            preset: settings
                .preset
                .as_deref()
                .and_then(Preset::from_arg)
                .unwrap_or_default(),
            stage: "idle".to_string(),
            progress: 0.0,
            progress_shown: 0.0,
            overall_shown: 0.0,
            run_start: None,
            elapsed_final: None,
            status: Status::Idle,
            transcript: String::new(),
            lines: Vec::new(),
            pending_lines: VecDeque::new(),
            reveal_accum: 0.0,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
            peaks_for: None,
            playhead: 0.0,
            timeline_duration: None,
            player: None,
            output_txt: None,
            summary_open: false,
            pending_rename: false,
            completion_pending: false,
            rename_open: false,
            rename_fields: Vec::new(),
            rename_focus: 0,
            sms_mode: settings.sms_mode.unwrap_or(false),
            num_speakers: match settings.num_speakers {
                None => Some(2),
                Some(0) => None,
                Some(count) => Some(count),
            },
            transcript_viewport: 0.0,
            transcript_content: 0.0,
            transcript_at_bottom: true,
            transcript_dirty: false,
            follow_offset: None,
            scroll_target: None,
            scroll_seek: false,
            font_scale: settings.font_scale.unwrap_or(1.0),
            follow_playhead: settings.follow_playhead.unwrap_or(true),
            console_open: false,
            console_lines: Vec::new(),
            toast: None,
            help_open: None,
            waveform_zoom: settings.waveform_zoom.unwrap_or(1.0),
            search_query: String::new(),
            search_index: 0,
            playback_speed: settings.playback_speed.unwrap_or(1.0),
            last_tick: None,
            activity_frame: 0,
            window_size: Self::window_size_from(&settings),
            window_position: match (settings.window_x, settings.window_y) {
                (Some(x), Some(y)) => Some(Point::new(x, y)),
                _ => None,
            },
            window_dirty: false,
            last_window_change: None,
            events: Arc::new(Mutex::new(VecDeque::new())),
            pid: Arc::new(AtomicU32::new(0)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue: Self::load_queue(),
            batch: false,
            current: None,
            current_index: None,
            selected: None,
            selected_view: None,
            view_error: None,
            line_edit: None,
            backfill_active: false,
            waveform_tried: HashSet::new(),
        }
    }

    fn settings_path() -> Option<PathBuf> {
        if cfg!(windows) {
            std::env::var("APPDATA")
                .ok()
                .map(|base| PathBuf::from(base).join("transiel").join("settings.json"))
        } else {
            let base = std::env::var("XDG_CONFIG_HOME")
                .ok()
                .or_else(|| std::env::var("HOME").ok().map(|home| format!("{home}/.config")))?;
            Some(PathBuf::from(base).join("transiel").join("settings.json"))
        }
    }

    fn load_settings() -> Settings {
        Self::settings_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_settings(&self) {
        let Some(path) = Self::settings_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let settings = Settings {
            preset: Some(self.preset.arg().to_string()),
            sms_mode: Some(self.sms_mode),
            num_speakers: Some(self.num_speakers.unwrap_or(0)),
            font_scale: Some(self.font_scale),
            playback_speed: Some(self.playback_speed),
            waveform_zoom: Some(self.waveform_zoom),
            follow_playhead: Some(self.follow_playhead),
            window_width: Some(self.window_size.width),
            window_height: Some(self.window_size.height),
            window_x: self.window_position.map(|point| point.x),
            window_y: self.window_position.map(|point| point.y),
            output_dir: self
                .output_dir
                .as_ref()
                .map(|path| path.display().to_string()),
        };
        if let Ok(text) = serde_json::to_string_pretty(&settings) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Queue state lives beside the settings file.
    fn queue_path() -> Option<PathBuf> {
        Self::settings_path().map(|path| path.with_file_name("queue.json"))
    }

    fn load_queue() -> Vec<QueueItem> {
        let Some(path) = Self::queue_path() else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let Ok(state) = serde_json::from_str::<QueueState>(&text) else {
            return Vec::new();
        };
        state
            .items
            .into_iter()
            .filter_map(|record| {
                let audio = record.audio.map(PathBuf::from);
                let transcript = record.transcript.map(PathBuf::from);
                let path = audio.clone().or_else(|| transcript.clone())?;
                let mut status = match record.status.as_str() {
                    // A run interrupted by a restart is simply pending again.
                    "running" => ItemStatus::Pending,
                    "failed" => ItemStatus::Failed(
                        record.message.unwrap_or_else(|| "failed".to_string()),
                    ),
                    "done" => ItemStatus::Done,
                    _ => ItemStatus::Pending,
                };
                if matches!(status, ItemStatus::Pending) && !path.is_file() {
                    status = ItemStatus::Failed("file not found".to_string());
                }
                Some(QueueItem {
                    path,
                    audio,
                    status,
                    duration: record.duration,
                    transcript,
                    stats: record.stats,
                    turns: Vec::new(),
                    peaks: Arc::new(record.peaks),
                })
            })
            .collect()
    }

    fn save_queue(&self) {
        let Some(path) = Self::queue_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let state = QueueState {
            items: self
                .queue
                .iter()
                .map(|item| QueueRecord {
                    audio: item.audio.as_ref().map(|path| path.display().to_string()),
                    transcript: item.transcript.as_ref().map(|path| path.display().to_string()),
                    duration: item.duration,
                    status: match &item.status {
                        ItemStatus::Pending => "pending",
                        ItemStatus::Running => "running",
                        ItemStatus::Done => "done",
                        ItemStatus::Failed(_) => "failed",
                    }
                    .to_string(),
                    message: match &item.status {
                        ItemStatus::Failed(message) => Some(message.clone()),
                        _ => None,
                    },
                    stats: item.stats.clone(),
                    peaks: item.peaks.as_ref().clone(),
                })
                .collect(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(path, text);
        }
    }

    // ---- displayed view (live run vs. a selected completed item) ----

    fn shown_lines(&self) -> &[DocLine] {
        match &self.selected_view {
            Some(view) => &view.lines,
            None => &self.lines,
        }
    }

    fn shown_turns(&self) -> &[backend::Turn] {
        match &self.selected_view {
            Some(view) => &view.turns,
            None => &self.turns,
        }
    }

    fn shown_peaks(&self) -> &[f32] {
        match &self.selected_view {
            Some(view) => &view.peaks,
            None => &self.peaks,
        }
    }

    fn shown_peaks_arc(&self) -> Arc<Vec<f32>> {
        match &self.selected_view {
            Some(view) => view.peaks.clone(),
            None => self.peaks.clone(),
        }
    }

    fn shown_duration(&self) -> Option<f32> {
        match &self.selected_view {
            Some(view) => view.duration,
            None => self.timeline_duration,
        }
    }

    fn shown_audio(&self) -> Option<&Path> {
        match &self.selected_view {
            Some(view) => view.audio.as_deref(),
            None => self.peaks_for.as_deref(),
        }
    }

    fn shown_output(&self) -> Option<&Path> {
        match &self.selected_view {
            Some(view) => view.output.as_deref(),
            None => self.output_txt.as_deref(),
        }
    }

    fn shown_transcript(&self) -> &str {
        match &self.selected_view {
            Some(view) => &view.transcript,
            None => &self.transcript,
        }
    }

    /// True when the queue holds at least one completed transcript.
    fn has_results(&self) -> bool {
        self.queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Done) && item.transcript.is_some())
    }

    /// Audio file name of the item currently being transcribed.
    fn current_file_name(&self) -> Option<String> {
        if !matches!(self.status, Status::Running) {
            return None;
        }
        let index = self.current.or(self.current_index)?;
        let item = self.queue.get(index)?;
        let path = item.audio.as_deref().unwrap_or(item.path.as_path());
        let name = path.file_name().and_then(|name| name.to_str())?;
        // Keep the progress label on one line for very long file names.
        const MAX: usize = 56;
        if name.chars().count() <= MAX {
            Some(name.to_string())
        } else {
            let mut short: String = name.chars().take(MAX - 1).collect();
            short.push('\u{2026}');
            Some(short)
        }
    }

    /// Playback is available for any shown transcript with audio, except the
    /// live file while it is still being transcribed.
    fn player_ready(&self) -> bool {
        self.shown_audio().is_some()
            && !(self.selected_view.is_none() && matches!(self.status, Status::Running))
    }

    /// Keep a pinned completed view across a file transition when its audio is
    /// playing or has just finished.
    fn keep_pinned_view(&self) -> bool {
        self.selected_view.is_some()
            && self
                .player
                .as_ref()
                .is_some_and(|player| player.is_playing() || player.finished())
    }

    fn completed_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|item| matches!(item.status, ItemStatus::Done))
            .count()
    }

    // ---- completed-item selection ----

    /// Open a completed queue item (or return to the live run when the running
    /// row is clicked).
    fn view_item(&mut self, index: usize) -> Task<Message> {
        // Clicking the row that is already on screen (e.g. while its audio is
        // playing) is not a view change.
        if self.selected == Some(index) && self.selected_view.is_some() {
            return Task::none();
        }
        self.open_item_view(index, None)
    }

    fn open_item_view(
        &mut self,
        index: usize,
        preloaded: Option<(String, Option<PathBuf>)>,
    ) -> Task<Message> {
        let Some(item) = self.queue.get(index) else {
            return Task::none();
        };
        if matches!(item.status, ItemStatus::Running) {
            self.return_to_live();
            return Task::none();
        }
        if !matches!(item.status, ItemStatus::Done) {
            return Task::none();
        }
        let Some(source) = item.transcript.clone() else {
            return Task::none();
        };
        let audio = item.audio.clone();
        let duration = item.duration.map(|seconds| seconds as f32);
        let turns = item.turns.clone();
        let peaks = item.peaks.clone();

        self.player = None;
        self.playhead = 0.0;
        self.last_tick = None;
        self.search_query.clear();
        self.search_index = 0;
        self.follow_offset = None;
        self.scroll_seek = false;
        self.transcript_viewport = 0.0;
        self.transcript_content = 0.0;
        self.transcript_at_bottom = true;

        let loaded = match preloaded {
            Some((text, output)) => Some((text, output)),
            None => match std::fs::read_to_string(&source) {
                Ok(text) => {
                    // JSON imports are displayed read-only; only plain .txt
                    // transcripts are safe to edit in place.
                    let editable = source
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("txt"))
                        .unwrap_or(false);
                    Some((text, editable.then(|| source.clone())))
                }
                Err(error) => {
                    self.selected = Some(index);
                    self.selected_view = Some(ItemView {
                        index,
                        source: source.clone(),
                        transcript: String::new(),
                        lines: Vec::new(),
                        turns: Vec::new(),
                        peaks: Arc::new(Vec::new()),
                        duration,
                        audio,
                        output: None,
                    });
                    self.view_error =
                        Some(format!("Could not read {}: {error}", source.display()));
                    return scrollable::snap_to(
                        transcript_id(),
                        scrollable::RelativeOffset { x: 0.0, y: 0.0 },
                    );
                }
            },
        };
        let Some((text, output)) = loaded else {
            return Task::none();
        };

        let lines = parse_lines(&text);
        self.view_error = None;
        self.selected = Some(index);
        self.selected_view = Some(ItemView {
            index,
            source: source.clone(),
            transcript: text,
            lines,
            turns,
            peaks,
            duration,
            audio: audio.clone(),
            output,
        });

        self.scroll_target = Some(0.0);
        let mut tasks = vec![scrollable::snap_to(
            transcript_id(),
            scrollable::RelativeOffset { x: 0.0, y: 0.0 },
        )];

        // A persisted waveform (and duration) renders immediately; only the
        // speaker turns still need to be re-derived from the diarization cache.
        let needs_probe = self
            .selected_view
            .as_ref()
            .is_some_and(|view| view.peaks.is_empty() || view.duration.unwrap_or(0.0) <= 0.0);
        let needs_turns = self
            .selected_view
            .as_ref()
            .is_some_and(|view| view.turns.is_empty());
        if let Some(audio) = audio {
            let num_speakers = self.queue[index]
                .stats
                .as_ref()
                .and_then(|stats| stats.num_speakers);
            if needs_probe {
                tasks.push(Task::perform(
                    async move { backend::load_completed(audio, num_speakers, TIMELINE_BUCKETS) },
                    move |(turns, probe)| {
                        Message::ViewDetailsLoaded(index, source.clone(), turns, probe)
                    },
                ));
            } else if needs_turns {
                tasks.push(Task::perform(
                    async move { backend::load_turns(&audio, num_speakers) },
                    move |turns| {
                        Message::ViewDetailsLoaded(index, source.clone(), turns, None)
                    },
                ));
            }
        }
        Task::batch(tasks)
    }

    /// Drop a pinned completed-item view and show the live run again.
    fn return_to_live(&mut self) {
        self.selected = None;
        self.selected_view = None;
        self.view_error = None;
        self.player = None;
        self.playhead = 0.0;
        self.last_tick = None;
        self.follow_offset = None;
        self.scroll_target = None;
        self.scroll_seek = false;
        self.search_index = 0;
    }

    /// Find an audio file beside the transcript with the same stem.
    fn matching_audio(transcript: &Path) -> Option<PathBuf> {
        let parent = transcript.parent()?;
        let stem = transcript.file_stem()?.to_str()?;
        let mut candidates: Vec<PathBuf> = std::fs::read_dir(parent)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && is_audio(path)
                    && path
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.eq_ignore_ascii_case(stem))
            })
            .collect();
        candidates.sort();
        candidates.into_iter().next()
    }

    /// Window settings seeded from the persisted size/position.
    pub fn window_settings() -> iced::window::Settings {
        let settings = Self::load_settings();
        let position = match (settings.window_x, settings.window_y) {
            (Some(x), Some(y)) => iced::window::Position::Specific(Point::new(x, y)),
            _ => iced::window::Position::Default,
        };
        iced::window::Settings {
            size: Self::window_size_from(&settings),
            position,
            min_size: Some(Size::new(640.0, 480.0)),
            icon: Self::window_icon(),
            ..iced::window::Settings::default()
        }
    }

    /// Title-bar / taskbar icon decoded from the bundled PNG.
    fn window_icon() -> Option<iced::window::Icon> {
        let image = image::load_from_memory(include_bytes!("../assets/icon.png"))
            .ok()?
            .into_rgba8();
        let (width, height) = image.dimensions();
        iced::window::icon::from_rgba(image.into_raw(), width, height).ok()
    }

    fn window_size_from(settings: &Settings) -> Size {
        // Cap the height so the window never opens taller than common screens.
        const MAX_HEIGHT: f32 = 1000.0;
        match (settings.window_width, settings.window_height) {
            (Some(width), Some(height)) if width >= 100.0 && height >= 100.0 => {
                Size::new(width, height.min(MAX_HEIGHT))
            }
            _ => Size::new(640.0, 900.0),
        }
    }

    pub fn theme(&self) -> iced::Theme {
        iced::Theme::custom(
            "Transgender".to_string(),
            iced::theme::Palette {
                background: Color::WHITE,
                text: Color::from_rgb(0.13, 0.13, 0.17),
                primary: Color::from_rgb(0.357, 0.808, 0.980),
                success: Color::from_rgb(0.357, 0.808, 0.980),
                danger: Color::from_rgb(0.961, 0.663, 0.722),
            },
        )
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
            keyboard::on_key_press(Self::key_press),
            iced::window::events().map(|(_id, event)| Message::WindowEvent(event)),
        ])
    }

    fn key_press(key: Key, modifiers: Modifiers) -> Option<Message> {
        use keyboard::key::Named;
        match key {
            Key::Named(Named::Tab) => Some(Message::RenameMove(true)),
            Key::Named(Named::Enter) if modifiers.command() => Some(Message::Start),
            Key::Named(Named::Enter) if modifiers.shift() => Some(Message::ShiftEnterSubmit),
            Key::Named(Named::Enter) => Some(Message::EnterSubmit),
            Key::Named(Named::Escape) => Some(Message::Escape),
            Key::Named(Named::Space) => Some(Message::ShortcutPlay),
            Key::Named(Named::ArrowRight) => Some(Message::SeekBy(5.0)),
            Key::Named(Named::ArrowLeft) => Some(Message::SeekBy(-5.0)),
            Key::Character(character) if modifiers.command() => match character.as_str() {
                "o" => Some(Message::AddFiles),
                "f" => Some(Message::FocusFind),
                "=" | "+" => Some(Message::ZoomIn),
                "-" | "_" => Some(Message::ZoomOut),
                "0" => Some(Message::ZoomReset),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SmsToggled(value) => {
                self.sms_mode = value;
                self.save_settings();
                Task::none()
            }
            Message::SpeakerCountChanged(option) => {
                if matches!(self.status, Status::Running) {
                    return Task::none();
                }
                self.num_speakers = Some(option.value());
                self.save_settings();
                Task::none()
            }
            Message::TranscriptScrolled(viewport) => {
                self.transcript_viewport = viewport.bounds().height;
                self.transcript_content = viewport.content_bounds().height;
                let offset = viewport.absolute_offset().y;
                let viewport_h = viewport.bounds().height;
                let content_h = viewport.content_bounds().height;
                self.transcript_at_bottom = offset + viewport_h >= content_h - 8.0;

                let following = self.follow_playhead;
                let follows = following
                    && self
                        .follow_offset
                        .is_some_and(|expected| (offset - expected).abs() <= 6.0);
                let programmatic = self
                    .scroll_target
                    .take()
                    .is_some_and(|target| (offset - target).abs() <= 6.0);

                // A scroll that doesn't match follow/programmatic targets is a
                // user scroll: stop auto-following and move the playhead to the
                // line now in view.
                if following && !follows {
                    self.follow_playhead = false;
                    self.follow_offset = None;
                    self.save_settings();
                }
                if !follows && !programmatic {
                    self.seek_from_scroll(offset, viewport_h);
                }
                Task::none()
            }
            Message::WindowEvent(event) => {
                match event {
                    iced::window::Event::Resized(size) => {
                        self.window_size = size;
                        self.window_dirty = true;
                        self.last_window_change = Some(Instant::now());
                    }
                    iced::window::Event::Moved(point) => {
                        self.window_position = Some(point);
                        self.window_dirty = true;
                        self.last_window_change = Some(Instant::now());
                    }
                    _ => {}
                }
                Task::none()
            }
            Message::ViewDetailsLoaded(index, source, turns, probe) => {
                let mut changed = false;
                if let Some(view) = self.selected_view.as_mut() {
                    if view.index == index && view.source == source {
                        if let Some(turns) = turns {
                            view.turns = turns;
                        }
                        if let Some((duration, peaks)) = probe {
                            if duration > 0.0 {
                                view.duration = Some(duration as f32);
                            }
                            if !peaks.is_empty() {
                                view.peaks = Arc::new(peaks);
                            }
                        }
                        changed = true;
                    }
                }
                if changed {
                    if let Some(view) = &self.selected_view {
                        if let Some(item) = self.queue.get_mut(index) {
                            item.turns = view.turns.clone();
                            item.peaks = view.peaks.clone();
                            item.duration = view.duration.map(|seconds| seconds as f64);
                        }
                    }
                    self.save_queue();
                }
                Task::none()
            }
            Message::QueueWaveformLoaded(path, probe) => {
                self.backfill_active = false;
                if let Some((duration, peaks)) = probe {
                    let peaks = Arc::new(peaks);
                    if let Some(item) = self
                        .queue
                        .iter_mut()
                        .find(|item| item.audio.as_deref() == Some(path.as_path()))
                    {
                        item.duration = Some(duration);
                        item.peaks = peaks.clone();
                    }
                    if let Some(view) = self.selected_view.as_mut() {
                        if view.audio.as_deref() == Some(path.as_path()) {
                            view.duration = Some(duration as f32);
                            view.peaks = peaks;
                        }
                    }
                    self.save_queue();
                }
                self.backfill_next_waveform()
            }
            Message::ViewItem(index) => self.view_item(index),
            Message::RequeueItem(index) => {
                if index >= self.queue.len() || self.queue[index].audio.is_none() {
                    self.toast = Some(("No audio file to transcribe".to_string(), Instant::now()));
                    return Task::none();
                }
                let item = &mut self.queue[index];
                item.status = ItemStatus::Pending;
                item.transcript = None;
                item.stats = None;
                item.turns.clear();
                item.peaks = Arc::new(Vec::new());
                self.selected = None;
                self.selected_view = None;
                self.view_error = None;
                self.save_queue();
                self.toast = Some(("Queued for transcription".to_string(), Instant::now()));
                if !matches!(self.status, Status::Running) {
                    self.begin_batch(false);
                }
                Task::none()
            }
            Message::Seek(seconds) => {
                let max = self.shown_duration().unwrap_or(0.0).max(0.0);
                self.playhead = seconds.clamp(0.0, max);
                self.last_tick = Some(Instant::now());
                if let Some(player) = &self.player {
                    player.seek(self.playhead, self.playback_speed);
                }
                Task::none()
            }
            Message::PlayerToggle => {
                self.toggle_playback();
                Task::none()
            }
            Message::FocusFind => text_input::focus(find_id()),
            Message::SearchChanged(query) => {
                self.search_query = query;
                self.search_index = 0;
                Task::none()
            }
            Message::FindNext => {
                let matches = self.matching_lines();
                if !matches.is_empty() {
                    self.search_index = (self.search_index + 1) % matches.len();
                    return self.scroll_to_line(matches[self.search_index]);
                }
                Task::none()
            }
            Message::FindPrev => {
                let matches = self.matching_lines();
                if !matches.is_empty() {
                    self.search_index = (self.search_index + matches.len() - 1) % matches.len();
                    return self.scroll_to_line(matches[self.search_index]);
                }
                Task::none()
            }
            Message::SpeedChanged(speed) => {
                self.playback_speed = speed.factor();
                if let Some(player) = &self.player {
                    player.set_speed(self.playback_speed);
                }
                Task::none()
            }
            Message::SelectLine(index) => {
                if let Some(line) = self.shown_lines().get(index) {
                    if let Some(start) = line.start {
                        let max = self.shown_duration().unwrap_or(start);
                        self.playhead = start.clamp(0.0, max);
                        self.follow_offset = None;
                        self.scroll_seek = true;
                        if let Some(player) = &self.player {
                            player.seek(self.playhead, self.playback_speed);
                        }
                    }
                }
                Task::none()
            }
            Message::RenameName(index, value) => {
                if let Some(field) = self.rename_fields.get_mut(index) {
                    field.1 = value;
                }
                Task::none()
            }
            Message::RenameMove(forward) => {
                if !self.rename_open || self.rename_fields.is_empty() {
                    return Task::none();
                }
                let fields = self.rename_fields.len();
                let total = fields + 2; // fields, then Save, then Cancel
                self.rename_focus = if forward {
                    (self.rename_focus + 1) % total
                } else {
                    (self.rename_focus + total - 1) % total
                };
                if self.rename_focus < fields {
                    text_input::focus(rename_id(self.rename_focus))
                } else {
                    Task::none()
                }
            }
            Message::EnterSubmit => {
                if self.line_edit.is_some() {
                    // Enter saves the line editor from any of its fields.
                    self.apply_line_edit();
                    Task::none()
                } else if self.rename_open {
                    self.apply_rename();
                    Task::none()
                } else {
                    let matches = self.matching_lines();
                    if !matches.is_empty() {
                        self.search_index = (self.search_index + 1) % matches.len();
                        return self.scroll_to_line(matches[self.search_index]);
                    }
                    Task::none()
                }
            }
            Message::ShiftEnterSubmit => {
                if self.line_edit.is_some() {
                    return Task::none();
                } else if !self.rename_open {
                    let matches = self.matching_lines();
                    if !matches.is_empty() {
                        self.search_index = (self.search_index + matches.len() - 1) % matches.len();
                        return self.scroll_to_line(matches[self.search_index]);
                    }
                }
                Task::none()
            }
            Message::ZoomIn => {
                self.font_scale = (self.font_scale + 0.1).min(2.5);
                self.save_settings();
                Task::none()
            }
            Message::ZoomOut => {
                self.font_scale = (self.font_scale - 0.1).max(0.6);
                self.save_settings();
                Task::none()
            }
            Message::ZoomReset => {
                self.font_scale = 1.0;
                self.save_settings();
                Task::none()
            }
            Message::ToggleFollowPlayhead => {
                let turning_off = self.follow_playhead;
                self.follow_playhead = !self.follow_playhead;
                // Turning Follow off while the live transcript is streaming:
                // keep the playhead on the newest line so the highlight stays
                // where the text is.
                if turning_off
                    && self.selected_view.is_none()
                    && matches!(self.status, Status::Running)
                {
                    if let Some(last) = self.display_order().last().copied() {
                        if let Some(start) =
                            self.shown_lines().get(last).and_then(|line| line.start)
                        {
                            self.playhead = start;
                        }
                    }
                }
                self.follow_offset = None;
                self.scroll_target = None;
                self.scroll_seek = false;
                self.save_settings();
                Task::none()
            }
            Message::DismissToast => {
                self.toast = None;
                Task::none()
            }
            Message::WaveZoom(factor) => {
                self.waveform_zoom = factor.clamp(1.0, 16.0);
                self.save_settings();
                Task::none()
            }
            Message::HelpChoice(kind) => {
                if matches!(kind, HelpKind::Console) {
                    self.console_open = !self.console_open;
                } else {
                    self.help_open = Some(kind);
                    self.toast = None;
                }
                Task::none()
            }
            Message::CloseHelp => {
                self.help_open = None;
                Task::none()
            }
            Message::Noop => Task::none(),
            Message::Escape => {
                if self.line_edit.is_some() {
                    self.line_edit = None;
                } else if self.rename_open {
                    self.rename_open = false;
                } else if self.summary_open {
                    self.summary_open = false;
                    if self.pending_rename {
                        self.pending_rename = false;
                        return self.open_rename();
                    }
                } else if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.search_index = 0;
                } else if matches!(self.status, Status::Running) {
                    self.cancel();
                } else if self.selected_view.is_some() {
                    self.return_to_live();
                }
                Task::none()
            }
            Message::ShortcutPlay => {
                if self.rename_open || self.line_edit.is_some() {
                    return Task::none();
                }
                self.toggle_playback();
                Task::none()
            }
            Message::SeekBy(delta) => {
                if self.rename_open || self.line_edit.is_some() {
                    return Task::none();
                }
                if self.shown_duration().is_some() {
                    let max = self.shown_duration().unwrap_or(0.0).max(0.0);
                    self.playhead = (self.playhead + delta).clamp(0.0, max);
                    let playhead = self.playhead;
                    self.last_tick = Some(Instant::now());
                    if let Some(player) = &self.player {
                        player.seek(playhead, self.playback_speed);
                    }
                }
                Task::none()
            }
            Message::ImportPicked(path) => match path {
                Some(path) => Task::perform(
                    async move { backend::read_transcript(&path).map(|text| (path, text)) },
                    Message::ImportLoaded,
                ),
                None => Task::none(),
            },
            Message::ImportLoaded(loaded) => {
                let Some((transcript, text)) = loaded else {
                    return Task::none();
                };
                let audio = Self::matching_audio(&transcript);
                // Reuse the entry for the same transcript; attach the audio to a
                // still-pending entry for the same audio instead of duplicating.
                let existing = self.queue.iter().position(|item| {
                    item.transcript.as_deref() == Some(transcript.as_path())
                        || (audio.is_some()
                            && item.audio == audio
                            && matches!(item.status, ItemStatus::Pending))
                });
                let index = match existing {
                    Some(index) => {
                        let item = &mut self.queue[index];
                        item.transcript = Some(transcript.clone());
                        if !matches!(item.status, ItemStatus::Done) {
                            item.status = ItemStatus::Done;
                            item.stats = None;
                            item.turns.clear();
                            item.peaks = Arc::new(Vec::new());
                        }
                        if item.audio.is_none() {
                            item.audio = audio.clone();
                        }
                        index
                    }
                    None => {
                        let path = audio.clone().unwrap_or_else(|| transcript.clone());
                        self.queue.push(QueueItem {
                            path,
                            audio: audio.clone(),
                            status: ItemStatus::Done,
                            duration: None,
                            transcript: Some(transcript.clone()),
                            stats: None,
                            turns: Vec::new(),
                            peaks: Arc::new(Vec::new()),
                        });
                        self.queue.len() - 1
                    }
                };
                self.save_queue();
                // Plain .txt files are editable in place; anything else (legacy
                // JSON) is shown read-only.
                let editable = transcript
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("txt"))
                    .unwrap_or(false);
                let output = editable.then(|| transcript.clone());
                let mut tasks = vec![self.open_item_view(index, Some((text, output)))];
                if let Some(audio) = audio {
                    tasks.push(Self::probe_items(vec![audio]));
                }
                Task::batch(tasks)
            }
            Message::AddFiles => Task::perform(
                async {
                    rfd::FileDialog::new()
                        .add_filter("Audio/Video", AUDIO_EXTS)
                        .pick_files()
                },
                Message::FilesPicked,
            ),
            Message::FilesPicked(paths) => {
                if let Some(paths) = paths {
                    let added = self.add_paths(paths);
                    return Self::probe_items(added);
                }
                Task::none()
            }
            Message::AddFolder => Task::perform(
                async { rfd::FileDialog::new().pick_folder() },
                Message::FolderPicked,
            ),
            Message::FolderPicked(dir) => {
                if let Some(dir) = dir {
                    let added = self.add_paths(audio_files_in(&dir));
                    return Self::probe_items(added);
                }
                Task::none()
            }
            Message::ItemDurationProbed(path, seconds) => {
                if let Some(item) = self
                    .queue
                    .iter_mut()
                    .find(|item| item.path == path || item.audio.as_deref() == Some(path.as_path()))
                {
                    item.duration = seconds;
                }
                if let Some(view) = self.selected_view.as_mut() {
                    if view.audio.as_deref() == Some(path.as_path()) {
                        view.duration = seconds.map(|seconds| seconds as f32);
                    }
                }
                self.save_queue();
                Task::none()
            }
            Message::RemoveItem(index) => {
                if !matches!(self.status, Status::Running) && index < self.queue.len() {
                    let was_viewing = self.selected_view.is_some();
                    self.queue.remove(index);
                    if let Some(current) = self.current.as_mut() {
                        if *current == index {
                            self.current = None;
                        } else if *current > index {
                            *current -= 1;
                        }
                    }
                    if let Some(current) = self.current_index.as_mut() {
                        if *current == index {
                            self.current_index = None;
                        } else if *current > index {
                            *current -= 1;
                        }
                    }
                    self.selected = self.selected.and_then(|selected| {
                        match selected.cmp(&index) {
                            std::cmp::Ordering::Equal => None,
                            std::cmp::Ordering::Greater => Some(selected - 1),
                            std::cmp::Ordering::Less => Some(selected),
                        }
                    });
                    if self.selected.is_none() {
                        self.selected_view = None;
                        self.view_error = None;
                        if was_viewing {
                            // The viewed transcript is gone: stop its audio.
                            self.player = None;
                            self.playhead = 0.0;
                            self.last_tick = None;
                        }
                    } else if let Some(view) = self.selected_view.as_mut() {
                        if view.index > index {
                            view.index -= 1;
                        }
                    }
                    self.save_queue();
                }
                Task::none()
            }
            Message::ClearQueue => {
                if !matches!(self.status, Status::Running) {
                    // "Clear" is a full reset: queue, transcript and run state.
                    self.queue.clear();
                    self.current = None;
                    self.current_index = None;
                    self.transcript.clear();
                    self.lines.clear();
                    self.pending_lines.clear();
                    self.reveal_accum = 0.0;
                    self.completion_pending = false;
                    self.transcript_dirty = false;
                    self.turns.clear();
                    self.peaks = Arc::new(Vec::new());
                    self.peaks_for = None;
                    self.playhead = 0.0;
                    self.timeline_duration = None;
                    self.player = None;
                    self.scroll_target = None;
                    self.scroll_seek = false;
                    self.output_txt = None;
                    self.summary_open = false;
                    self.pending_rename = false;
                    self.rename_open = false;
                    self.rename_fields.clear();
                    self.rename_focus = 0;
                    self.search_query.clear();
                    self.search_index = 0;
                    self.follow_offset = None;
                    self.transcript_at_bottom = true;
                    self.console_lines.clear();
                    self.toast = None;
                    self.selected = None;
                    self.selected_view = None;
                    self.view_error = None;
                    self.run_start = None;
                    self.elapsed_final = None;
                    self.progress = 0.0;
                    self.progress_shown = 0.0;
                    self.overall_shown = 0.0;
                    self.stage = "idle".to_string();
                    self.status = Status::Idle;
                    self.save_queue();
                }
                Task::none()
            }
            Message::OpenSummary => {
                if self.has_results() {
                    self.summary_open = true;
                    self.pending_rename = false;
                    self.toast = None;
                    return scrollable::snap_to(
                        summary_id(),
                        scrollable::RelativeOffset { x: 0.0, y: 0.0 },
                    );
                }
                Task::none()
            }
            Message::CloseSummary => {
                self.summary_open = false;
                if self.pending_rename {
                    self.pending_rename = false;
                    return self.open_rename();
                }
                Task::none()
            }
            Message::OpenRename => self.open_rename(),
            Message::EditLine(index) => {
                // Only transcripts we can write back to are editable, and the
                // live transcript is left alone while it is still being built.
                if self.shown_output().is_none()
                    || (self.selected_view.is_none() && matches!(self.status, Status::Running))
                {
                    self.toast = Some(("This transcript is read-only".to_string(), Instant::now()));
                    return Task::none();
                }
                let Some(line) = self.shown_lines().get(index) else {
                    return Task::none();
                };
                let speaker = line.speaker.clone();
                let text = line_text(line);
                self.line_edit = Some(LineEdit {
                    index,
                    speaker,
                    content: text_editor::Content::with_text(&text),
                });
                self.summary_open = false;
                self.rename_open = false;
                self.toast = None;
                Task::none()
            }
            Message::LineEditSpeaker(value) => {
                if let Some(edit) = &mut self.line_edit {
                    edit.speaker = value;
                }
                Task::none()
            }
            Message::LineEditAction(action) => {
                if let Some(edit) = &mut self.line_edit {
                    edit.content.perform(action);
                }
                Task::none()
            }
            Message::SaveLineEdit => {
                self.apply_line_edit();
                Task::none()
            }
            Message::CancelLineEdit => {
                self.line_edit = None;
                Task::none()
            }
            Message::DeleteLineEdit => {
                self.delete_edit_line();
                Task::none()
            }
            Message::CopyTranscript => {
                if self.shown_transcript().is_empty() {
                    return Task::none();
                }
                self.toast = Some(("Copied transcript".to_string(), Instant::now()));
                iced::clipboard::write(self.shown_transcript().to_string())
            }
            Message::RevealOutput => {
                self.reveal_output();
                Task::none()
            }
            Message::SaveTranscript => {
                match (self.shown_output(), self.shown_transcript()) {
                    (Some(path), text) => {
                        if std::fs::write(path, text).is_ok() {
                            self.toast =
                                Some((format!("Saved {}", path.display()), Instant::now()));
                        }
                    }
                    (None, text) if !text.is_empty() => {
                        self.toast = Some((
                            "Imported transcript is read-only".to_string(),
                            Instant::now(),
                        ));
                    }
                    _ => {
                        self.toast = Some(("No output file yet".to_string(), Instant::now()));
                    }
                }
                Task::none()
            }
            Message::ImportChoice(kind) => match kind {
                ImportKind::Transcription => Task::perform(
                    async {
                        rfd::FileDialog::new()
                            .add_filter("Transcript", &["txt", "json"])
                            .pick_file()
                    },
                    Message::ImportPicked,
                ),
                ImportKind::Audio => Task::perform(
                    async {
                        rfd::FileDialog::new()
                            .add_filter("Audio/Video", AUDIO_EXTS)
                            .pick_file()
                    },
                    Message::AudioPicked,
                ),
            },
            Message::AudioPicked(path) => {
                if let Some(path) = path {
                    if self
                        .queue
                        .iter()
                        .any(|item| item.audio.as_deref() == Some(path.as_path()))
                    {
                        self.toast =
                            Some(("Already in the queue".to_string(), Instant::now()));
                        return Task::none();
                    }
                    let added = self.add_paths(vec![path]);
                    if let Some(path) = added.first() {
                        let name = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("file")
                            .to_string();
                        self.toast =
                            Some((format!("Added {name} to the queue"), Instant::now()));
                        return Self::probe_items(added);
                    }
                }
                Task::none()
            }
            Message::PickOutput => {
                let default_dir = self.output_dir.clone();
                Task::perform(
                    async move {
                        let mut dialog = rfd::FileDialog::new();
                        if let Some(dir) = default_dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_folder()
                    },
                    Message::OutputPicked,
                )
            }
            Message::OutputPicked(dir) => {
                if let Some(dir) = dir {
                    self.output_dir = Some(dir);
                    self.save_settings();
                }
                Task::none()
            }
            Message::PresetChanged(preset) => {
                if matches!(self.status, Status::Running) || self.queue.is_empty() {
                    return Task::none();
                }
                self.preset = preset;
                self.save_settings();
                Task::none()
            }
            Message::Start => {
                self.start();
                Task::none()
            }
            Message::Cancel => {
                self.cancel();
                Task::none()
            }
            Message::Tick => {
                let mut tasks = vec![self.drain_events()];
                if self.transcript_dirty && self.selected_view.is_none() {
                    self.transcript_dirty = false;
                    let mut out = String::new();
                    for index in self.display_order() {
                        let line = &self.lines[index];
                        if line.raw.is_empty() {
                            continue;
                        }
                        out.push_str(&line.raw);
                        out.push('\n');
                    }
                    self.transcript = out;
                }

                // Reveal queued utterances one at a time, like natural speech.
                self.reveal_accum += 0.1;
                let mut revealed = 0usize;
                while !self.pending_lines.is_empty() {
                    let interval = if self.pending_lines.len() > REVEAL_BACKLOG {
                        REVEAL_CATCHUP_S
                    } else {
                        REVEAL_INTERVAL_S
                    };
                    if self.reveal_accum < interval {
                        break;
                    }
                    self.reveal_accum -= interval;
                    let line = self.pending_lines.pop_front().unwrap();
                    self.reveal_line(line);
                    revealed += 1;
                    if revealed >= 30 {
                        break;
                    }
                }

                self.activity_frame = self.activity_frame.wrapping_add(1);
                // Ease the per-file bar toward the latest reported progress.
                self.progress_shown += (self.progress - self.progress_shown) * 0.25;
                self.progress_shown = self.progress_shown.clamp(0.0, 1.0);

                // Ease an overall bar that continues smoothly across queued files.
                let total_items = self.queue.len().max(1) as f32;
                let animating =
                    matches!(self.status, Status::Running) || self.completion_pending;
                let target_overall = if animating {
                    (self.current_index.unwrap_or(0) as f32 + self.progress) / total_items
                } else {
                    let done = self
                        .queue
                        .iter()
                        .filter(|item| {
                            matches!(item.status, ItemStatus::Done | ItemStatus::Failed(_))
                        })
                        .count() as f32;
                    done / total_items
                };
                self.overall_shown += (target_overall - self.overall_shown) * 0.25;
                self.overall_shown = self.overall_shown.clamp(0.0, 1.0);

                // Once the bar has eased to 100%, present the completion dialogs.
                if self.completion_pending && self.overall_shown >= 0.995 {
                    self.overall_shown = 1.0;
                    self.completion_pending = false;
                    if self.present_completion() {
                        tasks.push(scrollable::snap_to(
                            summary_id(),
                            scrollable::RelativeOffset { x: 0.0, y: 0.0 },
                        ));
                    }
                }
                if let Some(player) = &self.player {
                    if player.is_playing() {
                        let now = Instant::now();
                        if let Some(last) = self.last_tick {
                            let dt = (now - last).as_secs_f32();
                            let limit = self.shown_duration().unwrap_or(f32::MAX);
                            self.playhead =
                                (self.playhead + dt * self.playback_speed).min(limit);
                        }
                        self.last_tick = Some(now);
                    } else {
                        self.last_tick = None;
                        if player.finished() {
                            self.playhead = self.shown_duration().unwrap_or(self.playhead);
                        }
                    }
                }
                let content = self.transcript_content;
                let viewport = self.transcript_viewport;
                // Follow tracks the active line on whichever transcript is
                // shown (streamed text while live, the audio playhead
                // otherwise). Manual scrolling disengages it.
                if self.follow_playhead {
                    if let Some(active) = self.active_line_index() {
                        if let Some(y) = self.follow_target(active) {
                            self.follow_offset = Some(y);
                            self.scroll_target = Some(y);
                            tasks.push(scrollable::scroll_to(
                                transcript_id(),
                                scrollable::AbsoluteOffset { x: 0.0, y },
                            ));
                        }
                    }
                } else {
                    self.follow_offset = None;
                    // Keep the live view pinned to the bottom as text is added,
                    // as long as the user hasn't scrolled away from the bottom.
                    if self.selected_view.is_none()
                        && self.transcript_at_bottom
                        && content > 0.0
                        && viewport > 0.0
                    {
                        let y = (content - viewport).max(0.0);
                        self.scroll_target = Some(y);
                        tasks.push(scrollable::scroll_to(
                            transcript_id(),
                            scrollable::AbsoluteOffset { x: 0.0, y },
                        ));
                    }
                }
                if self.window_dirty
                    && self
                        .last_window_change
                        .is_some_and(|last| last.elapsed() > Duration::from_millis(600))
                {
                    self.window_dirty = false;
                    self.save_settings();
                }
                if self
                    .toast
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4))
                {
                    self.toast = None;
                }
                tasks.push(self.backfill_next_waveform());
                Task::batch(tasks)
            }
            Message::RenameSave => {
                self.apply_rename();
                Task::none()
            }
            Message::RenameCancel => {
                self.rename_open = false;
                Task::none()
            }
        }
    }

    /// Populate the rename dialog from the distinct speakers in the transcript.
    /// Fields start empty (the current name is the placeholder); saving an empty
    /// field keeps the original name. Focuses the first field.
    fn open_rename(&mut self) -> Task<Message> {
        let mut seen: Vec<String> = Vec::new();
        for line in self.shown_lines() {
            if !line.speaker.is_empty() && !seen.contains(&line.speaker) {
                seen.push(line.speaker.clone());
            }
        }
        self.rename_fields = seen
            .into_iter()
            .map(|name| (name, String::new()))
            .collect();
        self.rename_focus = 0;
        self.rename_open = true;
        self.toast = None;
        if self.rename_fields.is_empty() {
            Task::none()
        } else {
            text_input::focus(rename_id(0))
        }
    }

    fn refresh_transcript(&mut self) {
        self.transcript = self
            .lines
            .iter()
            .map(|line| line.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !self.transcript.is_empty() {
            self.transcript.push('\n');
        }
        if let Some(path) = &self.output_txt {
            let _ = std::fs::write(path, &self.transcript);
        }
    }

    fn reveal_output(&self) {
        let dir = self
            .shown_output()
            .or_else(|| self.shown_audio())
            .and_then(|path| path.parent())
            .map(|path| path.to_path_buf())
            .or_else(|| self.output_dir.clone());
        let Some(dir) = dir else { return };
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(&dir).spawn();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
        }
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let mut added = Vec::new();
        for path in paths {
            if self
                .queue
                .iter()
                .any(|item| item.path == path || item.audio.as_deref() == Some(path.as_path()))
            {
                continue;
            }
            self.queue.push(QueueItem {
                path: path.clone(),
                audio: Some(path.clone()),
                status: ItemStatus::Pending,
                duration: None,
                transcript: None,
                stats: None,
                turns: Vec::new(),
                peaks: Arc::new(Vec::new()),
            });
            added.push(path);
        }
        if !added.is_empty() {
            self.save_queue();
        }
        added
    }

    fn probe_items(paths: Vec<PathBuf>) -> Task<Message> {
        Task::batch(paths.into_iter().map(|path| {
            let message_path = path.clone();
            Task::perform(
                async move { backend::probe_duration(&path) },
                move |seconds| Message::ItemDurationProbed(message_path.clone(), seconds),
            )
        }))
    }

    /// Show the summary (and queue the rename dialog) after the bar reaches 100%.
    /// Returns true when a summary was opened.
    fn present_completion(&mut self) -> bool {
        if !self.has_results() {
            return false;
        }
        let generated = self
            .queue
            .iter()
            .filter(|item| matches!(item.status, ItemStatus::Done) && item.stats.is_some())
            .count();
        self.summary_open = true;
        self.pending_rename = generated == 1 && self.completed_count() == 1;
        self.toast = None;
        notify("transiel", "Transcription complete");
        true
    }

    /// Move one queued utterance into the visible transcript.
    fn reveal_line(&mut self, line: DocLine) {
        if !self.transcript.is_empty() && !self.transcript.ends_with('\n') {
            self.transcript.push('\n');
        }
        self.transcript.push_str(&line.raw);
        self.transcript.push('\n');
        self.lines.push(line);
        self.transcript_dirty = true;
    }

    /// Probe one completed item's waveform in the background. Results are
    /// persisted so subsequent clicks show the waveform immediately.
    fn backfill_next_waveform(&mut self) -> Task<Message> {
        if self.backfill_active || matches!(self.status, Status::Running) {
            return Task::none();
        }
        let next = self.queue.iter().find_map(|item| {
            if !matches!(item.status, ItemStatus::Done) || !item.peaks.is_empty() {
                return None;
            }
            let audio = item.audio.as_ref()?;
            if !audio.is_file() || self.waveform_tried.contains(audio) {
                return None;
            }
            Some(audio.clone())
        });
        let Some(path) = next else {
            return Task::none();
        };
        self.waveform_tried.insert(path.clone());
        self.backfill_active = true;
        let probe_path = path.clone();
        let message_path = path;
        Task::perform(
            async move { backend::probe_waveform(&probe_path, TIMELINE_BUCKETS) },
            move |probe| Message::QueueWaveformLoaded(message_path.clone(), probe),
        )
    }

    /// Start or pause playback for the transcript currently on screen.
    fn toggle_playback(&mut self) {
        if !self.player_ready() {
            return;
        }
        if self.player.is_none() {
            let Some(path) = self.shown_audio().map(Path::to_path_buf) else {
                return;
            };
            self.player = Player::new(&path);
            if self.player.is_none() {
                self.toast =
                    Some(("Could not open the audio file".to_string(), Instant::now()));
                return;
            }
            if let Some(player) = &self.player {
                player.seek(self.playhead, self.playback_speed);
            }
        }
        if self.player.as_ref().is_some_and(|player| player.finished()) {
            self.playhead = 0.0;
        }
        if let Some(player) = &self.player {
            player.set_speed(self.playback_speed);
            player.toggle();
        }
        self.last_tick = Some(Instant::now());
    }

    fn start(&mut self) {
        if self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending))
        {
            self.begin_batch(false);
        } else {
            self.status = Status::Error("add at least one file".to_string());
        }
    }

    /// Spawn a single pipeline process for all pending queue items, so the
    /// models load once and are reused across the whole batch. `preserve_view`
    /// keeps a completed transcript (and its playback) visible across an
    /// automatic follow-up pass; explicit starts always return to live.
    fn begin_batch(&mut self, preserve_view: bool) {
        let preserve = preserve_view && self.keep_pinned_view();
        let inputs: Vec<PathBuf> = self
            .queue
            .iter()
            .filter(|item| matches!(item.status, ItemStatus::Pending))
            .map(|item| item.path.clone())
            .collect();
        if inputs.is_empty() {
            self.status = Status::Error("add at least one file".to_string());
            return;
        }

        self.batch = true;
        self.current = None;
        self.current_index = None;
        self.status = Status::Running;
        self.stage = "starting".to_string();
        self.progress = 0.0;
        self.progress_shown = 0.0;
        self.overall_shown = 0.0;
        self.run_start = Some(Instant::now());
        self.elapsed_final = None;
        self.transcript.clear();
        self.lines.clear();
        self.pending_lines.clear();
        self.reveal_accum = 0.0;
        self.completion_pending = false;
        self.current_index = None;
        self.transcript_dirty = false;
        self.follow_offset = None;
        self.scroll_target = None;
        self.scroll_seek = false;
        self.transcript_at_bottom = true;
        self.turns.clear();
        self.peaks = Arc::new(Vec::new());
        self.peaks_for = None;
        self.timeline_duration = None;
        self.output_txt = None;
        self.rename_open = false;
        if !preserve {
            self.playhead = 0.0;
            self.player = None;
            self.selected = None;
            self.selected_view = None;
            self.view_error = None;
        }
        self.cancelled.store(false, Ordering::SeqCst);

        backend::spawn(
            inputs,
            self.output_dir.clone(),
            self.preset.arg(),
            self.num_speakers,
            true, // diarization cache is always on
            self.events.clone(),
            self.pid.clone(),
            self.cancelled.clone(),
        );
    }

    /// Match a backend-reported audio path to a queue item.
    fn find_queue_index(&self, path: &Path) -> Option<usize> {
        self.queue.iter().position(|item| {
            item.path == path
                || item
                    .path
                    .canonicalize()
                    .ok()
                    .zip(path.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b)
                || item.path.file_name() == path.file_name()
        })
    }

    fn cancel(&mut self) {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid != 0 {
            backend::kill(pid);
        }
        self.cancelled.store(true, Ordering::SeqCst);
        self.status = Status::Cancelled;
        self.stage = "cancelled".to_string();
        self.progress = 0.0;
        self.progress_shown = 0.0;
        self.overall_shown = 0.0;
        self.run_start = None;
        self.elapsed_final = None;
        self.pending_lines.clear();
        self.reveal_accum = 0.0;
        self.completion_pending = false;
        self.current_index = None;
        self.batch = false;
        if let Some(index) = self.current.take() {
            self.queue[index].status = ItemStatus::Failed("cancelled".to_string());
        }
        self.save_queue();
    }

    fn rename_lines(lines: &mut [DocLine], mapping: &[(String, String)]) {
        for line in lines {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == line.speaker) {
                line.raw = line.raw.replacen(&line.speaker, new, 1);
                line.speaker = new.clone();
            }
        }
    }

    fn rename_turns(turns: &mut [backend::Turn], mapping: &[(String, String)]) {
        for turn in turns {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == turn.speaker) {
                turn.speaker = new.clone();
            }
        }
    }

    fn apply_rename(&mut self) {
        let mapping: Vec<(String, String)> = self
            .rename_fields
            .iter()
            .filter(|(old, new)| !new.trim().is_empty() && new.trim() != old)
            .map(|(old, new)| (old.clone(), new.trim().to_string()))
            .collect();

        if let Some(view) = self.selected_view.as_mut() {
            Self::rename_lines(&mut view.lines, &mapping);
            Self::rename_turns(&mut view.turns, &mapping);
            view.transcript = view
                .lines
                .iter()
                .map(|line| line.raw.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !view.transcript.is_empty() {
                view.transcript.push('\n');
            }
            if let Some(path) = &view.output {
                let _ = std::fs::write(path, &view.transcript);
            }
            let index = view.index;
            let turns = view.turns.clone();
            if let Some(item) = self.queue.get_mut(index) {
                item.turns = turns;
            }
        } else {
            Self::rename_lines(&mut self.lines, &mapping);
            Self::rename_turns(&mut self.turns, &mapping);
            self.refresh_transcript();
        }
        self.rename_open = false;
    }

    /// Apply the right-click line editor: only that line's speaker and text
    /// change; the rest of the transcript is untouched.
    fn apply_line_edit(&mut self) {
        let Some(edit) = self.line_edit.as_ref() else {
            return;
        };
        let index = edit.index;
        let speaker = edit.speaker.trim().to_string();
        // One transcript line must stay one physical line: collapse any
        // newlines typed into the editor into spaces.
        let text = edit
            .content
            .text()
            .trim_end()
            .replace(['\n', '\r'], " ");
        self.line_edit = None;

        if let Some(view) = self.selected_view.as_mut() {
            let Some(line) = view.lines.get_mut(index) else {
                return;
            };
            rewrite_line(line, &speaker, &text);
            view.transcript = view
                .lines
                .iter()
                .map(|line| line.raw.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !view.transcript.is_empty() {
                view.transcript.push('\n');
            }
            if let Some(path) = &view.output {
                let _ = std::fs::write(path, &view.transcript);
            }
        } else if let Some(line) = self.lines.get_mut(index) {
            rewrite_line(line, &speaker, &text);
            self.refresh_transcript();
        }
    }

    /// Delete the line currently open in the right-click editor.
    fn delete_edit_line(&mut self) {
        let Some(edit) = self.line_edit.take() else {
            return;
        };
        let index = edit.index;
        let mut removed = false;
        if let Some(view) = self.selected_view.as_mut() {
            if index < view.lines.len() {
                view.lines.remove(index);
                view.transcript = view
                    .lines
                    .iter()
                    .map(|line| line.raw.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !view.transcript.is_empty() {
                    view.transcript.push('\n');
                }
                if let Some(path) = &view.output {
                    let _ = std::fs::write(path, &view.transcript);
                }
                removed = true;
            }
        } else if index < self.lines.len() {
            self.lines.remove(index);
            self.refresh_transcript();
            removed = true;
        }
        if removed {
            self.toast = Some(("Line deleted".to_string(), Instant::now()));
        }
    }

    fn drain_events(&mut self) -> Task<Message> {
        let mut tasks: Vec<Task<Message>> = Vec::new();
        loop {
            let event = self
                .events
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front());
            let Some(event) = event else { break };
            match event {
                Event::Stage { stage, label, progress } => {
                    self.stage = if label.is_empty() { stage } else { label };
                    self.progress = progress.clamp(0.0, 1.0);
                }
                Event::Log { message } => {
                    self.console_lines.push(message);
                    if self.console_lines.len() > 500 {
                        let excess = self.console_lines.len() - 500;
                        self.console_lines.drain(0..excess);
                    }
                    if self.console_open {
                        tasks.push(scrollable::snap_to(
                            console_id(),
                            scrollable::RelativeOffset { x: 0.0, y: 1.0 },
                        ));
                    }
                }
                Event::File { audio } => {
                    let path = PathBuf::from(audio);
                    // Keep a completed view (and its audio) alive when the user
                    // is listening to it; otherwise return to the live file.
                    let keep_view = self.keep_pinned_view();
                    self.current = self.find_queue_index(&path);
                    self.current_index = self.current;
                    if let Some(index) = self.current {
                        self.queue[index].status = ItemStatus::Running;
                    }
                    if !keep_view {
                        self.selected = None;
                        self.selected_view = None;
                        self.view_error = None;
                    }
                    self.status = Status::Running;
                    self.stage = "loading".to_string();
                    self.progress = 0.0;
                    self.progress_shown = 0.0;
                    self.run_start = Some(Instant::now());
                    self.elapsed_final = None;
                    self.transcript.clear();
                    self.lines.clear();
                    self.pending_lines.clear();
                    self.reveal_accum = 0.0;
                    self.completion_pending = false;
                    self.transcript_dirty = false;
                    self.follow_offset = None;
                    self.scroll_target = None;
                    self.scroll_seek = false;
                    self.transcript_at_bottom = true;
                    self.turns.clear();
                    self.peaks = Arc::new(Vec::new());
                    self.peaks_for = None;
                    self.timeline_duration = None;
                    if !keep_view {
                        self.playhead = 0.0;
                        self.player = None;
                    }
                    self.output_txt = None;
                    self.save_queue();
                }
                Event::Meta { audio, duration } => {
                    self.peaks_for = Some(PathBuf::from(audio));
                    self.peaks = Arc::new(Vec::new());
                    if self.selected_view.is_none() {
                        self.playhead = 0.0;
                    }
                    self.timeline_duration = Some(duration);
                    if let Some(index) = self.current {
                        if let Some(item) = self.queue.get_mut(index) {
                            item.duration = Some(duration as f64);
                        }
                    }
                }
                Event::Turns { turns } => {
                    self.turns = turns;
                }
                Event::Waveform { audio, peaks } => {
                    // Emitted once diarization has identified the speakers.
                    self.peaks_for = Some(PathBuf::from(audio));
                    self.peaks = Arc::new(peaks);
                }
                Event::Line {
                    start,
                    speaker,
                    text,
                    confidence: _,
                } => {
                    let raw = format!("[{}] {speaker}: {text}", format_secs(start as f64));
                    // Queue it; the textbox reveals lines at a speech-like pace.
                    self.pending_lines.push_back(DocLine {
                        raw,
                        speaker,
                        start: Some(start),
                    });
                }
                Event::Done { txt, stats, .. } => {
                    // Finish revealing any queued utterances immediately.
                    while let Some(line) = self.pending_lines.pop_front() {
                        self.reveal_line(line);
                    }
                    self.reveal_accum = 0.0;
                    self.stage = "done".to_string();
                    // Target 100%; the bar eases there over the next ticks.
                    self.progress = 1.0;
                    if let Ok(contents) = std::fs::read_to_string(&txt) {
                        self.transcript = contents;
                    }
                    self.transcript_dirty = false;
                    if self.lines.is_empty() {
                        self.lines = parse_lines(&self.transcript);
                    }
                    self.output_txt = Some(PathBuf::from(&txt));
                    let transcript = PathBuf::from(&txt);
                    let turns = self.turns.clone();
                    let peaks = self.peaks.clone();
                    let duration = self.timeline_duration.map(|seconds| seconds as f64);
                    if let Some(index) = self.current.take() {
                        let item = &mut self.queue[index];
                        item.status = ItemStatus::Done;
                        item.transcript = Some(transcript);
                        item.stats = Some(stats);
                        item.turns = turns;
                        item.peaks = peaks;
                        item.duration = duration.or(item.duration);
                    }
                    self.save_queue();
                }
                Event::Error { message } => {
                    self.stage = "error".to_string();
                    let had_current = self.current.is_some();
                    if let Some(index) = self.current.take() {
                        self.queue[index].status = ItemStatus::Failed(message.clone());
                    }
                    if self.batch {
                        // A process-level failure mid-file leaves the rest
                        // unprocessed; mark them failed. A preflight failure
                        // (no file started, e.g. a missing model) keeps the
                        // queue pending so it can be retried after fixing it.
                        if had_current {
                            for item in self.queue.iter_mut() {
                                if matches!(
                                    item.status,
                                    ItemStatus::Pending | ItemStatus::Running
                                ) {
                                    item.status = ItemStatus::Failed(message.clone());
                                }
                            }
                        }
                        self.batch = false;
                    }
                    self.status = Status::Error(message);
                    self.save_queue();
                }
                Event::AllDone => {
                    self.batch = false;
                    self.current = None;
                    self.stage = "done".to_string();
                    self.progress = 1.0;
                    self.elapsed_final = self.run_start.map(|s| s.elapsed().as_secs_f64());
                    self.toast = None;
                    self.save_queue();
                    // Files added while a run was active are picked up as a
                    // follow-up pass without prompting.
                    if self
                        .queue
                        .iter()
                        .any(|item| matches!(item.status, ItemStatus::Pending))
                    {
                        self.begin_batch(true);
                    } else if self.has_results() {
                        self.status = Status::Done;
                        // Present the summary / rename only once the bar hits 100%.
                        self.completion_pending = true;
                    } else {
                        // Nothing succeeded: no dialogs, just settle at 100%.
                        self.progress = 1.0;
                        self.progress_shown = 1.0;
                        self.overall_shown = 1.0;
                    }
                }
            }
        }
        Task::batch(tasks)
    }

    fn stats_lines(&self) -> Vec<SummaryLine> {
        let mut lines: Vec<SummaryLine> = Vec::new();
        for item in &self.queue {
            if !matches!(item.status, ItemStatus::Done) {
                continue;
            }
            let name = item
                .audio
                .as_ref()
                .or(item.transcript.as_ref())
                .or(Some(&item.path))
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("file")
                .to_string();
            if !lines.is_empty() {
                lines.push(SummaryLine::Body(String::new()));
            }
            lines.push(SummaryLine::File(name));
            match &item.stats {
                Some(stats) => lines.extend(
                    Self::single_stats_lines(stats)
                        .into_iter()
                        .map(SummaryLine::Body),
                ),
                None => lines.push(SummaryLine::Body("Imported transcript".to_string())),
            }
        }
        if lines.is_empty() {
            vec![SummaryLine::Body("No completed run yet.".to_string())]
        } else {
            lines
        }
    }

    /// The summary block for one completed run.
    fn single_stats_lines(s: &backend::Stats) -> Vec<String> {
        let speed = if s.run_seconds > 0.0 {
            s.audio_seconds / s.run_seconds
        } else {
            0.0
        };
        let speakers = match s.num_speakers {
            Some(count) => count.to_string(),
            None => format!("{} (auto)", s.speakers),
        };
        let mut lines = vec![
            format!("Total run time: {}", format_secs(s.run_seconds)),
            format!("Audio length: {}", format_secs(s.audio_seconds)),
            format!("Speed: {:.1}\u{00d7} realtime", speed),
            format!("Speakers: {speakers}"),
            format!("Preset: {}", s.preset),
        ];
        if !s.speaker_stats.is_empty() {
            let mut stats: Vec<(&String, &backend::SpeakerStat)> =
                s.speaker_stats.iter().collect();
            stats.sort_by(|a, b| {
                b.1.seconds
                    .partial_cmp(&a.1.seconds)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            lines.push("Per speaker:".to_string());
            for (name, stat) in stats {
                lines.push(format!(
                    "  {name}: {}, {} segments, {} words",
                    format_secs(stat.seconds as f64),
                    stat.segments,
                    stat.words
                ));
            }
        }
        lines
    }

    fn elapsed(&self) -> Option<String> {
        if let Some(secs) = self.elapsed_final {
            return Some(format_secs(secs));
        }
        if matches!(self.status, Status::Running) {
            return self.run_start.map(|start| format_secs(start.elapsed().as_secs_f64()));
        }
        None
    }

    /// Collapsible console showing backend log output.
    fn console_panel(&self, scheme: &Scheme) -> Element<'static, Message> {
        let background = scheme.track;
        let mut col = column![].spacing(1).padding(4);
        for line in &self.console_lines {
            col = col.push(text(line.clone()).size(12));
        }
        container(scrollable(col).id(console_id()).height(Length::Fill).width(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fixed(150.0))
            .padding(8)
            .style(move |_theme| container::Style {
                background: Some(background.into()),
                border: iced::border::rounded(8.0),
                shadow: soft_shadow(),
                ..container::Style::default()
            })
            .into()
    }

    /// Line indices (into the displayed transcript) containing the query.
    fn matching_lines(&self) -> Vec<usize> {
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        self.shown_lines()
            .iter()
            .enumerate()
            .filter(|(_, line)| line.raw.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    /// Distinct speakers in transcript order (matches the SMS bubble ordering).
    fn speaker_order(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for line in self.shown_lines() {
            if !line.speaker.is_empty() && !order.contains(&line.speaker) {
                order.push(line.speaker.clone());
            }
        }
        order
    }

    /// Line indices sorted by start time (untimed lines last), stable.
    fn display_order(&self) -> Vec<usize> {
        let lines = self.shown_lines();
        let mut indices: Vec<usize> = (0..lines.len()).collect();
        indices.sort_by(|a, b| {
            let ta = lines[*a].start.unwrap_or(f32::INFINITY);
            let tb = lines[*b].start.unwrap_or(f32::INFINITY);
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        });
        indices
    }

    /// Scroll the transcript so the given line (by index) is roughly centred.
    /// Pixel scroll offset that centres the given line in the transcript.
    fn follow_target(&self, line: usize) -> Option<f32> {
        let total = self.shown_lines().len().max(1) as f32;
        let content = self.transcript_content;
        let viewport = self.transcript_viewport;
        if content <= 0.0 || viewport <= 0.0 {
            return None;
        }
        let rank = self.display_order().iter().position(|index| *index == line)?;
        let line_height = content / total;
        Some(
            ((rank as f32 + 0.5) * line_height - viewport / 2.0)
                .clamp(0.0, (content - viewport).max(0.0)),
        )
    }

    fn scroll_to_line(&mut self, line: usize) -> Task<Message> {
        match self.follow_target(line) {
            Some(y) => {
                self.scroll_target = Some(y);
                scrollable::scroll_to(
                    transcript_id(),
                    scrollable::AbsoluteOffset { x: 0.0, y },
                )
            }
            None => Task::none(),
        }
    }

    /// Move the playhead to the line matching the scroll position after a user
    /// scroll. Reaches the first line at the top and the last line at the
    /// bottom instead of always centring.
    fn seek_from_scroll(&mut self, offset: f32, viewport: f32) {
        let total = self.shown_lines().len();
        if total == 0 || viewport <= 0.0 {
            return;
        }
        let content = self.transcript_content;
        if content <= 0.0 {
            return;
        }
        let max_offset = (content - viewport).max(0.0);
        let rank = if max_offset > 0.0 {
            (offset.clamp(0.0, max_offset) / max_offset) * (total - 1) as f32
        } else {
            0.0
        };
        let order = self.display_order();
        if order.is_empty() {
            return;
        }
        let rank = rank.floor().clamp(0.0, (order.len() - 1) as f32) as usize;
        let start = order
            .iter()
            .skip(rank)
            .chain(order[..rank].iter().rev())
            .find_map(|index| self.shown_lines().get(*index).and_then(|line| line.start));
        let Some(start) = start else {
            return;
        };
        let max = self.shown_duration().unwrap_or(start);
        self.playhead = start.clamp(0.0, max);
        self.scroll_seek = true;
        self.last_tick = Some(Instant::now());
        if let Some(player) = &self.player {
            player.seek(self.playhead, self.playback_speed);
        }
    }

    /// Transcript view: per-line (search tinted) or SMS bubbles.
    fn transcript_element(&self) -> Element<'_, Message> {
        let scheme = Scheme::light();
        if let Some(error) = &self.view_error {
            let mut col = column![
                text("Transcript unavailable").size(20),
                text(error.clone()).size(14),
            ]
            .spacing(8)
            .padding(16);
            if let Some(index) = self.selected {
                let has_audio = self
                    .queue
                    .get(index)
                    .and_then(|item| item.audio.as_ref())
                    .is_some();
                if has_audio {
                    col = col.push(
                        action("Transcribe again")
                            .on_press(Message::RequeueItem(index))
                            .style(color_button(scheme.green, scheme.ink)),
                    );
                }
            }
            return col.into();
        }
        let query = self.search_query.trim().to_string();
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            self.matching_lines()
        };
        let active_line = matches.get(self.search_index).copied();
        let size = 16.0 * self.font_scale;
        let mut col = column![].spacing(2).padding(8).width(Length::Fill);
        let speaker_order = self.speaker_order();
        let display: Vec<usize> = self.display_order();
        let playhead_line = self.active_line_index();
        let lines = self.shown_lines();
        // SMS bubbles alternate sides per message, rather than giving each
        // speaker a fixed column.
        for idx in display {
            let line = &lines[idx];
            if line.raw.trim().is_empty() {
                continue;
            }
            let matched = !query.is_empty() && matches.contains(&idx);
            let active = playhead_line == Some(idx);
            if self.sms_mode {
                let index = speaker_order
                    .iter()
                    .position(|name| *name == line.speaker)
                    .unwrap_or(0);
                // Each speaker keeps a fixed side (their own column).
                let left = index.is_multiple_of(2);
                let tint = if matched {
                    Some(if Some(idx) == active_line {
                        scheme.search_active
                    } else {
                        scheme.search_hl
                    })
                } else {
                    None
                };
                col = col.push(
                    stack![
                        sms_row(
                            line.raw.trim(),
                            left,
                            speaker_palette(index, &scheme),
                            scheme.ink,
                            tint,
                            &query,
                            scheme.search_hl,
                            size,
                            active,
                        ),
                        mouse_area(
                            container(horizontal_space())
                            .width(Length::Fill)
                            .height(Length::Fill),
                        )
                        .interaction(mouse::Interaction::Pointer)
                        .on_press(Message::SelectLine(idx))
                        .on_right_press(Message::EditLine(idx)),
                    ],
                );
            } else {
                let speaker_bg = speaker_order
                    .iter()
                    .position(|name| *name == line.speaker)
                    .map(|index| speaker_palette(index, &scheme));
                let tint = if matched {
                    Some(if Some(idx) == active_line {
                        scheme.search_active
                    } else {
                        scheme.search_hl
                    })
                } else {
                    speaker_bg
                };
                col = col.push(line_element(
                    line,
                    idx,
                    tint,
                    &query,
                    scheme.search_hl,
                    &scheme,
                    size,
                    active,
                ));
            }
        }

        stack![
            scrollable(col)
            .id(transcript_id())
            .on_scroll(Message::TranscriptScrolled)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(scrollbar_style),
            container(self.view_controls(&scheme))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::End)
            .align_y(iced::Alignment::End)
            .padding(10),
        ]
            .into()
    }

    /// Zoom / Follow / SMS controls overlaid on the transcript (bottom-right).
    fn view_controls(&self, scheme: &Scheme) -> Element<'_, Message> {
        let follow: Element<Message> = if self.follow_playhead {
            row![icon(ICON_CHECK, 12.0), text("Follow")]
                .spacing(4)
                .align_y(iced::Alignment::Center)
                .into()
        } else {
            text("Follow").into()
        };
        let sms: Element<Message> = if self.sms_mode {
            row![icon(ICON_CHECK, 12.0), text("SMS")]
                .spacing(4)
                .align_y(iced::Alignment::Center)
                .into()
        } else {
            text("SMS").into()
        };
        let sms_on = self.sms_mode;
        row![
            self.tip(
                action(icon(ICON_ZOOM_IN, 14.0))
                .on_press(Message::ZoomIn)
                .style(pill_button_secondary),
                "Zoom text in",
                scheme,
            ),
            self.tip(
                action(icon(ICON_ZOOM_OUT, 14.0))
                .on_press(Message::ZoomOut)
                .style(pill_button_secondary),
                "Zoom text out",
                scheme,
            ),
            self.tip(
                action(follow)
                .on_press(Message::ToggleFollowPlayhead)
                .style(pill_button_secondary),
                "Keep the active line in view",
                scheme,
            ),
            self.tip(
                action(sms)
                .on_press(Message::SmsToggled(!sms_on))
                .style(pill_button_secondary),
                "Show the transcript as chat bubbles",
                scheme,
            ),
        ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
    }

    /// Index of the last line whose start time is at or before `t`.
    fn line_for_playhead(&self, t: f32) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (index, line) in self.shown_lines().iter().enumerate() {
            if let Some(start) = line.start {
                if start <= t && best.map_or(true, |(_, best_start)| start >= best_start) {
                    best = Some((index, start));
                }
            }
        }
        best.map(|(index, _)| index)
    }

    /// The line to highlight / keep in view.
    fn active_line_index(&self) -> Option<usize> {
        if self.shown_lines().is_empty() {
            return None;
        }
        let live_streaming = self.selected_view.is_none() && matches!(self.status, Status::Running);
        if live_streaming && !self.scroll_seek {
            // The newest streamed utterance is the live playhead.
            self.display_order().last().copied()
        } else {
            // Completed views, playback, and manual scroll-seeks use the audio
            // playhead.
            self.line_for_playhead(self.playhead)
                .or_else(|| self.display_order().first().copied())
        }
    }

    /// Waveform/speaker timeline strip (greyed out until audio is probed).
    fn timeline_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let duration = self.shown_duration().unwrap_or(0.0);
        let enabled = duration > 0.0 && !self.shown_peaks().is_empty();
        let program = Timeline {
            peaks: self.shown_peaks_arc(),
            turns: Arc::new(if enabled {
                self.speaker_turns(scheme)
            } else {
                Vec::new()
            }),
            duration,
            playhead: self.playhead,
            track: scheme.timeline_bg,
            wave: scheme.green,
            playhead_color: scheme.ink,
            enabled,
            zoom: self.waveform_zoom,
        };
        let strip = canvas(program)
            .width(Length::Fill)
            .height(Length::Fixed(TIMELINE_HEIGHT));
        let mut column = column![].spacing(4).width(Length::Fill);
        if enabled {
            column = column.push(self.tip(strip, "Click/drag to seek, scroll to zoom", scheme));
            column = column.push(self.timeline_legend(scheme));
        } else {
            column = column.push(strip);
        }
        column.into()
    }

    /// Colour swatches + names for the speakers on the timeline.
    fn timeline_legend(&self, scheme: &Scheme) -> Element<'_, Message> {
        let order = self.speaker_order();
        let mut row = row![].spacing(12).align_y(iced::Alignment::Center);
        for (index, name) in order.iter().enumerate() {
            let color = speaker_palette(index, scheme);
            let label = name.clone();
            row = row.push(
                row![
                    container(horizontal_space())
                    .width(Length::Fixed(12.0))
                    .height(Length::Fixed(12.0))
                    .style(move |_theme| container::Style {
                        background: Some(color.into()),
                        border: iced::border::rounded(3.0),
                        ..container::Style::default()
                    }),
                    text(label).size(12),
                ]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            );
        }
        row.into()
    }

    fn speaker_turns(&self, scheme: &Scheme) -> Vec<TimelineTurn> {
        let order = self.speaker_order();
        self.shown_turns()
            .iter()
            .map(|turn| {
                let index = order
                    .iter()
                    .position(|name| *name == turn.speaker)
                    .unwrap_or(0);
                TimelineTurn {
                    start: turn.start,
                    end: turn.end,
                    color: speaker_palette(index, scheme),
                }
            })
            .collect()
    }

    /// Playback controls: play/pause, time, and a speed selector.
    fn player_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let duration = self.shown_duration().unwrap_or(0.0).max(0.0);
        let ready = self.player_ready();
        let playing = self.player.as_ref().is_some_and(|player| player.is_playing());
        let glyph = if ready { scheme.ink } else { MUTED };

        let play_icon = if playing { ICON_PAUSE } else { ICON_PLAY };
        let play_button: Element<Message> = if ready {
            action(icon_colored(play_icon, 14.0, glyph))
                .on_press(Message::PlayerToggle)
                .style(pill_button_secondary)
                .into()
        } else {
            action(icon_colored(ICON_PLAY, 14.0, glyph))
                .style(greyed_button)
                .into()
        };

        let time_color = if ready { scheme.ink } else { MUTED };
        let total = if duration > 0.0 {
            format_secs(duration as f64)
        } else {
            "--:--".to_string()
        };
        let time = text(format!(
            "{} / {}",
            format_secs(self.playhead as f64),
            total
        ))
            .size(12)
            .style(move |_theme| text::Style {
                color: Some(time_color),
            });

        let speed: Element<Message> = if ready {
            pick_list(
                Speed::ALL,
                Some(Speed::from_factor(self.playback_speed)),
                Message::SpeedChanged,
            )
                .style(speed_pick)
                .handle(caret_handle(false))
                .into()
        } else {
            stack![
                pick_list(Speed::ALL, Some(Speed::One), Message::SpeedChanged)
                .style(greyed_pick)
                .handle(caret_handle(false)),
                mouse_area(
                    container(horizontal_space())
                    .width(Length::Fill)
                    .height(Length::Fill),
                )
                .interaction(mouse::Interaction::Idle)
                .on_press(Message::Tick),
            ]
                .into()
        };

        row![play_button, time, speed]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
    }

    /// Find controls, always shown on the player row.
    fn search_element(&self, _scheme: &Scheme) -> Element<'_, Message> {
        let count = self.matching_lines().len();
        let status = if self.search_query.trim().is_empty() {
            String::new()
        } else if count == 0 {
            "no matches".to_string()
        } else {
            format!("{} / {}", self.search_index + 1, count)
        };
        let query_input = text_input("Find", &self.search_query)
            .id(find_id())
            .on_input(Message::SearchChanged)
            .width(Length::Fixed(120.0))
            .style(pill_input);
        let prev = action(icon(ICON_ARROW_UP, 14.0))
            .on_press(Message::FindPrev)
            .style(pill_button_secondary);
        let next = action(icon(ICON_ARROW_DOWN, 14.0))
            .on_press(Message::FindNext)
            .style(pill_button_secondary);
        row![query_input, text(status).size(12), prev, next]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
    }

    /// Wrap `content` with a tooltip.
    fn tip<'a>(
        &self,
        content: impl Into<Element<'a, Message>>,
        label: &'static str,
        scheme: &Scheme,
    ) -> Element<'a, Message> {
        let background = scheme.tip_bg;
        let text_color = scheme.tip_text;
        let border_color = scheme.tip_text;
        tooltip(
            content,
            container(text(label).size(12))
                .padding(6)
                .style(move |_theme| container::Style {
                    background: Some(background.into()),
                    text_color: Some(text_color),
                    border: iced::border::Border {
                        color: border_color,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    shadow: soft_shadow(),
                    ..container::Style::default()
                }),
            tooltip::Position::Top,
        )
            .into()
    }

    pub fn view(&self) -> Element<'_, Message> {
        let scheme = Scheme::light();
        let output = self
            .output_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(Default: audio source directory)".to_string());

        let running = matches!(self.status, Status::Running);
        let pending = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending));
        let has_queue = !self.queue.is_empty();
        let can_add = true;
        let can_remove = !running;
        let can_output = !running && has_queue;
        let can_preset = !running && has_queue;
        let can_start = !running && pending;
        let can_cancel = running;

        let preset: Element<Message> = if can_preset {
            pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged)
                .style(pill_pick)
                .into()
        } else {
            // Keep the real pick_list (so iced draws its arrow) but put a
            // click-capturing layer on top so it cannot be opened.
            stack![
                pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged)
                .style(greyed_pick),
                mouse_area(
                    container(horizontal_space())
                    .width(Length::Fill)
                    .height(Length::Fill),
                )
                .interaction(mouse::Interaction::Idle)
                .on_press(Message::Tick),
            ]
                .into()
        };

        let mut queue_list = column![].spacing(4).width(Length::Fill);
        for (index, item) in self.queue.iter().enumerate() {
            let badge: Element<Message> = match &item.status {
                ItemStatus::Pending => queue_status_icon(ICON_CIRCLE, 18.0, scheme.text),
                ItemStatus::Running => {
                    queue_status_badge(ICON_HOURGLASS, 14.0, scheme.track, scheme.text)
                }
                ItemStatus::Done => queue_status_badge(ICON_CHECK, 20.0, scheme.green, scheme.ink),
                ItemStatus::Failed(_) => {
                    queue_status_badge(ICON_X, 16.0, scheme.pink, scheme.ink)
                }
            };
            let name = item
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| item.path.display().to_string());
            let detail = match &item.status {
                ItemStatus::Failed(message) => format!(" \u{2014} {message}"),
                _ => String::new(),
            };
            let duration = match item.duration {
                Some(secs) => format!("[{}]", format_secs(secs)),
                None => "[--:--]".to_string(),
            };
            let viewing = self.selected == Some(index) && self.selected_view.is_some();
            let selectable = matches!(item.status, ItemStatus::Running)
                || (matches!(item.status, ItemStatus::Done) && item.transcript.is_some());
            let mut item_row = row![
                badge,
                text(duration),
                text(format!("{name}{detail}")).width(Length::Fill),
            ]
                .spacing(8)
                .align_y(iced::Alignment::Center);

            match &item.status {
                ItemStatus::Running => {
                    item_row = item_row.push(
                        progress_bar(0.0..=1.0, self.progress_shown)
                            .width(Length::Fixed(90.0))
                            .height(Length::Fixed(10.0))
                            .style(move |_theme| progress_bar::Style {
                                background: scheme.bar_track.into(),
                                bar: scheme.blue.into(),
                                border: iced::border::rounded(5.0),
                            }),
                    );
                    item_row = item_row.push(
                        text(format!("{:.0}%", self.progress_shown * 100.0)).size(12),
                    );
                }
                ItemStatus::Done => {
                    item_row = item_row.push(text("100%").size(12));
                }
                _ => {}
            }

            let row_tint = scheme.warn_tint;
            let hover_tint = scheme.bar_track;
            let row_text = scheme.text;
            let row_button = button(item_row)
                .padding([2, 6])
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let background = if viewing {
                        row_tint
                    } else if selectable && matches!(status, button::Status::Hovered) {
                        hover_tint
                    } else {
                        Color::TRANSPARENT
                    };
                    button::Style {
                        background: Some(background.into()),
                        text_color: row_text,
                        border: iced::border::rounded(8.0),
                        shadow: Shadow::default(),
                    }
                });
            let row_button = if selectable {
                row_button.on_press(Message::ViewItem(index))
            } else {
                row_button
            };
            queue_list = queue_list.push(
                row![
                    row_button,
                    self.tip(
                        action(icon(ICON_X, 13.0))
                            .on_press_maybe(can_remove.then_some(Message::RemoveItem(index)))
                            .style(pill_button_secondary),
                        "Remove from queue",
                        &scheme,
                    ),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center),
            );
        }

        let mut queue_col = column![
            row![
                action("Add File(s)")
                .on_press_maybe(can_add.then_some(Message::AddFiles))
                .style(color_button(scheme.blue, scheme.ink)),
                action("Add Folder")
                .on_press_maybe(can_add.then_some(Message::AddFolder))
                .style(color_button(scheme.blue, scheme.ink)),
                action("Clear")
                .on_press_maybe((!running && has_queue).then_some(Message::ClearQueue))
                .style(color_button(scheme.pink, scheme.ink)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        ]
            .spacing(6);

        if has_queue {
            const MAX_QUEUE_ROWS: usize = 5;
            let rows = self.queue.len();
            if rows > MAX_QUEUE_ROWS {
                // Show five rows and scroll the rest.
                let viewport = MAX_QUEUE_ROWS as f32 * 34.0;
                queue_col = queue_col.push(
                    scrollable(queue_list)
                        .width(Length::Fill)
                        .height(Length::Fixed(viewport))
                        .style(queue_scrollbar_style),
                );
            } else {
                let line_height = 34.0;
                let height = rows as f32 * line_height * 1.2;
                queue_col = queue_col.push(
                    container(queue_list)
                        .width(Length::Fill)
                        .height(Length::Fixed(height))
                        .align_y(iced::Alignment::Start),
                );
            }
        }

        let speaker_pick = pick_list(
            SpeakerOpt::ALL,
            Some(SpeakerOpt::from_value(self.num_speakers)),
            Message::SpeakerCountChanged,
        );
        let speakers: Element<Message> = if running {
            // Keep the dropdown visible (with its arrow) but inactive.
            stack![
                speaker_pick.style(greyed_pick),
                mouse_area(
                    container(horizontal_space())
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .interaction(mouse::Interaction::Idle)
                .on_press(Message::Tick),
            ]
            .into()
        } else {
            speaker_pick.style(pill_pick).into()
        };

        let mut controls = column![
            row![
                action("Save Location")
                .on_press_maybe(can_output.then_some(Message::PickOutput))
                .style(color_button(scheme.blue, scheme.ink)),
                text(output.clone()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            row![
                container(text("Preset:"))
                .height(Length::Fixed(30.0))
                .align_y(iced::Alignment::Center),
                preset,
                container(text("Speakers:"))
                .height(Length::Fixed(30.0))
                .align_y(iced::Alignment::Center),
                speakers,
            ]
            .spacing(8)
            .align_y(iced::Alignment::Start),
        ]
            .spacing(16);

        controls = controls.push(
            row![
                action("Transcribe")
                .on_press_maybe(can_start.then_some(Message::Start))
                .style(color_button(scheme.green, scheme.ink)),
                action("Cancel")
                .on_press_maybe(can_cancel.then_some(Message::Cancel))
                .style(color_button(scheme.pink, scheme.ink)),
            ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
        );

        let global_progress = self.overall_shown;

        let clock = row![
            icon_colored(ICON_CLOCK, 14.0, scheme.text),
            text(self.elapsed().unwrap_or_default()).width(Length::Fixed(60.0)),
        ]
            .spacing(4)
            .align_y(iced::Alignment::Center);

        let file_name = self.current_file_name();
        let label = progress_label(
            &self.status,
            &self.stage,
            self.overall_shown,
            has_queue,
            // Advance the ellipses once every 0.5s (ticks are 100ms).
            1 + (self.activity_frame / 5) % 5,
            file_name.as_deref(),
        );
        // The file name leads the status, including while "Decoding" animates.
        // A fixed five-dot block keeps the left side stationary.
        let label_el: Element<Message> = if self.stage.eq_ignore_ascii_case("Decoding") {
            let active = 1 + (self.activity_frame / 5) % 5;
            let mut dots = vec![text::Span::new(".".repeat(active)).color(scheme.text)];
            if active < 5 {
                dots.push(
                    text::Span::new(".".repeat(5 - active)).color(Color::TRANSPARENT),
                );
            }
            let mut row = row![];
            if let Some(name) = &file_name {
                row = row.push(text(format!("{name} \u{2014} ")));
            }
            row = row.push(text("Decoding"));
            row = row.push(rich_text(dots));
            row.into()
        } else {
            text(label).into()
        };

        let status_row = row![
            clock,
            stack![
                progress_bar(0.0..=1.0, global_progress)
                .width(Length::Fill)
                .height(Length::Fixed(28.0))
                .style(move |_theme| progress_bar::Style {
                    background: scheme.bar_track.into(),
                    bar: scheme.blue.into(),
                    border: iced::border::rounded(14.0),
                }),
                container(label_el)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
            ]
            .width(Length::Fill),
        ]
            .spacing(2)
            .align_y(iced::Alignment::Center);

        let mut middle = column![status_row].spacing(8).width(Length::Fill);
        middle = middle.push(self.timeline_element(&scheme));
        // Imports stay available while a run is active: new files join the
        // queue and are picked up in a follow-up pass.
        let import: Element<Message> =
            pick_list(ImportKind::ALL, None::<ImportKind>, Message::ImportChoice)
                .placeholder("Import\u{2026}")
                .style(ink_pick(scheme.ink))
                .handle(caret_handle(false))
                .into();
        middle = middle.push(
            row![
                self.player_element(&scheme),
                action("Summary")
                .on_press_maybe(
                    (!running && self.has_results()).then_some(Message::OpenSummary)
                )
                .style(pill_button_secondary),
                self.tip(
                    action("Rename Speakers")
                    .on_press_maybe(
                        (!running
                            && self.shown_output().is_some()
                            && !self.shown_lines().is_empty())
                        .then_some(Message::OpenRename)
                    )
                    .style(pill_button_secondary),
                    "Rename speakers in this transcript",
                    &scheme,
                ),
                horizontal_space(),
                import,
            ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .width(Length::Fill),
        );

        let mut transcript_box = column![]
            .spacing(6)
            .width(Length::Fill)
            .height(Length::Fill);
        if let Some(view) = &self.selected_view {
            let name = view
                .audio
                .as_ref()
                .or(Some(&view.source))
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("transcript");
            let suffix = if view.output.is_none() && self.view_error.is_none() {
                " (read-only)"
            } else {
                ""
            };
            transcript_box = transcript_box.push(
                row![
                    icon_colored(ICON_CIRCLE_FILL, 8.0, scheme.blue),
                    text(format!("Viewing {name}{suffix}")).size(12),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .padding([0, 8]),
            );
        }
        transcript_box = transcript_box.push(self.transcript_element());

        let mut base = column![
            text("Transiel").size(32),
            queue_col,
            controls,
            middle,
            container(transcript_box)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(scheme.panel_bg.into()),
                border: iced::border::rounded(16.0),
                shadow: soft_shadow(),
                ..container::Style::default()
            }),
        ]
            .spacing(8)
            .padding(12);

        base = base.push(
            container(
                row![
                    self.search_element(&scheme),
                    horizontal_space(),
                    action("Copy")
                    .on_press_maybe(
                        (!self.shown_transcript().is_empty()).then_some(Message::CopyTranscript)
                    )
                    .style(pill_button_secondary),
                    action("Save")
                    .on_press_maybe(
                        (self.shown_output().is_some() && !self.shown_transcript().is_empty())
                        .then_some(Message::SaveTranscript)
                    )
                    .style(pill_button_secondary),
                    action("Reveal")
                    .on_press_maybe(
                        (self.shown_output().is_some()
                            || self.shown_audio().is_some()
                            || self.output_dir.is_some())
                        .then_some(Message::RevealOutput)
                    )
                    .style(pill_button_secondary),
                    pick_list(HelpKind::ALL, None::<HelpKind>, Message::HelpChoice)
                    .placeholder("Help\u{2026}")
                    .style(ink_pick(scheme.ink))
                    .handle(caret_handle(true)),
                ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
            )
                .width(Length::Fill),
        );

        if self.console_open {
            base = base.push(self.console_panel(&scheme));
        }

        if let Some((message, _)) = &self.toast {
            let toast = mouse_area(
                container(text(message.clone()).size(13))
                    .padding([8, 14])
                    .style(move |_theme| container::Style {
                        background: Some(scheme.blue.into()),
                        text_color: Some(scheme.ink),
                        border: iced::border::rounded(PILL),
                        shadow: soft_shadow(),
                        ..container::Style::default()
                    }),
            )
                .on_press(Message::DismissToast);
            let overlay = container(toast)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::End)
                .align_y(iced::Alignment::Start)
                .padding(20);
            return stack![base, overlay].into();
        }

        if let Some(edit) = &self.line_edit {
            let mut col = column![
                text("Edit line").size(20),
                column![
                    text("Speaker").size(12),
                    text_input("Speaker", &edit.speaker)
                        .on_input(Message::LineEditSpeaker)
                        .style(pill_input),
                ]
                .spacing(4),
                column![
                    text("Transcribed text").size(12),
                    text_editor(&edit.content)
                        .on_action(Message::LineEditAction)
                        .height(Length::Fixed(160.0))
                        .padding(6),
                ]
                .spacing(4),
            ]
            .spacing(10);
            col = col.push(
                container(
                    row![
                        action("Delete")
                        .on_press(Message::DeleteLineEdit)
                        .style(color_button(scheme.pink, scheme.ink)),
                        horizontal_space(),
                        action("Save")
                        .on_press(Message::SaveLineEdit)
                        .style(color_button(scheme.blue, scheme.ink)),
                        action("Cancel")
                        .on_press(Message::CancelLineEdit)
                        .style(pill_button_secondary),
                    ]
                    .spacing(8),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            );
            let dialog = container(col.width(Length::Fixed(560.0)))
                .padding(16)
                .style(move |_theme| container::Style {
                    background: Some(scheme.dialog_bg.into()),
                    border: iced::border::rounded(16.0),
                    shadow: dialog_shadow(),
                    ..container::Style::default()
                });
            return modal_overlay(base, dialog);
        }

        if self.summary_open {
            let lines = self.stats_lines();
            let mut body = column![text("Summary").size(20)].spacing(8);
            const FILE_INDENT: f32 = 16.0;
            const BODY_INDENT: f32 = 32.0;
            for line in &lines {
                match line {
                    SummaryLine::File(name) => {
                        // Dark, regular text: still larger than the body but
                        // not competing with the "Summary" title.
                        body = body.push(
                            container(text(name.clone()).size(17).color(scheme.text))
                                .width(Length::Fill)
                                .padding(iced::Padding {
                                    top: 0.0,
                                    right: 0.0,
                                    bottom: 0.0,
                                    left: FILE_INDENT,
                                }),
                        );
                    }
                    SummaryLine::Body(line) => {
                        body = body.push(
                            container(text(line.clone()))
                                .width(Length::Fill)
                                .padding(iced::Padding {
                                    top: 0.0,
                                    right: 0.0,
                                    bottom: 0.0,
                                    left: BODY_INDENT,
                                }),
                        );
                    }
                }
            }

            // Cap the scrollable height so long queues stay usable.
            let cap = (self.window_size.height * 0.8).max(300.0);
            let estimated = (lines.len() as f32 + 1.0) * 22.0 + 24.0;
            let height = estimated.min(cap);

            let scroll = scrollable(body)
                .id(summary_id())
                .width(Length::Fill)
                .height(Length::Fixed(height));

            let col = column![
                container(scroll).width(Length::Fill),
                container(
                    row![action("Close")
                        .on_press(Message::CloseSummary)
                        .style(color_button(scheme.blue, scheme.ink))]
                    .spacing(8),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            ]
                .spacing(8);

            let dialog = container(col.width(Length::Fixed(380.0)))
                .padding(16)
                .style(move |_theme| container::Style {
                    background: Some(scheme.dialog_bg.into()),
                    border: iced::border::rounded(16.0),
                    shadow: dialog_shadow(),
                    ..container::Style::default()
                });
            return modal_overlay(base, dialog);
        }

        if let Some(kind) = self.help_open {
            let mut col = column![text(kind.label()).size(20)].spacing(8);
            match kind {
                HelpKind::Shortcuts => {
                    // Key in a fixed-width column so the descriptions line up.
                    const KEY_WIDTH: f32 = 150.0;
                    for (key, description) in [
                        ("Ctrl+O", "Add files"),
                        ("Ctrl+F", "Find"),
                        ("Ctrl+Enter", "Transcribe"),
                        ("Ctrl + / - / 0", "Transcript font zoom"),
                        ("Space", "Play / pause"),
                        ("Left / Right", "Seek \u{00b1}5s"),
                        ("Esc", "Dismiss"),
                    ] {
                        col = col.push(
                            row![
                                text(key).width(Length::Fixed(KEY_WIDTH)),
                                text(description),
                            ]
                                .spacing(12),
                        );
                    }
                }
                HelpKind::About => {
                    for line in [
                        format!("transiel {}", env!("CARGO_PKG_VERSION")),
                        "Local speaker diarization + transcription.".to_string(),
                        "Diarization: pyannote segmentation + wespeaker (sherpa-onnx)."
                            .to_string(),
                        "Transcription: whisper.cpp.".to_string(),
                        "Runs fully offline once models are downloaded.".to_string(),
                    ] {
                        col = col.push(text(line));
                    }
                }
                HelpKind::Console => {}
            }
            col = col.push(
                container(
                    row![action("Close")
                        .on_press(Message::CloseHelp)
                        .style(color_button(scheme.blue, scheme.ink))]
                        .spacing(8),
                )
                    .width(Length::Fill)
                    .align_x(iced::Alignment::End),
            );
            let dialog = container(col.width(Length::Fixed(460.0)))
                .padding(16)
                .style(move |_theme| container::Style {
                    background: Some(scheme.dialog_bg.into()),
                    border: iced::border::rounded(16.0),
                    shadow: dialog_shadow(),
                    ..container::Style::default()
                });
            return modal_overlay(base, dialog);
        }

        if !self.rename_open {
            return base.into();
        }

        let mut col = column![text("Rename Speakers").size(20)].spacing(8);
        if self.rename_fields.is_empty() {
            col = col.push(text("No speakers to rename."));
        }
        for (index, (original, value)) in self.rename_fields.iter().enumerate() {
            col = col.push(
                row![
                    text(original.clone()).width(Length::Fixed(120.0)),
                    text_input(original.as_str(), value)
                    .id(rename_id(index))
                    .on_input(move |text| Message::RenameName(index, text))
                    .on_submit(Message::RenameSave)
                    .width(Length::Fill)
                    .style(pill_input),
                ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
            );
        }
        col = col.push(
            container(
                row![
                    action("Save")
                    .on_press(Message::RenameSave)
                    .style(color_button(scheme.blue, scheme.ink)),
                    action("Cancel")
                    .on_press(Message::RenameCancel)
                    .style(color_button(scheme.pink, scheme.ink)),
                ]
                    .spacing(8),
            )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
        );
        let dialog = container(col.width(Length::Fixed(380.0)))
            .padding(16)
            .style(move |_theme| container::Style {
                background: Some(scheme.dialog_bg.into()),
                border: iced::border::rounded(16.0),
                shadow: dialog_shadow(),
                ..container::Style::default()
            });

        modal_overlay(base, dialog)
    }
}

/// A speaker turn, resolved to a draw colour.
#[derive(Debug, Clone)]
struct TimelineTurn {
    start: f32,
    end: f32,
    color: Color,
}

#[derive(Default)]
struct TimelineState {
    scrubbing: bool,
}

/// Waveform + speaker-band timeline with a draggable playhead.
struct Timeline {
    peaks: Arc<Vec<f32>>,
    turns: Arc<Vec<TimelineTurn>>,
    duration: f32,
    playhead: f32,
    track: Color,
    wave: Color,
    playhead_color: Color,
    enabled: bool,
    zoom: f32,
}

impl Timeline {
    fn visible_left(&self) -> f32 {
        let z = self.zoom.max(1.0);
        let span = 1.0 / z;
        let center = if self.duration > 0.0 {
            (self.playhead / self.duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (center - span / 2.0).clamp(0.0, (1.0 - span).max(0.0))
    }

    fn x_of(&self, fraction: f32, width: f32) -> f32 {
        let span = 1.0 / self.zoom.max(1.0);
        ((fraction - self.visible_left()) / span) * width
    }

    fn time_at(&self, x: f32, width: f32) -> f32 {
        if width <= 0.0 || self.duration <= 0.0 {
            return 0.0;
        }
        let span = 1.0 / self.zoom.max(1.0);
        let fraction = (self.visible_left() + (x / width) * span).clamp(0.0, 1.0);
        fraction * self.duration
    }
}

impl canvas::Program<Message> for Timeline {
    type State = TimelineState;

    fn update(
        &self,
        state: &mut TimelineState,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        use canvas::event::Status;
        if !self.enabled {
            return (Status::Ignored, None);
        }
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(position) = cursor.position_in(bounds) {
                    state.scrubbing = true;
                    return (
                        Status::Captured,
                        Some(Message::Seek(self.time_at(position.x, bounds.width))),
                    );
                }
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.scrubbing = false;
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if state.scrubbing {
                    if let Some(position) = cursor.position_in(bounds) {
                        return (
                            Status::Captured,
                            Some(Message::Seek(self.time_at(position.x, bounds.width))),
                        );
                    }
                }
            }
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta })
                if cursor.position_in(bounds).is_some() =>
            {
                let amount = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / 40.0,
                };
                if amount != 0.0 {
                    let next = (self.zoom * (1.0 + 0.15 * amount)).clamp(1.0, 16.0);
                    return (Status::Captured, Some(Message::WaveZoom(next)));
                }
            }
            _ => {}
        }
        (Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &TimelineState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let width = bounds.width;
        let height = bounds.height;

        frame.fill_rectangle(Point::ORIGIN, bounds.size(), self.track);

        let band_height = (height * 0.16).clamp(6.0, 12.0);
        let wave_area = height - band_height - 4.0;
        let mid = wave_area / 2.0;

        let count = self.peaks.len();
        if count > 0 {
            let peak_max = self
                .peaks
                .iter()
                .copied()
                .fold(0.0_f32, f32::max)
                .max(1e-6);
            let half = mid * 0.92;
            let span = 1.0 / self.zoom.max(1.0);
            let left = self.visible_left();
            let i0 = ((left * count as f32).floor() as isize - 1).max(0) as usize;
            let i1 = (((left + span) * count as f32).ceil() as usize + 1).min(count);
            let bar = (width / (count as f32 * span)).max(0.7);
            for index in i0..i1 {
                let fraction = index as f32 / count as f32;
                let x = (fraction - left) / span * width;
                if x < -bar || x > width {
                    continue;
                }
                let amplitude = (self.peaks[index] / peak_max).clamp(0.0, 1.0) * half;
                if amplitude <= 0.0 {
                    continue;
                }
                frame.fill_rectangle(
                    Point::new(x, mid - amplitude),
                    Size::new(bar, amplitude * 2.0),
                    self.wave,
                );
            }
        }

        if self.duration > 0.0 {
            for turn in self.turns.iter() {
                let x0 = self.x_of(turn.start / self.duration, width);
                let x1 = self.x_of(turn.end / self.duration, width);
                if x1 < 0.0 || x0 > width {
                    continue;
                }
                let left = x0.max(0.0);
                let right = x1.min(width);
                frame.fill_rectangle(
                    Point::new(left, height - band_height),
                    Size::new((right - left).max(1.0), band_height),
                    turn.color,
                );
            }
            let x = self.x_of((self.playhead / self.duration).clamp(0.0, 1.0), width);
            frame.fill_rectangle(
                Point::new((x - 1.0).max(0.0), 0.0),
                Size::new(2.0, height),
                self.playhead_color,
            );
        }

        vec![frame.into_geometry()]
    }
}

fn speaker_palette(index: usize, scheme: &Scheme) -> Color {
    const EXTRA: [(f32, f32, f32); 8] = [
        (0.66, 0.62, 0.98), // violet
        (0.45, 0.82, 0.62), // green
        (0.98, 0.78, 0.42), // amber
        (0.55, 0.85, 0.88), // teal
        (0.96, 0.58, 0.42), // coral
        (0.58, 0.66, 0.98), // periwinkle
        (0.78, 0.88, 0.48), // lime
        (0.90, 0.70, 0.97), // lavender
    ];
    match index {
        0 => scheme.sms0,
        1 => scheme.sms1,
        other => {
            let (r, g, b) = EXTRA[(other - 2) % EXTRA.len()];
            Color::from_rgb(r, g, b)
        }
    }
}

/// Audio playback through rodio; the `OutputStream` must stay alive.
struct Player {
    _stream: OutputStream,
    sink: Sink,
    path: PathBuf,
}

impl Player {
    fn new(path: &Path) -> Option<Player> {
        let (stream, handle) = OutputStream::try_default().ok()?;
        let sink = Sink::try_new(&handle).ok()?;
        let decoder = Self::decoder(path)?;
        sink.append(decoder);
        sink.pause();
        Some(Player {
            _stream: stream,
            sink,
            path: path.to_path_buf(),
        })
    }

    fn decoder(path: &Path) -> Option<Decoder<std::io::BufReader<std::fs::File>>> {
        let file = std::fs::File::open(path).ok()?;
        Decoder::new(std::io::BufReader::new(file)).ok()
    }

    /// Re-append the source after it has been fully consumed, so the sink is
    /// seekable again instead of staying stuck at the end.
    fn rebuild_if_empty(&self) {
        if self.sink.empty() {
            if let Some(decoder) = Self::decoder(&self.path) {
                self.sink.append(decoder);
                self.sink.pause();
            }
        }
    }

    fn is_playing(&self) -> bool {
        !self.sink.is_paused() && !self.sink.empty()
    }

    fn finished(&self) -> bool {
        self.sink.empty()
    }

    fn toggle(&self) {
        self.rebuild_if_empty();
        if self.sink.empty() {
            return;
        }
        if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    /// Seek to an audio-timeline position. rodio's `try_seek` works in
    /// real-time units (it multiplies by the speed factor internally), so the
    /// requested audio position is divided by the current speed.
    fn seek(&self, seconds: f32, speed: f32) {
        self.rebuild_if_empty();
        let real = seconds.max(0.0) / speed.max(0.01);
        let _ = self.sink.try_seek(Duration::from_secs_f32(real));
    }

    fn set_speed(&self, factor: f32) {
        self.sink.set_speed(factor.max(0.1));
    }
}

/// A Bootstrap Icons glyph rendered as an element.
fn icon(code: char, size: f32) -> Element<'static, Message> {
    text(code.to_string())
        .font(Font::with_name("bootstrap-icons"))
        .size(size)
        .into()
}

/// A Bootstrap Icons glyph with an explicit colour.
fn icon_colored(code: char, size: f32, color: Color) -> Element<'static, Message> {
    text(code.to_string())
        .font(Font::with_name("bootstrap-icons"))
        .size(size)
        .style(move |_theme| text::Style { color: Some(color) })
        .into()
}

/// Queue status indicator: a glyph on a filled circular badge, so the running
/// arrow and completed check stand out without changing the row text size.
fn queue_status_badge(
    glyph: char,
    glyph_size: f32,
    background: Color,
    foreground: Color,
) -> Element<'static, Message> {
    container(icon_colored(glyph, glyph_size, foreground))
        .width(Length::Fixed(26.0))
        .height(Length::Fixed(26.0))
        .center_x(Length::Fixed(26.0))
        .center_y(Length::Fixed(26.0))
        .style(move |_theme| container::Style {
            background: Some(background.into()),
            border: iced::border::rounded(13.0),
            ..container::Style::default()
        })
        .into()
}

/// A status glyph without a filled badge (pending items: an empty ring).
fn queue_status_icon(glyph: char, glyph_size: f32, color: Color) -> Element<'static, Message> {
    container(icon_colored(glyph, glyph_size, color))
        .width(Length::Fixed(26.0))
        .height(Length::Fixed(26.0))
        .center_x(Length::Fixed(26.0))
        .center_y(Length::Fixed(26.0))
        .into()
}

/// A dynamic pick-list handle: caret points down when closed (or up when
/// `invert` is set), flipping when the menu opens.
fn caret_handle(invert: bool) -> pick_list::Handle<Font> {
    let icon = |code: char| pick_list::Icon {
        font: Font::with_name("bootstrap-icons"),
        code_point: code,
        size: Some(Pixels(12.0)),
        line_height: iced::advanced::text::LineHeight::default(),
        shaping: iced::advanced::text::Shaping::Basic,
    };
    let (closed, open) = if invert {
        (icon(ICON_CARET_UP), icon(ICON_CARET_DOWN))
    } else {
        (icon(ICON_CARET_DOWN), icon(ICON_CARET_UP))
    };
    pick_list::Handle::Dynamic { closed, open }
}

/// Pick-list style whose text/placeholder/handle use `ink` (button text colour).
fn ink_pick(ink: Color) -> impl Fn(&iced::Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let mut style = pick_list::default(theme, status);
        style.text_color = ink;
        style.placeholder_color = ink;
        style.handle_color = ink;
        style.border.radius = PILL.into();
        style
    }
}

// Bootstrap Icons codepoints (see assets/bootstrap-icons.json).
const ICON_PLAY: char = '\u{f4f4}';
const ICON_PAUSE: char = '\u{f4c3}';
const ICON_ARROW_UP: char = '\u{f148}';
const ICON_ARROW_DOWN: char = '\u{f128}';
const ICON_CIRCLE_FILL: char = '\u{f287}';
const ICON_CIRCLE: char = '\u{f28a}';
const ICON_HOURGLASS: char = '\u{f421}';
const ICON_CHECK: char = '\u{f633}';
const ICON_X: char = '\u{f659}';
const ICON_CARET_DOWN: char = '\u{f229}';
const ICON_CARET_UP: char = '\u{f235}';
const ICON_ZOOM_IN: char = '\u{f62c}';
const ICON_ZOOM_OUT: char = '\u{f62d}';
const ICON_CLOCK: char = '\u{f293}';

/// Layer `dialog` over `base` behind a dimming backdrop that swallows clicks,
/// so a visible modal blocks everything outside it.
fn modal_overlay<'a>(
    base: impl Into<Element<'a, Message>>,
    dialog: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let base = base.into();
    let dialog = dialog.into();
    let backdrop = mouse_area(
        container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.4).into()),
                ..container::Style::default()
            }),
    )
        .on_press(Message::Noop);
    // The dialog itself also consumes stray clicks and keeps an arrow cursor
    // on its non-interactive areas, so nothing behind it is clickable.
    let dialog = mouse_area(dialog)
        .interaction(mouse::Interaction::Idle)
        .on_press(Message::Noop);
    let centered = container(dialog)
        .center(Length::Fill)
        .width(Length::Fill)
        .height(Length::Fill);
    stack![base, backdrop, centered].into()
}

/// Parse "[time] Speaker: text" transcript lines into the render model.
/// The spoken text of a rendered line, without its timestamp/speaker prefix.
fn line_text(line: &DocLine) -> String {
    let mut rest = line.raw.as_str();
    if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            rest = stripped[end + 1..].trim_start();
        }
    }
    if !line.speaker.is_empty() {
        if let Some(after) = rest.strip_prefix(&format!("{}: ", line.speaker)) {
            return after.to_string();
        }
        if let Some(after) = rest.strip_prefix(&format!("{}:", line.speaker)) {
            return after.trim_start().to_string();
        }
    }
    rest.to_string()
}

/// The `[timestamp] ` prefix of a rendered line, if present.
fn line_prefix(raw: &str) -> &str {
    if !raw.starts_with('[') {
        return "";
    }
    match raw.find(']') {
        Some(end) => &raw[..(end + 2).min(raw.len())],
        None => "",
    }
}

/// Rewrite one line in place, preserving its timestamp and changing only the
/// speaker and text.
fn rewrite_line(line: &mut DocLine, speaker: &str, text: &str) {
    let prefix = line_prefix(&line.raw).to_string();
    line.raw = if speaker.is_empty() {
        format!("{prefix}{text}")
    } else {
        format!("{prefix}{speaker}: {text}")
    };
    line.speaker = speaker.to_string();
}

fn parse_lines(text: &str) -> Vec<DocLine> {
    text.lines()
        .map(|line| DocLine {
            raw: line.to_string(),
            speaker: speaker_of(line),
            start: parse_time_prefix(line),
        })
        .collect()
}

/// Parse a leading "[mm:ss]" / "[h:mm:ss]" timestamp into seconds.
fn parse_time_prefix(raw: &str) -> Option<f32> {
    let inner = raw.strip_prefix('[')?;
    let end = inner.find(']')?;
    let stamp = &inner[..end];
    let mut seconds = 0.0f32;
    let mut seen = false;
    for part in stamp.split(':') {
        let value: f32 = part.trim().parse().ok()?;
        seconds = seconds * 60.0 + value;
        seen = true;
    }
    seen.then_some(seconds)
}

/// Extract the speaker label from a rendered transcript line.
fn speaker_of(raw: &str) -> String {
    let rest = match raw.find(']') {
        Some(index) if raw.starts_with('[') => &raw[index + 1..],
        _ => raw,
    };
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    match rest.split_once(": ") {
        Some((head, _)) => head.trim().to_string(),
        None => String::new(),
    }
}

/// Progress-bar text: phase (with detail) plus overall percentage.
fn progress_label(
    status: &Status,
    stage: &str,
    progress: f32,
    has_queue: bool,
    dots: usize,
    file: Option<&str>,
) -> String {
    let mut label = match status {
        Status::Idle if has_queue => "Ready".to_string(),
        Status::Idle => "Idle".to_string(),
        Status::Error(message) => format!("Error: {message}"),
        _ => {
            let label = capitalize_first(stage);
            let lower = label.to_ascii_lowercase();
            // Animate ellipses while decoding (1..=5, cycling).
            if lower == "decoding" {
                format!("{label}{}", ".".repeat(dots.clamp(1, 5)))
            } else if lower == "done" {
                // Keep saying "Transcribing" while the eased bar catches up;
                // only call the run completed once the bar has reached 100%.
                if matches!(status, Status::Done) && progress >= 0.995 {
                    format!("Completed ({:.0}%)", progress.clamp(0.0, 1.0) * 100.0)
                } else {
                    format!("Transcribing ({:.0}%)", progress.clamp(0.0, 1.0) * 100.0)
                }
            } else if lower.starts_with("transcribing") {
                format!("{label} ({:.0}%)", progress.clamp(0.0, 1.0) * 100.0)
            } else {
                label
            }
        }
    };
    if let Some(file) = file {
        label = format!("{file} \u{2014} {label}");
    }
    label
}

fn capitalize_first(text: &str) -> String {
    match text.chars().next() {
        Some(first) => first.to_uppercase().collect::<String>() + &text[first.len_utf8()..],
        None => String::new(),
    }
}

fn format_secs(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Bracketed absolute timestamp matching the backend output, e.g. "[03:07]".
fn format_timestamp(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("[{hours:02}:{minutes:02}:{seconds:02}]")
    } else {
        format!("[{minutes:02}:{seconds:02}]")
    }
}

fn find_id() -> text_input::Id {
    text_input::Id::new("find-query")
}

fn transcript_id() -> scrollable::Id {
    scrollable::Id::new("transcript")
}

fn console_id() -> scrollable::Id {
    scrollable::Id::new("console")
}

fn summary_id() -> scrollable::Id {
    scrollable::Id::new("summary")
}

fn rename_id(index: usize) -> text_input::Id {
    text_input::Id::new(format!("rename-{index}"))
}

fn scrollbar_style(theme: &iced::Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let visible = matches!(
        status,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    style.vertical_rail.scroller.color = if visible {
        Color::from_rgba(0.0, 0.0, 0.0, 0.5)
    } else {
        Color::TRANSPARENT
    };
    style
}

/// Queue scrollbar: always visible when the list overflows.
fn queue_scrollbar_style(theme: &iced::Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let active = matches!(
        status,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    style.vertical_rail.scroller.color = if active {
        Color::from_rgba(0.0, 0.0, 0.0, 0.55)
    } else {
        Color::from_rgba(0.0, 0.0, 0.0, 0.28)
    };
    style
}

fn sms_row(
    content: &str,
    left: bool,
    color: Color,
    ink: Color,
    tint: Option<Color>,
    query: &str,
    highlight: Color,
    size: f32,
    active: bool,
) -> Element<'static, Message> {
    let background = tint.unwrap_or(color);
    let border = if active {
        iced::border::Border {
            color: ink,
            width: 2.0,
            radius: 8.0.into(),
        }
    } else {
        iced::border::rounded(8.0)
    };
    let bubble = container(rich_text(styled_spans(content, query, Some(ink), highlight, size)))
        .width(Length::Fill)
        .align_x(iced::Alignment::Start)
        .padding(6)
        .style(move |_theme| container::Style {
            background: Some(background.into()),
            border,
            ..container::Style::default()
        });
    if left {
        row![bubble, horizontal_space()].spacing(8).into()
    } else {
        row![horizontal_space(), bubble].spacing(8).into()
    }
}

/// A transcript line with an optional timestamp gutter, a clickable speaker
/// label (rename) and a clickable body (select for merge/split).
#[allow(clippy::too_many_arguments)]
fn line_element(
    line: &DocLine,
    idx: usize,
    tint: Option<Color>,
    query: &str,
    highlight: Color,
    scheme: &Scheme,
    size: f32,
    active: bool,
) -> Element<'static, Message> {
    let bold = Font {
        weight: font::Weight::Bold,
        ..Font::default()
    };
    let ink = scheme.ink;

    let speaker = line.speaker.clone();
    let pos = if speaker.is_empty() {
        0
    } else {
        line.raw.find(&speaker).unwrap_or(0)
    };
    let rest = if speaker.is_empty() {
        line.raw.clone()
    } else {
        let after = &line.raw[pos + speaker.len()..];
        after.strip_prefix(": ").unwrap_or(after).to_string()
    };
    let stamp = line
        .start
        .map(|s| format_timestamp(s as f64))
        .unwrap_or_default();

    let mut content = row![]
        .spacing(6)
        .align_y(iced::Alignment::Start)
        .width(Length::Fill);

    if !stamp.is_empty() {
        content = content.push(
            text(stamp)
                .size(size * 0.82)
                .style(move |_theme| text::Style { color: Some(ink) }),
        );
    }

    if !speaker.is_empty() {
        content = content.push(text(speaker).size(size).font(bold).color(ink));
    }

    content = content.push(rich_text(styled_spans(&rest, query, Some(ink), highlight, size)));
    content = content.push(horizontal_space());

    let border = if active {
        iced::border::Border {
            color: ink,
            width: 2.0,
            radius: 4.0.into(),
        }
    } else {
        iced::border::rounded(4.0)
    };
    stack![
        container(content)
        .width(Length::Fill)
        .padding([2, 4])
        .style(move |_theme| container::Style {
            background: tint.map(Into::into),
            border,
            ..container::Style::default()
        }),
        mouse_area(
            container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fill),
        )
        .interaction(mouse::Interaction::Pointer)
        .on_press(Message::SelectLine(idx))
        .on_right_press(Message::EditLine(idx)),
    ]
        .into()
}

const PILL: f32 = 1000.0;

/// A button (content-sized / minimum width).
fn action<'a>(content: impl Into<Element<'a, Message>>) -> iced::widget::Button<'a, Message> {
    button(content)
}

/// Fire a best-effort desktop notification (non-blocking).
fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary(&title)
            .body(&body)
            .show();
    });
}

/// A subtle drop shadow that gives surfaces a soft, raised look.
fn soft_shadow() -> Shadow {
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.20),
        offset: Vector::new(0.0, 2.0),
        blur_radius: 8.0,
    }
}

/// A larger shadow for floating dialogs.
fn dialog_shadow() -> Shadow {
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.35),
        offset: Vector::new(0.0, 6.0),
        blur_radius: 24.0,
    }
}

fn pill_button_secondary(theme: &iced::Theme, status: button::Status) -> button::Style {
    let mut style = button::secondary(theme, status);
    style.border.radius = PILL.into();
    style.shadow = soft_shadow();
    style
}

fn pill_input(theme: &iced::Theme, status: text_input::Status) -> text_input::Style {
    let mut style = text_input::default(theme, status);
    style.border.radius = PILL.into();
    style
}

fn pill_pick(theme: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    let mut style = pick_list::default(theme, status);
    style.border.radius = PILL.into();
    style
}

/// Borderless pick list whose colours mirror the secondary (Advanced) button.
fn speed_pick(theme: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    let button_status = match status {
        pick_list::Status::Hovered | pick_list::Status::Opened => button::Status::Hovered,
        pick_list::Status::Active => button::Status::Active,
    };
    let base = button::secondary(theme, button_status);
    pick_list::Style {
        text_color: base.text_color,
        placeholder_color: base.text_color,
        handle_color: base.text_color,
        background: base
            .background
            .unwrap_or_else(|| Color::TRANSPARENT.into()),
        border: iced::border::Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: PILL.into(),
        },
    }
}

/// Muted colour used for controls that are visible but unavailable.
const MUTED: Color = Color {
    r: 0.6,
    g: 0.6,
    b: 0.64,
    a: 1.0,
};

fn greyed_pick(theme: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    let mut style = pick_list::default(theme, status);
    style.text_color = MUTED;
    style.handle_color = MUTED;
    style.background = Color::from_rgb(0.88, 0.88, 0.93).into();
    style.border = iced::border::Border {
        color: Color::TRANSPARENT,
        width: 0.0,
        radius: PILL.into(),
    };
    style
}

fn greyed_button(theme: &iced::Theme, status: button::Status) -> button::Style {
    let mut style = button::secondary(theme, status);
    style.background = Some(Color::from_rgba(0.6, 0.6, 0.64, 0.12).into());
    style.text_color = MUTED;
    style.border.radius = PILL.into();
    style
}

fn color_button(bg: Color, ink: Color) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let (bg, text) = match status {
            button::Status::Hovered => (Color { a: 0.85, ..bg }, ink),
            button::Status::Disabled => (Color { a: 0.35, ..bg }, Color { a: 0.6, ..ink }),
            _ => (bg, ink),
        };
        button::Style {
            background: Some(bg.into()),
            text_color: text,
            border: iced::border::rounded(PILL),
            shadow: soft_shadow(),
            ..button::Style::default()
        }
    }
}

fn styled_spans(
    raw: &str,
    query: &str,
    color: Option<Color>,
    highlight: Color,
    size: f32,
) -> Vec<text::Span<'static, Message>> {
    let bold = Font {
        weight: font::Weight::Bold,
        ..Font::default()
    };
    let head_len = raw.find(": ").map(|index| index + 2).unwrap_or(0);
    let needle = query.to_lowercase();

    let mut matches: Vec<(usize, usize)> = Vec::new();
    if !needle.is_empty() {
        let lower = raw.to_lowercase();
        let mut from = 0usize;
        while let Some(pos) = lower[from..].find(needle.as_str()) {
            let start = from + pos;
            let end = start + needle.len();
            matches.push((start, end));
            from = end.max(start + 1);
            if from >= raw.len() {
                break;
            }
        }
    }

    let mut bounds = vec![0usize, raw.len()];
    if head_len <= raw.len() {
        bounds.push(head_len);
    }
    for (start, end) in &matches {
        bounds.push(*start);
        bounds.push(*end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut spans = Vec::new();
    for window in bounds.windows(2) {
        let (a, b) = (window[0], window[1]);
        if a >= b || !raw.is_char_boundary(a) || !raw.is_char_boundary(b) {
            continue;
        }
        let mut span = text::Span::new(raw[a..b].to_string()).size(size);
        if a < head_len {
            span = span.font(bold);
        }
        if let Some(c) = color {
            span = span.color(c);
        }
        if matches.iter().any(|(s, e)| a >= *s && b <= *e) {
            span = span.background(highlight);
        }
        spans.push(span);
    }
    if spans.is_empty() {
        spans.push(text::Span::new(raw.to_string()).size(size));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate the process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the environment lock even after another test has panicked.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "transiel-app-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Points both the Windows and Unix config lookups at a temporary
    /// directory, restoring the process environment when dropped.
    struct ConfigEnv {
        dir: PathBuf,
        xdg: Option<std::ffi::OsString>,
        appdata: Option<std::ffi::OsString>,
    }

    impl ConfigEnv {
        fn new(tag: &str) -> Self {
            let dir = temp_dir(tag);
            let xdg = std::env::var_os("XDG_CONFIG_HOME");
            let appdata = std::env::var_os("APPDATA");
            // `settings_path` uses APPDATA on Windows and XDG_CONFIG_HOME
            // elsewhere, so set both to keep tests hermetic on every platform.
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            std::env::set_var("APPDATA", &dir);
            Self { dir, xdg, appdata }
        }
    }

    impl Drop for ConfigEnv {
        fn drop(&mut self) {
            restore_env("XDG_CONFIG_HOME", self.xdg.take());
            restore_env("APPDATA", self.appdata.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn queue_state_round_trips() {
        let _guard = env_lock();
        let config = ConfigEnv::new("queue");
        let dir = config.dir.clone();

        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = dir.join("talk-transcript.txt");
        std::fs::write(&transcript, "[00:00] SPEAKER 1: hi\n").unwrap();

        let mut app = App::new();
        assert!(app.queue.is_empty());
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio.clone()),
            status: ItemStatus::Done,
            duration: Some(1.5),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.1, 0.2, 0.3]),
        });
        app.save_queue();

        let loaded = App::load_queue();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].audio.as_deref(), Some(audio.as_path()));
        assert_eq!(loaded[0].transcript.as_deref(), Some(transcript.as_path()));
        assert!(matches!(loaded[0].status, ItemStatus::Done));
        assert_eq!(loaded[0].duration, Some(1.5));
        assert_eq!(loaded[0].peaks.as_ref(), &vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn matching_audio_finds_same_stem() {
        let dir = temp_dir("matching");
        let transcript = dir.join("interview.txt");
        std::fs::write(&transcript, "").unwrap();
        std::fs::write(dir.join("interview.mp3"), b"audio").unwrap();
        std::fs::write(dir.join("other.wav"), b"audio").unwrap();

        let matched = App::matching_audio(&transcript).unwrap();
        assert_eq!(
            matched.file_name().and_then(|name| name.to_str()),
            Some("interview.mp3")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn viewing_and_renaming_a_completed_item() {
        let _guard = env_lock();
        let config = ConfigEnv::new("view");
        let dir = config.dir.clone();

        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = dir.join("talk-transcript.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello\n[00:05] SPEAKER 2: hi\n",
        )
        .unwrap();

        let mut app = App::new();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio.clone()),
            status: ItemStatus::Done,
            duration: Some(6.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });

        // Clicking the completed row opens its transcript.
        let _ = app.view_item(0);
        assert_eq!(app.selected, Some(0));
        assert_eq!(app.shown_lines().len(), 2);
        assert!(app
            .shown_transcript()
            .contains("SPEAKER 1: hello"));

        // The view renders without panicking.
        let _ = app.view();

        // Renaming writes back to the item's transcript file.
        app.rename_fields = vec![("SPEAKER 1".to_string(), "ALICE".to_string())];
        app.apply_rename();
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("ALICE: hello"));
        assert!(!saved.contains("SPEAKER 1"));

        // A missing transcript surfaces the inline error state.
        std::fs::remove_file(&transcript).unwrap();
        app.return_to_live();
        let _ = app.view_item(0);
        assert!(app.view_error.is_some());
        assert!(app.shown_lines().is_empty());
    }

    #[test]
    fn importing_a_transcript_adds_a_completed_item() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("import-config");
        let media = temp_dir("import-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        let text = "[00:00] SPEAKER 1: imported line\n";
        std::fs::write(&transcript, text).unwrap();

        let mut app = App::new();
        assert!(app.queue.is_empty());
        let _ = app.update(Message::ImportLoaded(Some((
            transcript.clone(),
            text.to_string(),
        ))));

        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].audio.as_deref(), Some(audio.as_path()));
        assert!(matches!(app.queue[0].status, ItemStatus::Done));
        assert_eq!(app.selected, Some(0));
        assert!(app.shown_transcript().contains("imported line"));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn editing_one_line_changes_only_that_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("edit-config");
        let media = temp_dir("edit-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello\n[00:05] SPEAKER 2: hi\n",
        )
        .unwrap();

        let mut app = App::new();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(6.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });
        let _ = app.view_item(0);

        let _ = app.update(Message::EditLine(0));
        {
            let edit = app.line_edit.as_mut().unwrap();
            assert_eq!(edit.speaker, "SPEAKER 1");
            assert_eq!(edit.content.text().trim_end(), "hello");
            edit.speaker = "ALICE".to_string();
            edit.content = text_editor::Content::with_text("hello there");
        }
        // The editor dialog renders without panicking.
        let _ = app.view();

        let _ = app.update(Message::SaveLineEdit);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("[00:00] ALICE: hello there"));
        assert!(saved.contains("[00:05] SPEAKER 2: hi"));

        // Enter saves the form from the line editor.
        let _ = app.update(Message::EditLine(1));
        app.line_edit.as_mut().unwrap().content =
            text_editor::Content::with_text("hi there");
        let _ = app.update(Message::EnterSubmit);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("[00:05] SPEAKER 2: hi there"));

        // Delete removes the edited row.
        let _ = app.update(Message::EditLine(0));
        let _ = app.update(Message::DeleteLineEdit);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(!saved.contains("ALICE: hello there"));
        assert!(saved.contains("SPEAKER 2: hi there"));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn player_ready_for_completed_items_during_a_run() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("player-config");
        let media = temp_dir("player-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(&transcript, "[00:00] SPEAKER 1: hello\n").unwrap();

        let mut app = App::new();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio.clone()),
            status: ItemStatus::Done,
            duration: Some(6.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.1; 16]),
        });

        // The live file is not playable while it is being transcribed.
        app.status = Status::Running;
        app.peaks_for = Some(audio);
        assert!(!app.player_ready());

        // A completed item stays playable during that same run.
        let _ = app.view_item(0);
        assert!(app.player_ready());

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn follow_toggle_is_user_controlled_during_a_run() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-config");

        let mut app = App::new();
        app.status = Status::Running;
        app.follow_playhead = true;
        let _ = app.update(Message::ToggleFollowPlayhead);
        assert!(!app.follow_playhead);
    }

    #[test]
    fn scrolling_moves_the_playhead_to_the_visible_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("scroll-config");
        let media = temp_dir("scroll-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: first\n[00:05] SPEAKER 2: second\n",
        )
        .unwrap();

        let mut app = App::new();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(10.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });
        let _ = app.view_item(0);

        // Two lines over a scrollable range: the top maps to the first line
        // and the bottom maps to the last line.
        app.transcript_content = 300.0;
        app.seek_from_scroll(0.0, 100.0);
        assert_eq!(app.playhead, 0.0);
        app.seek_from_scroll(200.0, 100.0);
        assert_eq!(app.playhead, 5.0);
        assert!(app.scroll_seek);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn preflight_error_keeps_pending_items_retryable() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("preflight-config");

        let mut app = App::new();
        let audio = PathBuf::from("/tmp/audio.mp3");
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Pending,
            duration: None,
            transcript: None,
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });
        app.batch = true;
        app.status = Status::Running;
        app.events.lock().unwrap().push_back(Event::Error {
            message: "Accuracy model unavailable — run: transiel --cli models download"
                .to_string(),
        });

        let _ = app.drain_events();
        assert!(matches!(app.queue[0].status, ItemStatus::Pending));
        assert!(matches!(app.status, Status::Error(_)));
        assert!(!app.batch);
    }

    #[test]
    fn palette_has_distinct_colors_for_many_speakers() {
        let scheme = Scheme::light();
        let colors: Vec<Color> = (0..10).map(|index| speaker_palette(index, &scheme)).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "speakers {i} and {j} share a color");
            }
        }
    }

    #[test]
    fn done_label_waits_for_the_bar() {
        assert_eq!(
            progress_label(&Status::Running, "done", 0.9, true, 1, None),
            "Transcribing (90%)"
        );
        assert_eq!(
            progress_label(&Status::Done, "done", 0.9, true, 1, None),
            "Transcribing (90%)"
        );
        assert_eq!(
            progress_label(&Status::Done, "done", 1.0, true, 1, None),
            "Completed (100%)"
        );
        assert_eq!(
            progress_label(&Status::Done, "transcribing 3/4", 0.75, true, 1, Some("talk.mp3")),
            "talk.mp3 \u{2014} Transcribing 3/4 (75%)"
        );
    }
}
