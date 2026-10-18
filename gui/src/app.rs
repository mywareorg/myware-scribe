use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, Modifiers};
use serde::{Deserialize, Serialize};
use iced::widget::{
    button, canvas, column, container, horizontal_space, mouse_area, pick_list, progress_bar,
    rich_text, row, scrollable, stack, text, text_input, toggler, tooltip,
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
    Done(PathBuf),
    Failed(String),
}

#[derive(Debug, Clone)]
struct QueueItem {
    path: PathBuf,
    status: ItemStatus,
    duration: Option<f64>,
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
    confidence: f32,
    start: Option<f32>,
}

impl DocLine {
    fn low_confidence(&self) -> bool {
        self.confidence < CONFIDENCE_MIN
    }
}

/// Lines at or below this average log-probability are flagged as low confidence.
const CONFIDENCE_MIN: f32 = -0.6;

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
    timestamps: Option<bool>,
    join_speaker: Option<bool>,
    sms_mode: Option<bool>,
    dark_mode: Option<bool>,
    num_speakers: Option<i32>,
    confidence: Option<bool>,
    font_scale: Option<f32>,
    playback_speed: Option<f32>,
    waveform_zoom: Option<f32>,
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
    warn_tint_alt: Color,
    search_hl: Color,
    search_active: Color,
    tip_bg: Color,
    tip_text: Color,
}

impl Scheme {
    fn for_theme(dark: bool) -> Scheme {
        if dark {
            Scheme {
                panel_bg: Color::from_rgb(0.094, 0.094, 0.145),
                dialog_bg: Color::from_rgb(0.118, 0.118, 0.180),
                track: Color::from_rgb(0.067, 0.067, 0.106),
                bar_track: Color::from_rgba(0.357, 0.808, 0.980, 0.24),
                timeline_bg: Color::from_rgb(0.22, 0.22, 0.26),
                blue: Color::from_rgb(0.357, 0.808, 0.980),
                pink: Color::from_rgb(0.961, 0.663, 0.722),
                green: Color::from_rgb(0.30, 0.72, 0.42),
                ink: Color::from_rgb(0.067, 0.067, 0.106),
                text: Color::from_rgb(0.93, 0.94, 0.98),
                sms0: Color::from_rgb(0.357, 0.808, 0.980),
                sms1: Color::from_rgb(0.961, 0.663, 0.722),
                warn_tint: Color::from_rgba(0.537, 0.749, 1.0, 0.30),
                warn_tint_alt: Color::from_rgba(0.953, 0.545, 0.659, 0.16),
                search_hl: Color::from_rgba(1.0, 0.85, 0.35, 0.35),
                search_active: Color::from_rgba(1.0, 0.72, 0.25, 0.65),
                tip_bg: Color::from_rgb(0.24, 0.24, 0.32),
                tip_text: Color::from_rgb(0.93, 0.94, 0.98),
            }
        } else {
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
                warn_tint_alt: Color::from_rgba(0.961, 0.663, 0.722, 0.22),
                search_hl: Color::from_rgba(1.0, 0.85, 0.3, 0.5),
                search_active: Color::from_rgba(1.0, 0.72, 0.2, 0.8),
                tip_bg: Color::from_rgb(0.99, 0.99, 1.0),
                tip_text: Color::from_rgb(0.13, 0.13, 0.17),
            }
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
    TimestampsToggled(bool),
    JoinSpeakerToggled(bool),
    ToggleOptions,
    SmsToggled(bool),
    DarkToggled(bool),
    SpeakerCountChanged(SpeakerOpt),
    ConfidenceToggled(bool),
    TranscriptScrolled(scrollable::Viewport),
    WindowEvent(iced::window::Event),
    TimelineLoaded(PathBuf, Option<(f64, Vec<f32>)>),
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
    ImportLoaded(Option<String>),
    ImportChoice(ImportKind),
    CopyTranscript,
    SaveTranscript,
    RevealOutput,
    AudioPicked(Option<PathBuf>),
    OpenRename,
    CloseSummary,
    AddFiles,
    FilesPicked(Option<Vec<PathBuf>>),
    AddFolder,
    FolderPicked(Option<PathBuf>),
    RemoveItem(usize),
    ClearQueue,
    BatchContinue,
    BatchCancel,
    ItemDurationProbed(PathBuf, Option<f64>),
}

pub struct App {
    output_dir: Option<PathBuf>,
    preset: Preset,
    stage: String,
    progress: f32,
    progress_shown: f32,
    run_start: Option<Instant>,
    elapsed_final: Option<f64>,
    status: Status,
    transcript: String,
    lines: Vec<DocLine>,
    turns: Vec<backend::Turn>,
    peaks: Arc<Vec<f32>>,
    peaks_for: Option<PathBuf>,
    playhead: f32,
    timeline_duration: Option<f32>,
    player: Option<Player>,
    output_txt: Option<PathBuf>,
    summary_open: bool,
    pending_rename: bool,
    rename_open: bool,
    rename_fields: Vec<(String, String)>,
    rename_focus: usize,
    batch_runs: Vec<(String, backend::Stats)>,
    timestamps: bool,
    join_speaker: bool,
    options_open: bool,
    sms_mode: bool,
    dark_mode: bool,
    num_speakers: Option<i32>,
    confidence: bool,
    transcript_viewport: f32,
    transcript_content: f32,
    transcript_at_bottom: bool,
    follow_offset: Option<f32>,
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
    pulse: f32,
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
    batch_warning: bool,
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
            run_start: None,
            elapsed_final: None,
            status: Status::Idle,
            transcript: String::new(),
            lines: Vec::new(),
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
            peaks_for: None,
            playhead: 0.0,
            timeline_duration: None,
            player: None,
            output_txt: None,
            summary_open: false,
            pending_rename: false,
            rename_open: false,
            rename_fields: Vec::new(),
            rename_focus: 0,
            batch_runs: Vec::new(),
            timestamps: settings.timestamps.unwrap_or(true),
            join_speaker: settings.join_speaker.unwrap_or(false),
            options_open: false,
            sms_mode: settings.sms_mode.unwrap_or(false),
            dark_mode: settings.dark_mode.unwrap_or(false),
            num_speakers: match settings.num_speakers {
                None => Some(2),
                Some(0) => None,
                Some(count) => Some(count),
            },
            confidence: settings.confidence.unwrap_or(true),
            transcript_viewport: 0.0,
            transcript_content: 0.0,
            transcript_at_bottom: true,
            follow_offset: None,
            font_scale: settings.font_scale.unwrap_or(1.0),
            follow_playhead: true,
            console_open: false,
            console_lines: Vec::new(),
            toast: None,
            help_open: None,
            waveform_zoom: settings.waveform_zoom.unwrap_or(1.0),
            search_query: String::new(),
            search_index: 0,
            playback_speed: settings.playback_speed.unwrap_or(1.0),
            last_tick: None,
            pulse: 0.0,
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
            queue: Vec::new(),
            batch: false,
            current: None,
            batch_warning: false,
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
            timestamps: Some(self.timestamps),
            join_speaker: Some(self.join_speaker),
            sms_mode: Some(self.sms_mode),
            dark_mode: Some(self.dark_mode),
            num_speakers: Some(self.num_speakers.unwrap_or(0)),
            confidence: Some(self.confidence),
            font_scale: Some(self.font_scale),
            playback_speed: Some(self.playback_speed),
            waveform_zoom: Some(self.waveform_zoom),
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
        if self.dark_mode {
            iced::Theme::custom(
                "Transgender Dark".to_string(),
                iced::theme::Palette {
                    background: Color::from_rgb(0.094, 0.094, 0.145),
                    text: Color::from_rgb(0.93, 0.94, 0.98),
                    primary: Color::from_rgb(0.357, 0.808, 0.980),
                    success: Color::from_rgb(0.357, 0.808, 0.980),
                    danger: Color::from_rgb(0.961, 0.663, 0.722),
                },
            )
        } else {
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
            Message::TimestampsToggled(value) => {
                self.timestamps = value;
                self.save_settings();
                Task::none()
            }
            Message::JoinSpeakerToggled(value) => {
                self.join_speaker = value;
                self.save_settings();
                Task::none()
            }
            Message::ToggleOptions => {
                if self.queue.is_empty() {
                    return Task::none();
                }
                self.options_open = !self.options_open;
                Task::none()
            }
            Message::SmsToggled(value) => {
                self.sms_mode = value;
                self.save_settings();
                Task::none()
            }
            Message::DarkToggled(value) => {
                self.dark_mode = value;
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
            Message::ConfidenceToggled(value) => {
                self.confidence = value;
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
                // Any scroll that doesn't match the offset follow just set
                // means the user scrolled: disengage follow.
                if self.follow_playhead {
                    if let Some(expected) = self.follow_offset {
                        if (offset - expected).abs() > 6.0 {
                            self.follow_playhead = false;
                            self.follow_offset = None;
                            self.save_settings();
                        }
                    }
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
            Message::TimelineLoaded(path, result) => {
                if self.peaks_for.as_deref() == Some(path.as_path()) {
                    if let Some((duration, peaks)) = result {
                        if duration > 0.0 {
                            self.timeline_duration = Some(duration as f32);
                        }
                        if !peaks.is_empty() {
                            self.peaks = Arc::new(peaks);
                        }
                    }
                }
                Task::none()
            }
            Message::Seek(seconds) => {
                let max = self.timeline_duration.unwrap_or(0.0).max(0.0);
                self.playhead = seconds.clamp(0.0, max);
                self.last_tick = Some(Instant::now());
                if let Some(player) = &self.player {
                    player.seek(self.playhead, self.playback_speed);
                }
                Task::none()
            }
            Message::PlayerToggle => {
                let ready = self.timeline_duration.unwrap_or(0.0) > 0.0;
                if ready {
                    if self.player.is_none() {
                        if let Some(path) = self.peaks_for.clone() {
                            self.player = Player::new(&path);
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
                if let Some(line) = self.lines.get(index) {
                    if let Some(start) = line.start {
                        let max = self.timeline_duration.unwrap_or(start);
                        self.playhead = start.clamp(0.0, max);
                        self.follow_playhead = true;
                        self.follow_offset = None;
                        self.save_settings();
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
                if self.rename_open {
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
                if !self.rename_open {
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
                self.follow_playhead = !self.follow_playhead;
                self.follow_offset = None;
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
                if self.batch_warning {
                    self.batch_warning = false;
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
                }
                Task::none()
            }
            Message::ShortcutPlay => {
                if self.rename_open {
                    return Task::none();
                }
                let ready = self.timeline_duration.unwrap_or(0.0) > 0.0;
                if ready {
                    if self.player.is_none() {
                        if let Some(path) = self.peaks_for.clone() {
                            self.player = Player::new(&path);
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
                Task::none()
            }
            Message::SeekBy(delta) => {
                if self.rename_open {
                    return Task::none();
                }
                if self.timeline_duration.is_some() {
                    let max = self.timeline_duration.unwrap_or(0.0).max(0.0);
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
                    async move { backend::read_transcript(&path) },
                    Message::ImportLoaded,
                ),
                None => Task::none(),
            },
            Message::ImportLoaded(text) => {
                if let Some(text) = text {
                    self.transcript = text;
                    self.lines = parse_lines(&self.transcript);
                    self.follow_offset = None;
                    self.transcript_at_bottom = true;
                    self.sms_mode = true;
                }
                Task::none()
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
                if let Some(item) = self.queue.iter_mut().find(|item| item.path == path) {
                    item.duration = seconds;
                }
                Task::none()
            }
            Message::RemoveItem(index) => {
                if !matches!(self.status, Status::Running) && index < self.queue.len() {
                    self.queue.remove(index);
                    if let Some(current) = self.current.as_mut() {
                        if *current == index {
                            self.current = None;
                        } else if *current > index {
                            *current -= 1;
                        }
                    }
                }
                Task::none()
            }
            Message::ClearQueue => {
                if !matches!(self.status, Status::Running) {
                    self.queue.clear();
                    self.current = None;
                }
                Task::none()
            }
            Message::OpenSummary => {
                if !self.batch_runs.is_empty() {
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
            Message::CopyTranscript => {
                if self.transcript.is_empty() {
                    return Task::none();
                }
                self.toast = Some(("Copied transcript".to_string(), Instant::now()));
                iced::clipboard::write(self.transcript.clone())
            }
            Message::RevealOutput => {
                self.reveal_output();
                Task::none()
            }
            Message::SaveTranscript => {
                if let Some(path) = &self.output_txt {
                    if std::fs::write(path, &self.transcript).is_ok() {
                        self.toast = Some((format!("Saved {}", path.display()), Instant::now()));
                    }
                } else {
                    self.toast = Some(("No output file yet".to_string(), Instant::now()));
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
                    self.player = None;
                    self.peaks = Arc::new(Vec::new());
                    self.peaks_for = Some(path.clone());
                    self.playhead = 0.0;
                    self.timeline_duration = None;
                    let probe = path.clone();
                    return Task::perform(
                        async move { backend::probe_audio(&probe, TIMELINE_BUCKETS) },
                        move |result| Message::TimelineLoaded(path.clone(), result),
                    );
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
                let will_batch = self
                    .queue
                    .iter()
                    .any(|item| matches!(item.status, ItemStatus::Pending));
                if will_batch && self.queue.len() > 1 {
                    self.batch_warning = true;
                    self.toast = None;
                } else {
                    self.start();
                }
                Task::none()
            }
            Message::BatchContinue => {
                self.batch_warning = false;
                self.begin_batch();
                Task::none()
            }
            Message::BatchCancel => {
                self.batch_warning = false;
                Task::none()
            }
            Message::Cancel => {
                self.cancel();
                Task::none()
            }
            Message::Tick => {
                let mut tasks = vec![self.drain_events()];
                self.pulse = (self.pulse + 0.10) % std::f32::consts::TAU;
                // Ease the displayed bar toward the latest reported progress.
                self.progress_shown += (self.progress - self.progress_shown) * 0.25;
                self.progress_shown = self.progress_shown.clamp(0.0, 1.0);
                if let Some(player) = &self.player {
                    if player.is_playing() {
                        let now = Instant::now();
                        if let Some(last) = self.last_tick {
                            let dt = (now - last).as_secs_f32();
                            let limit = self.timeline_duration.unwrap_or(f32::MAX);
                            self.playhead =
                                (self.playhead + dt * self.playback_speed).min(limit);
                        }
                        self.last_tick = Some(now);
                    } else {
                        self.last_tick = None;
                        if player.finished() {
                            self.playhead = self.timeline_duration.unwrap_or(self.playhead);
                        }
                    }
                }
                let content = self.transcript_content;
                let viewport = self.transcript_viewport;
                if self.follow_playhead {
                    if let Some(active) = self.active_line_index() {
                        if let Some(y) = self.follow_target(active) {
                            self.follow_offset = Some(y);
                            tasks.push(scrollable::scroll_to(
                                transcript_id(),
                                scrollable::AbsoluteOffset { x: 0.0, y },
                            ));
                        }
                    }
                } else {
                    self.follow_offset = None;
                    // Keep the view pinned to the bottom as text is added, as
                    // long as the user hasn't scrolled away from the bottom.
                    if self.transcript_at_bottom && content > 0.0 && viewport > 0.0 {
                        let y = (content - viewport).max(0.0);
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
        for line in &self.lines {
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
            .output_txt
            .as_ref()
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
            if self.queue.iter().any(|item| item.path == path) {
                continue;
            }
            self.queue.push(QueueItem {
                path: path.clone(),
                status: ItemStatus::Pending,
                duration: None,
            });
            added.push(path);
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

    fn start(&mut self) {
        if self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending))
        {
            self.begin_batch();
        } else {
            self.status = Status::Error("add at least one file".to_string());
        }
    }

    /// Spawn a single pipeline process for all pending queue items, so the
    /// models load once and are reused across the whole batch.
    fn begin_batch(&mut self) {
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
        self.status = Status::Running;
        self.stage = "starting".to_string();
        self.progress = 0.0;
        self.progress_shown = 0.0;
        self.run_start = Some(Instant::now());
        self.elapsed_final = None;
        self.transcript.clear();
        self.lines.clear();
        self.follow_offset = None;
        self.transcript_at_bottom = true;
        self.turns.clear();
        self.peaks = Arc::new(Vec::new());
        self.peaks_for = None;
        self.playhead = 0.0;
        self.timeline_duration = None;
        self.player = None;
        self.output_txt = None;
        self.batch_runs.clear();
        self.rename_open = false;
        self.cancelled.store(false, Ordering::SeqCst);

        backend::spawn(
            inputs,
            self.output_dir.clone(),
            self.preset.arg(),
            self.timestamps,
            self.join_speaker,
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
        self.run_start = None;
        self.elapsed_final = None;
        self.batch = false;
        if let Some(index) = self.current.take() {
            self.queue[index].status = ItemStatus::Failed("cancelled".to_string());
        }
    }

    fn apply_rename(&mut self) {
        let mapping: Vec<(String, String)> = self
            .rename_fields
            .iter()
            .filter(|(old, new)| !new.trim().is_empty() && new.trim() != old)
            .map(|(old, new)| (old.clone(), new.trim().to_string()))
            .collect();

        for line in &mut self.lines {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == line.speaker) {
                line.raw = line.raw.replacen(&line.speaker, new, 1);
                line.speaker = new.clone();
            }
        }
        for turn in &mut self.turns {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == turn.speaker) {
                turn.speaker = new.clone();
            }
        }
        self.refresh_transcript();
        self.rename_open = false;
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
                    self.current = self.find_queue_index(&path);
                    if let Some(index) = self.current {
                        self.queue[index].status = ItemStatus::Running;
                    }
                    self.status = Status::Running;
                    self.stage = "loading".to_string();
                    self.progress = 0.0;
                    self.progress_shown = 0.0;
                    self.run_start = Some(Instant::now());
                    self.elapsed_final = None;
                    self.transcript.clear();
                    self.lines.clear();
                    self.follow_offset = None;
                    self.transcript_at_bottom = true;
                    self.turns.clear();
                    self.peaks = Arc::new(Vec::new());
                    self.peaks_for = None;
                    self.playhead = 0.0;
                    self.timeline_duration = None;
                    self.player = None;
                    self.output_txt = None;
                }
                Event::Meta { audio, duration } => {
                    let path = PathBuf::from(audio);
                    if self.peaks_for.as_deref() != Some(path.as_path()) {
                        self.peaks_for = Some(path.clone());
                        self.peaks = Arc::new(Vec::new());
                        self.playhead = 0.0;
                        self.timeline_duration = Some(duration);
                        let probe_path = path.clone();
                        let message_path = path;
                        tasks.push(Task::perform(
                            async move { backend::probe_audio(&probe_path, TIMELINE_BUCKETS) },
                            move |result| Message::TimelineLoaded(message_path.clone(), result),
                        ));
                    }
                }
                Event::Turns { turns } => {
                    self.turns = turns;
                }
                Event::Line {
                    start,
                    speaker,
                    text,
                    confidence,
                } => {
                    let raw = if self.timestamps {
                        format!("[{}] {speaker}: {text}", format_secs(start as f64))
                    } else {
                        format!("{speaker}: {text}")
                    };
                    self.lines.push(DocLine {
                        raw: raw.clone(),
                        speaker,
                        confidence,
                        start: Some(start),
                    });
                    if !self.transcript.is_empty() && !self.transcript.ends_with('\n') {
                        self.transcript.push('\n');
                    }
                    self.transcript.push_str(&raw);
                    self.transcript.push('\n');
                }
                Event::Done { txt, stats, .. } => {
                    self.stage = "done".to_string();
                    self.progress = 1.0;
                    self.progress_shown = 1.0;
                    if let Ok(contents) = std::fs::read_to_string(&txt) {
                        self.transcript = contents;
                    }
                    if self.lines.is_empty() {
                        self.lines = parse_lines(&self.transcript);
                    }
                    self.output_txt = Some(PathBuf::from(&txt));
                    let name = self
                        .current
                        .and_then(|index| self.queue.get(index))
                        .and_then(|item| item.path.file_name())
                        .and_then(|n| n.to_str())
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| {
                            PathBuf::from(&txt)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("file")
                                .to_string()
                        });
                    self.batch_runs.push((name, stats));
                    if let Some(index) = self.current.take() {
                        self.queue[index].status = ItemStatus::Done(PathBuf::from(&txt));
                    }
                }
                Event::Error { message } => {
                    self.stage = "error".to_string();
                    if let Some(index) = self.current.take() {
                        self.queue[index].status = ItemStatus::Failed(message.clone());
                    }
                    if self.batch {
                        // A process-level failure (the pipeline died without
                        // signalling completion) leaves the rest unprocessed.
                        for item in self.queue.iter_mut() {
                            if matches!(item.status, ItemStatus::Pending | ItemStatus::Running) {
                                item.status = ItemStatus::Failed(message.clone());
                            }
                        }
                        self.batch = false;
                    }
                    self.status = Status::Error(message);
                }
                Event::AllDone => {
                    self.batch = false;
                    self.current = None;
                    self.stage = "done".to_string();
                    self.progress = 1.0;
                    self.progress_shown = 1.0;
                    self.elapsed_final = self.run_start.map(|s| s.elapsed().as_secs_f64());
                    self.toast = None;
                    // Summary covers every successful run; rename only a single file.
                    self.summary_open = !self.batch_runs.is_empty();
                    self.pending_rename = self.batch_runs.len() == 1;
                    if self.batch_runs.is_empty() {
                        // Nothing succeeded, so keep any error status.
                    } else {
                        self.status = Status::Done;
                        tasks.push(scrollable::snap_to(
                            summary_id(),
                            scrollable::RelativeOffset { x: 0.0, y: 0.0 },
                        ));
                        notify("transiel", "Transcription complete");
                    }
                }
            }
        }
        Task::batch(tasks)
    }

    fn stats_lines(&self) -> Vec<String> {
        if self.batch_runs.is_empty() {
            return vec!["No completed run yet.".to_string()];
        }
        let mut lines: Vec<String> = Vec::new();
        for (name, stats) in &self.batch_runs {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(name.clone());
            lines.extend(Self::single_stats_lines(stats));
        }
        lines
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

    /// Line indices (into `self.lines`) containing the current search query.
    fn matching_lines(&self) -> Vec<usize> {
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.raw.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    /// Distinct speakers in transcript order (matches the SMS bubble ordering).
    fn speaker_order(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for line in &self.lines {
            if !line.speaker.is_empty() && !order.contains(&line.speaker) {
                order.push(line.speaker.clone());
            }
        }
        order
    }

    /// Line indices sorted by start time (untimed lines last), stable.
    fn display_order(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.lines.len()).collect();
        indices.sort_by(|a, b| {
            let ta = self.lines[*a].start.unwrap_or(f32::INFINITY);
            let tb = self.lines[*b].start.unwrap_or(f32::INFINITY);
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        });
        indices
    }

    /// Scroll the transcript so the given line (by index) is roughly centred.
    /// Pixel scroll offset that centres the given line in the transcript.
    fn follow_target(&self, line: usize) -> Option<f32> {
        let total = self.lines.len().max(1) as f32;
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

    fn scroll_to_line(&self, line: usize) -> Task<Message> {
        match self.follow_target(line) {
            Some(y) => scrollable::scroll_to(
                transcript_id(),
                scrollable::AbsoluteOffset { x: 0.0, y },
            ),
            None => Task::none(),
        }
    }

    /// Transcript view: per-line (confidence/search tinted) or SMS bubbles.
    fn transcript_element(&self) -> Element<'_, Message> {
        let scheme = Scheme::for_theme(self.dark_mode);
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
        for idx in display {
            let line = &self.lines[idx];
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
                let left = index % 2 == 0;
                let tint = if matched {
                    Some(if Some(idx) == active_line {
                        scheme.search_active
                    } else {
                        scheme.search_hl
                    })
                } else {
                    (self.confidence && line.low_confidence())
                        .then_some(if left { scheme.warn_tint } else { scheme.warn_tint_alt })
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
                        .on_press(Message::SelectLine(idx)),
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
                } else if self.confidence && line.low_confidence() {
                    Some(scheme.warn_tint)
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
                "Keep the transcript centered on the playhead",
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
        for (index, line) in self.lines.iter().enumerate() {
            if let Some(start) = line.start {
                if start <= t && best.map_or(true, |(_, best_start)| start >= best_start) {
                    best = Some((index, start));
                }
            }
        }
        best.map(|(index, _)| index)
    }

    /// The line to highlight / keep in view: the most recently added line while
    /// transcribing, otherwise the line matching the playhead.
    fn active_line_index(&self) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        if matches!(self.status, Status::Running) || self.timeline_duration.is_none() {
            self.display_order().last().copied()
        } else {
            self.line_for_playhead(self.playhead)
        }
    }

    /// Waveform/speaker timeline strip (greyed out until audio is probed).
    fn timeline_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let duration = self.timeline_duration.unwrap_or(0.0);
        let enabled = duration > 0.0 && !self.peaks.is_empty();
        let program = Timeline {
            peaks: self.peaks.clone(),
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
        self.turns
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
        let duration = self.timeline_duration.unwrap_or(0.0).max(0.0);
        let running = matches!(self.status, Status::Running);
        let ready = duration > 0.0 && self.peaks_for.is_some() && !running;
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
        let time = text(format!(
            "{} / {}",
            format_secs(self.playhead as f64),
            format_secs(duration as f64)
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
        let scheme = Scheme::for_theme(self.dark_mode);
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
        let can_options = has_queue;
        let can_add = !running;
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

        let mut options_col = column![action(
            row![
                text("Options"),
                icon(
                    if self.options_open {
                        ICON_CARET_UP
                    } else {
                        ICON_CARET_DOWN
                    },
                    12.0,
                ),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        )
            .on_press_maybe(can_options.then_some(Message::ToggleOptions))
            .style(move |theme: &iced::Theme, status| {
                if can_options {
                    pill_button_secondary(theme, status)
                } else {
                    greyed_button(theme, status)
                }
            })]
            .spacing(8);

        let mut queue_list = column![].spacing(4).width(Length::Fill);
        for (index, item) in self.queue.iter().enumerate() {
            let glyph: Element<Message> = match &item.status {
                ItemStatus::Pending => icon_colored(ICON_CIRCLE_FILL, 9.0, scheme.ink),
                ItemStatus::Running => icon_colored(
                    ICON_ARROW_RIGHT,
                    14.0,
                    Color {
                        a: 0.55 + 0.45 * (0.5 + 0.5 * self.pulse.sin()),
                        ..scheme.blue
                    },
                ),
                ItemStatus::Done(_) => {
                    icon_colored(ICON_CHECK, 14.0, Color::from_rgb(0.3, 0.72, 0.42))
                }
                ItemStatus::Failed(_) => icon_colored(ICON_X, 14.0, scheme.pink),
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
            let mut item_row = row![
                glyph,
                text(duration),
                text(format!("{name}{detail}")).width(Length::Fill),
            ]
                .spacing(8)
                .align_y(iced::Alignment::Center);

            match &item.status {
                ItemStatus::Running => {
                    item_row = item_row.push(
                        progress_bar(0.0..=1.0, self.progress)
                            .width(Length::Fixed(90.0))
                            .height(Length::Fixed(10.0))
                            .style(move |_theme| progress_bar::Style {
                                background: scheme.bar_track.into(),
                                bar: scheme.blue.into(),
                                border: iced::border::rounded(5.0),
                            }),
                    );
                    item_row = item_row.push(
                        text(format!("{:.0}%", self.progress * 100.0)).size(12),
                    );
                }
                ItemStatus::Done(_) => {
                    item_row = item_row.push(text("100%").size(12));
                }
                _ => {}
            }

            item_row = item_row.push(self.tip(
                action(icon(ICON_X, 13.0))
                    .on_press_maybe(can_add.then_some(Message::RemoveItem(index)))
                    .style(pill_button_secondary),
                "Remove from queue",
                &scheme,
            ));
            queue_list = queue_list.push(item_row);
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
            let line_height = 30.0;
            let height = self.queue.len() as f32 * line_height * 1.2;
            queue_col = queue_col.push(
                container(queue_list)
                    .width(Length::Fill)
                    .height(Length::Fixed(height))
                    .align_y(iced::Alignment::Start),
            );
        }

        if self.options_open && can_options {
            let speakers_control: Element<Message> = if running {
                text(SpeakerOpt::from_value(self.num_speakers).to_string()).into()
            } else {
                pick_list(
                    SpeakerOpt::ALL,
                    Some(SpeakerOpt::from_value(self.num_speakers)),
                    Message::SpeakerCountChanged,
                )
                    .style(pill_pick)
                    .into()
            };
            options_col = options_col.push(
                column![
                    self.tip(
                        row![
                            text("Speakers"),
                            horizontal_space(),
                            speakers_control,
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Expected number of speakers",
                        &scheme,
                    ),
                    self.tip(
                        row![
                            text("Timestamps"),
                            horizontal_space(),
                            toggler(self.timestamps)
                            .on_toggle_maybe((!running).then_some(Message::TimestampsToggled)),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Prefix each line with its start time",
                        &scheme,
                    ),
                    self.tip(
                        row![
                            text("Join Speakers"),
                            horizontal_space(),
                            toggler(self.join_speaker)
                            .on_toggle_maybe((!running).then_some(Message::JoinSpeakerToggled)),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Merge consecutive lines from the same speaker",
                        &scheme,
                    ),
                    self.tip(
                        row![
                            text("Confidence"),
                            horizontal_space(),
                            toggler(self.confidence)
                            .on_toggle_maybe((!running).then_some(Message::ConfidenceToggled)),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Tint lines the model transcribed with low confidence",
                        &scheme,
                    ),
                    self.tip(
                        row![
                            text("Dark theme"),
                            horizontal_space(),
                            toggler(self.dark_mode)
                            .on_toggle_maybe((!running).then_some(Message::DarkToggled)),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Use the dark theme",
                        &scheme,
                    ),
                ]
                    .spacing(8),
            );
        }

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
                options_col,
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

        let total_items = self.queue.len().max(1) as f32;
        let done_items = self
            .queue
            .iter()
            .filter(|item| matches!(item.status, ItemStatus::Done(_) | ItemStatus::Failed(_)))
            .count() as f32;
        let current_fraction = if running { self.progress_shown } else { 0.0 };
        let global_progress =
            ((done_items + current_fraction) / total_items).clamp(0.0, 1.0);

        let clock = row![
            icon_colored(ICON_CLOCK, 14.0, scheme.text),
            text(self.elapsed().unwrap_or_default()).width(Length::Fixed(60.0)),
        ]
            .spacing(4)
            .align_y(iced::Alignment::Center);

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
                container(text(capitalize_first(&match &self.status {
                    Status::Idle => "Ready".to_string(),
                    Status::Error(message) => format!("error: {message}"),
                    _ => self.stage.clone(),
                })))
                .center_x(Length::Fill)
                .center_y(Length::Fill),
            ]
            .width(Length::Fill),
        ]
            .spacing(2)
            .align_y(iced::Alignment::Center);

        let mut middle = column![status_row].spacing(8).width(Length::Fill);
        middle = middle.push(self.timeline_element(&scheme));
        let import: Element<Message> = if running {
            stack![
                pick_list(ImportKind::ALL, None::<ImportKind>, Message::ImportChoice)
                .placeholder("Import\u{2026}")
                .style(greyed_pick)
                .handle(caret_handle(false)),
                mouse_area(
                    container(horizontal_space())
                    .width(Length::Fill)
                    .height(Length::Fill),
                )
                .interaction(mouse::Interaction::Idle)
                .on_press(Message::Noop),
            ]
                .into()
        } else {
            pick_list(ImportKind::ALL, None::<ImportKind>, Message::ImportChoice)
                .placeholder("Import\u{2026}")
                .style(ink_pick(scheme.ink))
                .handle(caret_handle(false))
                .into()
        };
        middle = middle.push(
            row![
                self.player_element(&scheme),
                action("Summary")
                .on_press_maybe(
                    (!running && !self.batch_runs.is_empty()).then_some(Message::OpenSummary)
                )
                .style(pill_button_secondary),
                self.tip(
                    action("Rename Speakers")
                    .on_press_maybe(
                        (!running && !self.lines.is_empty()).then_some(Message::OpenRename)
                    )
                    .style(pill_button_secondary),
                    "Rename speakers across the transcript",
                    &scheme,
                ),
                horizontal_space(),
                import,
            ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .width(Length::Fill),
        );

        let mut base = column![
            text("Transiel").size(32),
            queue_col,
            controls,
            middle,
            container(self.transcript_element())
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
                        (!self.transcript.is_empty()).then_some(Message::CopyTranscript)
                    )
                    .style(pill_button_secondary),
                    action("Save")
                    .on_press_maybe(
                        (self.output_txt.is_some() && !self.transcript.is_empty())
                        .then_some(Message::SaveTranscript)
                    )
                    .style(pill_button_secondary),
                    action("Reveal")
                    .on_press_maybe(
                        (self.output_txt.is_some() || self.output_dir.is_some())
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

        if self.batch_warning {
            let dialog = container(
                column![
                    text("Warning").size(20),
                    text(
                        "Speakers can not be renamed in batch mode unless the \
                         transcription files are reimported.",
                    ),
                    container(
                        row![
                            action("Cancel")
                            .on_press(Message::BatchCancel)
                            .style(color_button(scheme.pink, scheme.ink)),
                            action("Continue")
                            .on_press(Message::BatchContinue)
                            .style(color_button(scheme.blue, scheme.ink)),
                        ]
                        .spacing(8),
                    )
                    .width(Length::Fill)
                    .align_x(iced::Alignment::End),
                ]
                    .spacing(12)
                    .width(Length::Fixed(380.0)),
            )
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
            for line in &lines {
                body = body.push(text(line.clone()));
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
                        "Diarization: DiariZen / pyannote.  Transcription: faster-whisper."
                            .to_string(),
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
    const EXTRA: [(f32, f32, f32); 4] = [
        (0.66, 0.62, 0.98),
        (0.45, 0.82, 0.62),
        (0.98, 0.78, 0.42),
        (0.55, 0.85, 0.88),
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
const ICON_ARROW_RIGHT: char = '\u{f138}';
const ICON_ARROW_UP: char = '\u{f148}';
const ICON_ARROW_DOWN: char = '\u{f128}';
const ICON_CIRCLE_FILL: char = '\u{f287}';
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
    let centered = container(dialog)
        .center(Length::Fill)
        .width(Length::Fill)
        .height(Length::Fill);
    stack![base, backdrop, centered].into()
}

/// Parse "[time] Speaker: text" transcript lines into the render model.
fn parse_lines(text: &str) -> Vec<DocLine> {
    text.lines()
        .map(|line| DocLine {
            raw: line.to_string(),
            speaker: speaker_of(line),
            confidence: 1.0,
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
        .on_press(Message::SelectLine(idx)),
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
    style.background = Color::from_rgba(0.6, 0.6, 0.64, 0.12).into();
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
