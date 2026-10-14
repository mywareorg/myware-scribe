use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, Modifiers};
use serde::{Deserialize, Serialize};
use iced::widget::{
    button, canvas, column, container, horizontal_space, mouse_area, pick_list, progress_bar,
    rich_text, row, scrollable, slider, stack, text, text_input, toggler, tooltip,
};
use iced::{
    font, mouse, Color, Element, Font, Length, Point, Rectangle, Renderer, Size, Subscription,
    Task, Theme,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    #[default]
    Speed,
    Balanced,
    Accuracy,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::Speed, Preset::Balanced, Preset::Accuracy];

    fn label(self) -> &'static str {
        match self {
            Preset::Speed => "Speed",
            Preset::Balanced => "Balanced",
            Preset::Accuracy => "Accuracy",
        }
    }

    fn arg(self) -> &'static str {
        match self {
            Preset::Speed => "speed",
            Preset::Balanced => "balanced",
            Preset::Accuracy => "accuracy",
        }
    }

    fn from_arg(value: &str) -> Option<Preset> {
        match value {
            "speed" => Some(Preset::Speed),
            "balanced" => Some(Preset::Balanced),
            "accuracy" => Some(Preset::Accuracy),
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
    cache: Option<bool>,
    confidence: Option<bool>,
    window_width: Option<f32>,
    window_height: Option<f32>,
    window_x: Option<f32>,
    window_y: Option<f32>,
}

struct Scheme {
    panel_bg: Color,
    dialog_bg: Color,
    track: Color,
    blue: Color,
    pink: Color,
    ink: Color,
    sms0: Color,
    sms1: Color,
    warn_tint: Color,
    warn_tint_alt: Color,
    search_hl: Color,
    search_active: Color,
}

impl Scheme {
    fn for_theme(dark: bool) -> Scheme {
        if dark {
            Scheme {
                panel_bg: Color::from_rgb(0.094, 0.094, 0.145),
                dialog_bg: Color::from_rgb(0.118, 0.118, 0.180),
                track: Color::from_rgb(0.067, 0.067, 0.106),
                blue: Color::from_rgb(0.537, 0.706, 0.980),
                pink: Color::from_rgb(0.953, 0.545, 0.659),
                ink: Color::from_rgb(0.067, 0.067, 0.106),
                sms0: Color::from_rgb(0.537, 0.706, 0.980),
                sms1: Color::from_rgb(0.796, 0.651, 0.969),
                warn_tint: Color::from_rgba(0.537, 0.749, 1.0, 0.30),
                warn_tint_alt: Color::from_rgba(0.953, 0.545, 0.659, 0.16),
                search_hl: Color::from_rgba(1.0, 0.85, 0.35, 0.35),
                search_active: Color::from_rgba(1.0, 0.72, 0.25, 0.65),
            }
        } else {
            Scheme {
                panel_bg: Color::from_rgb(0.96, 0.97, 0.995),
                dialog_bg: Color::from_rgb(0.99, 0.99, 1.0),
                track: Color::from_rgb(0.88, 0.88, 0.93),
                blue: Color::from_rgb(0.357, 0.808, 0.980),
                pink: Color::from_rgb(0.961, 0.663, 0.722),
                ink: Color::from_rgb(0.13, 0.13, 0.17),
                sms0: Color::from_rgb(0.357, 0.808, 0.980),
                sms1: Color::from_rgb(0.961, 0.663, 0.722),
                warn_tint: Color::from_rgba(0.529, 0.867, 1.0, 0.35),
                warn_tint_alt: Color::from_rgba(0.961, 0.663, 0.722, 0.22),
                search_hl: Color::from_rgba(1.0, 0.85, 0.3, 0.5),
                search_active: Color::from_rgba(1.0, 0.72, 0.2, 0.8),
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
    Close,
    RenameName1(String),
    RenameName2(String),
    RenameSave,
    RenameCancel,
    RenameMove(bool),
    RenameActivate,
    OpenSummary,
    TimestampsToggled(bool),
    JoinSpeakerToggled(bool),
    ToggleAdvanced,
    SmsToggled(bool),
    DarkToggled(bool),
    SpeakerCountChanged(SpeakerOpt),
    CacheToggled(bool),
    ConfidenceToggled(bool),
    WindowEvent(iced::window::Event),
    TimelineLoaded(PathBuf, Option<(f64, Vec<f32>)>),
    Seek(f32),
    PlayerToggle,
    ToggleSearch,
    SearchChanged(String),
    ReplaceChanged(String),
    FindPrev,
    FindNext,
    ReplaceAll,
    EditLine(usize),
    EditText(String),
    EditSave,
    EditCancel,
    Escape,
    ShortcutPlay,
    SeekBy(f32),
    PickImport,
    ImportPicked(Option<PathBuf>),
    ImportLoaded(Option<String>),
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
    speakers: HashMap<String, String>,
    turns: Vec<backend::Turn>,
    meta_duration: Option<f32>,
    peaks: Arc<Vec<f32>>,
    peaks_for: Option<PathBuf>,
    playhead: f32,
    timeline_duration: Option<f32>,
    player: Option<Player>,
    output_txt: Option<PathBuf>,
    rename_open: bool,
    rename_focus: usize,
    focus_pending: bool,
    name1: String,
    name2: String,
    stats: Option<backend::Stats>,
    timestamps: bool,
    join_speaker: bool,
    advanced_open: bool,
    sms_mode: bool,
    dark_mode: bool,
    num_speakers: Option<i32>,
    cache: bool,
    confidence: bool,
    search_open: bool,
    search_query: String,
    replace_query: String,
    search_index: usize,
    edit_open: bool,
    edit_index: Option<usize>,
    edit_text: String,
    window_size: Size,
    window_position: Option<Point>,
    window_dirty: bool,
    last_window_change: Option<Instant>,
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
            output_dir: None,
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
            speakers: HashMap::new(),
            turns: Vec::new(),
            meta_duration: None,
            peaks: Arc::new(Vec::new()),
            peaks_for: None,
            playhead: 0.0,
            timeline_duration: None,
            player: None,
            output_txt: None,
            rename_open: false,
            rename_focus: 0,
            focus_pending: false,
            name1: String::new(),
            name2: String::new(),
            stats: None,
            timestamps: settings.timestamps.unwrap_or(true),
            join_speaker: settings.join_speaker.unwrap_or(false),
            advanced_open: false,
            sms_mode: settings.sms_mode.unwrap_or(false),
            dark_mode: settings.dark_mode.unwrap_or(false),
            num_speakers: match settings.num_speakers {
                None => Some(2),
                Some(0) => None,
                Some(count) => Some(count),
            },
            cache: settings.cache.unwrap_or(true),
            confidence: settings.confidence.unwrap_or(true),
            search_open: false,
            search_query: String::new(),
            replace_query: String::new(),
            search_index: 0,
            edit_open: false,
            edit_index: None,
            edit_text: String::new(),
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
            cache: Some(self.cache),
            confidence: Some(self.confidence),
            window_width: Some(self.window_size.width),
            window_height: Some(self.window_size.height),
            window_x: self.window_position.map(|point| point.x),
            window_y: self.window_position.map(|point| point.y),
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
            ..iced::window::Settings::default()
        }
    }

    fn window_size_from(settings: &Settings) -> Size {
        match (settings.window_width, settings.window_height) {
            (Some(width), Some(height)) if width >= 100.0 && height >= 100.0 => {
                Size::new(width, height)
            }
            _ => Size::new(1000.0, 700.0),
        }
    }

    pub fn theme(&self) -> iced::Theme {
        if self.dark_mode {
            iced::Theme::CatppuccinMocha
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
            Key::Named(Named::Tab) => Some(Message::RenameMove(modifiers.shift())),
            Key::Named(Named::Enter) if modifiers.command() => Some(Message::Start),
            Key::Named(Named::Enter) => Some(Message::RenameActivate),
            Key::Named(Named::Escape) => Some(Message::Escape),
            Key::Named(Named::Space) => Some(Message::ShortcutPlay),
            Key::Named(Named::ArrowRight) => Some(Message::SeekBy(5.0)),
            Key::Named(Named::ArrowLeft) => Some(Message::SeekBy(-5.0)),
            Key::Character(character) if modifiers.command() => match character.as_str() {
                "o" => Some(Message::AddFiles),
                "f" => Some(Message::ToggleSearch),
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
            Message::ToggleAdvanced => {
                self.advanced_open = !self.advanced_open;
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
            Message::CacheToggled(value) => {
                self.cache = value;
                self.save_settings();
                Task::none()
            }
            Message::ConfidenceToggled(value) => {
                self.confidence = value;
                self.save_settings();
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
                        self.timeline_duration = Some(duration as f32);
                        self.peaks = Arc::new(peaks);
                    }
                }
                Task::none()
            }
            Message::Seek(seconds) => {
                let max = self.timeline_duration.unwrap_or(0.0).max(0.0);
                self.playhead = seconds.clamp(0.0, max);
                if let Some(player) = &self.player {
                    player.seek(self.playhead);
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
                    }
                    if let Some(player) = &self.player {
                        player.toggle();
                    }
                }
                Task::none()
            }
            Message::ToggleSearch => {
                self.search_open = !self.search_open;
                if self.search_open {
                    text_input::focus(find_id())
                } else {
                    Task::none()
                }
            }
            Message::SearchChanged(query) => {
                self.search_query = query;
                self.search_index = 0;
                Task::none()
            }
            Message::ReplaceChanged(value) => {
                self.replace_query = value;
                Task::none()
            }
            Message::FindNext => {
                let count = self.matching_lines().len();
                if count > 0 {
                    self.search_index = (self.search_index + 1) % count;
                }
                Task::none()
            }
            Message::FindPrev => {
                let count = self.matching_lines().len();
                if count > 0 {
                    self.search_index = (self.search_index + count - 1) % count;
                }
                Task::none()
            }
            Message::ReplaceAll => {
                if !self.search_query.is_empty() {
                    for line in &mut self.lines {
                        line.raw = line.raw.replace(&self.search_query, &self.replace_query);
                    }
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
                Task::none()
            }
            Message::EditLine(index) => {
                if let Some(line) = self.lines.get(index) {
                    self.edit_text = line.raw.clone();
                    self.edit_index = Some(index);
                    self.edit_open = true;
                    return text_input::focus(edit_id());
                }
                Task::none()
            }
            Message::EditText(value) => {
                self.edit_text = value;
                Task::none()
            }
            Message::EditCancel => {
                self.edit_open = false;
                self.edit_index = None;
                Task::none()
            }
            Message::EditSave => {
                if let Some(index) = self.edit_index.take() {
                    if let Some(line) = self.lines.get_mut(index) {
                        line.speaker = speaker_of(&self.edit_text);
                        line.raw = self.edit_text.clone();
                    }
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
                self.edit_open = false;
                Task::none()
            }
            Message::Escape => {
                if self.batch_warning {
                    self.batch_warning = false;
                } else if self.edit_open {
                    self.edit_open = false;
                    self.edit_index = None;
                } else if self.rename_open {
                    self.rename_open = false;
                } else if self.search_open {
                    self.search_open = false;
                } else if matches!(self.status, Status::Running) {
                    self.cancel();
                }
                Task::none()
            }
            Message::ShortcutPlay => {
                if self.search_open || self.edit_open || self.rename_open {
                    return Task::none();
                }
                let ready = self.timeline_duration.unwrap_or(0.0) > 0.0;
                if ready {
                    if self.player.is_none() {
                        if let Some(path) = self.peaks_for.clone() {
                            self.player = Player::new(&path);
                        }
                    }
                    if let Some(player) = &self.player {
                        player.toggle();
                    }
                }
                Task::none()
            }
            Message::SeekBy(delta) => {
                if self.search_open || self.edit_open || self.rename_open {
                    return Task::none();
                }
                if self.timeline_duration.is_some() {
                    let max = self.timeline_duration.unwrap_or(0.0).max(0.0);
                    self.playhead = (self.playhead + delta).clamp(0.0, max);
                    let playhead = self.playhead;
                    if let Some(player) = &self.player {
                        player.seek(playhead);
                    }
                }
                Task::none()
            }
            Message::PickImport => Task::perform(
                async {
                    rfd::FileDialog::new()
                        .add_filter("Transcript", &["txt", "json"])
                        .pick_file()
                },
                Message::ImportPicked,
            ),
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
                if self.stats.is_some() {
                    self.rename_open = true;
                    self.rename_focus = 0;
                    self.focus_pending = false;
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
                // Ease the displayed bar toward the latest reported progress.
                self.progress_shown += (self.progress - self.progress_shown) * 0.25;
                self.progress_shown = self.progress_shown.clamp(0.0, 1.0);
                if let Some(player) = &self.player {
                    if player.is_playing() {
                        self.playhead = player.position();
                    } else if player.finished() {
                        self.playhead = self.timeline_duration.unwrap_or(self.playhead);
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
                if self.focus_pending && self.rename_open {
                    self.focus_pending = false;
                    tasks.push(self.focus_task());
                }
                Task::batch(tasks)
            }
            Message::Close => iced::exit(),
            Message::RenameName1(value) => {
                self.name1 = value;
                Task::none()
            }
            Message::RenameName2(value) => {
                self.name2 = value;
                Task::none()
            }
            Message::RenameSave => {
                self.apply_rename();
                Task::none()
            }
            Message::RenameCancel => {
                self.rename_open = false;
                Task::none()
            }
            Message::RenameMove(back) => {
                if !self.rename_open {
                    return Task::none();
                }
                const COUNT: usize = 4;
                self.rename_focus = if back {
                    (self.rename_focus + COUNT - 1) % COUNT
                } else {
                    (self.rename_focus + 1) % COUNT
                };
                self.focus_task()
            }
            Message::RenameActivate => {
                if !self.rename_open {
                    return Task::none();
                }
                match self.rename_focus {
                    0 | 1 => {
                        self.rename_focus = (self.rename_focus + 1) % 4;
                        self.focus_task()
                    }
                    2 => {
                        self.apply_rename();
                        Task::none()
                    }
                    _ => {
                        self.rename_open = false;
                        Task::none()
                    }
                }
            }
        }
    }

    fn focus_task(&self) -> Task<Message> {
        match self.rename_focus {
            0 => text_input::focus(name1_id()),
            1 => text_input::focus(name2_id()),
            _ => Task::none(),
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

    fn begin_batch(&mut self) {
        self.batch = true;
        self.transcript.clear();
        self.output_txt = None;
        self.stats = None;
        self.rename_open = false;
        self.name1.clear();
        self.name2.clear();
        self.cancelled.store(false, Ordering::SeqCst);
        self.run_next();
    }

    /// Run the next pending queue item, or finish the batch when none remain.
    fn run_next(&mut self) {
        let Some(index) = self
            .queue
            .iter()
            .position(|item| matches!(item.status, ItemStatus::Pending))
        else {
            self.batch = false;
            self.current = None;
            self.status = Status::Done;
            self.stage = "done".to_string();
            self.progress = 1.0;
            self.progress_shown = 1.0;
            return;
        };

        let input = self.queue[index].path.clone();

        let Some(root) = backend::repo_root() else {
            self.queue[index].status =
                ItemStatus::Failed("cannot locate repo root".to_string());
            return self.run_next();
        };
        let python = backend::venv_python(&root);
        if !python.is_file() {
            self.queue[index].status =
                ItemStatus::Failed("python not found; run scripts/bootstrap.py".to_string());
            return self.run_next();
        }

        let base = backend::output_base(self.output_dir.as_deref(), &input);

        self.queue[index].status = ItemStatus::Running;
        self.current = Some(index);
        self.status = Status::Running;
        self.stage = "starting".to_string();
        self.progress = 0.0;
        self.progress_shown = 0.0;
        self.run_start = Some(Instant::now());
        self.elapsed_final = None;
        self.cancelled.store(false, Ordering::SeqCst);

        self.transcript.clear();
        self.lines.clear();
        self.speakers.clear();
        self.turns.clear();
        self.meta_duration = None;
        self.peaks = Arc::new(Vec::new());
        self.peaks_for = None;
        self.playhead = 0.0;
        self.timeline_duration = None;
        self.player = None;
        self.output_txt = None;
        self.stats = None;

        backend::spawn(
            python,
            root,
            input,
            base,
            self.preset.arg(),
            self.timestamps,
            self.join_speaker,
            self.num_speakers,
            self.cache,
            self.events.clone(),
            self.pid.clone(),
            self.cancelled.clone(),
        );
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
        let name1 = if self.name1.trim().is_empty() {
            "SPEAKER 1".to_string()
        } else {
            self.name1.trim().to_string()
        };
        let name2 = if self.name2.trim().is_empty() {
            "SPEAKER 2".to_string()
        } else {
            self.name2.trim().to_string()
        };

        self.transcript = self
            .transcript
            .replace("SPEAKER 1", &name1)
            .replace("SPEAKER 2", &name2);
        self.lines = parse_lines(&self.transcript);

        if let Some(path) = &self.output_txt {
            let _ = std::fs::write(path, &self.transcript);
        }
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
                Event::Log { .. } => {}
                Event::Meta { audio, duration } => {
                    self.meta_duration = Some(duration);
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
                Event::Turns { speakers, turns } => {
                    self.speakers = speakers;
                    self.turns = turns;
                }
                Event::Line {
                    start,
                    speaker,
                    text,
                    confidence,
                    ..
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
                    self.elapsed_final = self.run_start.map(|s| s.elapsed().as_secs_f64());
                    if let Ok(contents) = std::fs::read_to_string(&txt) {
                        self.transcript = contents;
                    }
                    if self.lines.is_empty() {
                        self.lines = parse_lines(&self.transcript);
                    }
                    self.output_txt = Some(PathBuf::from(&txt));
                    self.stats = Some(stats);
                    if self.batch {
                        if let Some(index) = self.current.take() {
                            self.queue[index].status = ItemStatus::Done(PathBuf::from(&txt));
                        }
                        if self.queue.len() == 1 {
                            self.batch = false;
                            self.status = Status::Done;
                            self.name1.clear();
                            self.name2.clear();
                            self.rename_open = true;
                            self.rename_focus = 0;
                            self.focus_pending = true;
                        } else {
                            self.run_next();
                        }
                    } else {
                        self.status = Status::Done;
                        self.name1.clear();
                        self.name2.clear();
                        self.rename_open = true;
                        self.rename_focus = 0;
                        self.focus_pending = true;
                    }
                }
                Event::Error { message } => {
                    self.stage = "error".to_string();
                    if self.batch {
                        if let Some(index) = self.current.take() {
                            self.queue[index].status = ItemStatus::Failed(message);
                        }
                        self.run_next();
                    } else {
                        self.status = Status::Error(message);
                    }
                }
            }
        }
        Task::batch(tasks)
    }

    fn stats_lines(&self) -> Vec<String> {
        match &self.stats {
            Some(s) => {
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
                    format!("Segments: {}", s.lines),
                    format!("Speakers: {speakers}"),
                    format!("Preset: {}", s.preset),
                    format!("Diarizer: {}", s.diarizer),
                    format!("Transcriber: {}", s.transcriber),
                ];
                if s.cache_hit {
                    lines.push("Diarization cache: hit".to_string());
                }
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
            None => vec!["No completed run yet.".to_string()],
        }
    }

    fn status_text(&self) -> String {
        match &self.status {
            Status::Idle => "idle".to_string(),
            Status::Running => "running".to_string(),
            Status::Done => "done".to_string(),
            Status::Cancelled => "cancelled".to_string(),
            Status::Error(message) => format!("error: {message}"),
        }
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

    /// Transcript view: per-line (confidence/search tinted) or SMS bubbles.
    fn transcript_element(&self) -> Element<'_, Message> {
        let scheme = Scheme::for_theme(self.dark_mode);
        let query = if self.search_open {
            self.search_query.trim().to_string()
        } else {
            String::new()
        };
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            self.matching_lines()
        };
        let active_line = matches.get(self.search_index).copied();
        let mut col = column![].spacing(2).padding(8).width(Length::Fill);
        let mut sides: Vec<String> = Vec::new();
        for (idx, line) in self.lines.iter().enumerate() {
            if line.raw.trim().is_empty() {
                continue;
            }
            let matched = !query.is_empty() && matches.contains(&idx);
            if self.sms_mode {
                let label = line.speaker.clone();
                let index = match sides.iter().position(|s| s == &label) {
                    Some(i) => i,
                    None => {
                        sides.push(label.clone());
                        sides.len() - 1
                    }
                };
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
                col = col.push(mouse_area(sms_row(
                    line.raw.trim(),
                    left,
                    sms_color(index, &scheme),
                    scheme.ink,
                    tint,
                    &query,
                    scheme.search_hl,
                ))
                .on_press(Message::EditLine(idx)));
            } else {
                let tint = if matched {
                    Some(if Some(idx) == active_line {
                        scheme.search_active
                    } else {
                        scheme.search_hl
                    })
                } else {
                    (self.confidence && line.low_confidence()).then_some(scheme.warn_tint)
                };
                col = col.push(
                    mouse_area(line_row(&line.raw, tint, &query, scheme.search_hl))
                        .on_press(Message::EditLine(idx)),
                );
            }
        }
        scrollable(col)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(scrollbar_style)
            .into()
    }

    /// Waveform/speaker timeline strip, or `None` before audio is probed.
    fn timeline_element(&self, scheme: &Scheme) -> Option<Element<'_, Message>> {
        let duration = self.timeline_duration?;
        if self.peaks.is_empty() || duration <= 0.0 {
            return None;
        }
        let program = Timeline {
            peaks: self.peaks.clone(),
            turns: Arc::new(self.speaker_turns(scheme)),
            duration,
            playhead: self.playhead,
            track: scheme.track,
            wave: scheme.blue,
            playhead_color: scheme.ink,
        };
        Some(with_tip(
            canvas(program)
                .width(Length::Fill)
                .height(Length::Fixed(TIMELINE_HEIGHT)),
            "Click or drag to seek",
            scheme,
        ))
    }

    fn speaker_turns(&self, scheme: &Scheme) -> Vec<TimelineTurn> {
        let mut order: Vec<&str> = Vec::new();
        for turn in &self.turns {
            if !order.iter().any(|name| *name == turn.speaker.as_str()) {
                order.push(turn.speaker.as_str());
            }
        }
        self.turns
            .iter()
            .map(|turn| {
                let index = order
                    .iter()
                    .position(|name| *name == turn.speaker.as_str())
                    .unwrap_or(0);
                TimelineTurn {
                    start: turn.start,
                    end: turn.end,
                    color: speaker_palette(index, scheme),
                }
            })
            .collect()
    }

    /// Bottom-left playback controls: play/pause, time, scrubber.
    fn player_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let duration = self.timeline_duration.unwrap_or(0.0).max(0.0);
        let ready = duration > 0.0 && self.peaks_for.is_some();
        let playing = self.player.as_ref().is_some_and(|player| player.is_playing());
        let play_button = button(
            canvas(PlayPause {
                playing,
                color: scheme.ink,
            })
            .width(Length::Fixed(12.0))
            .height(Length::Fixed(12.0)),
        )
        .on_press_maybe(ready.then_some(Message::PlayerToggle))
        .style(pill_button_secondary);
        let time = text(format!(
            "{} / {}",
            format_secs(self.playhead as f64),
            format_secs(duration as f64)
        ))
        .size(12);
        let scrubber = slider(0.0..=duration.max(0.001), self.playhead, Message::Seek)
            .width(Length::Fixed(160.0));
        with_tip(
            row![play_button, time, scrubber]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            "Play or pause the current audio",
            scheme,
        )
    }

    /// Find / replace bar shown above the transcript when search is open.
    fn search_element(&self, scheme: &Scheme) -> Element<'_, Message> {
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
            .width(Length::Fill)
            .style(pill_input);
        let replace_input = text_input("Replace with", &self.replace_query)
            .on_input(Message::ReplaceChanged)
            .width(Length::Fill)
            .style(pill_input);
        let prev = button("Prev")
            .on_press(Message::FindPrev)
            .style(pill_button_secondary);
        let next = button("Next")
            .on_press(Message::FindNext)
            .style(pill_button_secondary);
        let replace_all = button("Replace All")
            .on_press(Message::ReplaceAll)
            .style(color_button(scheme.blue, scheme.ink));
        let close = button("\u{00d7}")
            .on_press(Message::ToggleSearch)
            .style(pill_button_secondary);
        with_tip(
            row![
                query_input,
                text(status).size(12),
                prev,
                next,
                replace_input,
                replace_all,
                close,
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            "Find and replace text in the transcript",
            scheme,
        )
    }

    pub fn view(&self) -> Element<'_, Message> {
        let scheme = Scheme::for_theme(self.dark_mode);
        let output = self
            .output_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(default: alongside each audio file)".to_string());

        let running = matches!(self.status, Status::Running);
        let pending = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending));
        let has_queue = !self.queue.is_empty();
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
            let greyed = |theme: &iced::Theme, status: pick_list::Status| {
                let mut style = pick_list::default(theme, status);
                let grey = Color::from_rgb(0.55, 0.55, 0.55);
                style.text_color = grey;
                style.handle_color = grey;
                style.background = Color::from_rgb(0.9, 0.9, 0.9).into();
                style.border.radius = PILL.into();
                style
            };
            stack![
                pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged).style(greyed),
                mouse_area(
                    container(horizontal_space())
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .on_press(Message::Tick),
            ]
            .into()
        };

        let mut controls = column![
            row![
                with_tip(
                    button("Save Location")
                        .on_press_maybe(can_output.then_some(Message::PickOutput))
                        .style(color_button(scheme.blue, scheme.ink)),
                    "Choose where transcripts are written (default: alongside each file)",
                    &scheme,
                ),
                text(output.clone()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            row![
                text("Preset:"),
                preset,
                with_tip(
                    button(
                        row![
                            text("Advanced"),
                            text('\u{e800}').font(Font::with_name("Iced-Icons")).size(12),
                        ]
                        .spacing(4)
                        .align_y(iced::Alignment::Center),
                    )
                    .on_press(Message::ToggleAdvanced)
                    .style(pill_button_secondary),
                    "Show extra options",
                    &scheme,
                ),
                with_tip(
                    button("Find")
                        .on_press(Message::ToggleSearch)
                        .style(pill_button_secondary),
                    "Find and replace text in the transcript",
                    &scheme,
                ),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        ]
        .spacing(16);

        let mut queue_list = column![].spacing(4).width(Length::Fill);
        for (index, item) in self.queue.iter().enumerate() {
            let glyph: Element<Message> = match &item.status {
                ItemStatus::Pending => {
                    let color = scheme.ink;
                    text("\u{2022}").style(move |_theme| text::Style { color: Some(color) }).into()
                }
                ItemStatus::Running => arrow_glyph(scheme.blue),
                ItemStatus::Done(_) => {
                    let color = Color::from_rgb(0.3, 0.72, 0.42);
                    text('\u{f00c}')
                        .font(Font::with_name("Iced-Icons"))
                        .style(move |_theme| text::Style { color: Some(color) })
                        .into()
                }
                ItemStatus::Failed(_) => {
                    let color = scheme.pink;
                    text("\u{00d7}").style(move |_theme| text::Style { color: Some(color) }).into()
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
            queue_list = queue_list.push(
                row![
                    glyph,
                    text(duration),
                    text(format!("{name}{detail}")).width(Length::Fill),
                    with_tip(
                        button("\u{00d7}")
                            .on_press_maybe(can_add.then_some(Message::RemoveItem(index)))
                            .style(pill_button_secondary),
                        "Remove from queue",
                        &scheme,
                    ),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }

        let mut queue_col = column![
            row![
                with_tip(
                    button("Add File(s)")
                        .on_press_maybe(can_add.then_some(Message::AddFiles))
                        .style(color_button(scheme.blue, scheme.ink)),
                    "Add one or more audio or video files to the queue",
                    &scheme,
                ),
                with_tip(
                    button("Add Folder")
                        .on_press_maybe(can_add.then_some(Message::AddFolder))
                        .style(color_button(scheme.blue, scheme.ink)),
                    "Queue every audio or video file in a folder",
                    &scheme,
                ),
                with_tip(
                    button("Clear")
                        .on_press_maybe((!running && has_queue).then_some(Message::ClearQueue))
                        .style(color_button(scheme.pink, scheme.ink)),
                    "Remove all queued files",
                    &scheme,
                ),
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

        if self.advanced_open {
            controls = controls.push(
                column![
                    with_tip(
                        row![
                            text("Confidence"),
                            horizontal_space(),
                            toggler(self.confidence).on_toggle(Message::ConfidenceToggled),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Tint lines the model transcribed with low confidence",
                        &scheme,
                    ),
                    with_tip(
                        row![
                            text("Dark theme"),
                            horizontal_space(),
                            toggler(self.dark_mode).on_toggle(Message::DarkToggled),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Use the dark (Catppuccin Mocha) theme",
                        &scheme,
                    ),
                    with_tip(
                        row![
                            text("Diarization cache"),
                            horizontal_space(),
                            toggler(self.cache).on_toggle(Message::CacheToggled),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Reuse diarization results for unchanged audio",
                        &scheme,
                    ),
                    with_tip(
                        row![
                            text("Join Speakers"),
                            horizontal_space(),
                            toggler(self.join_speaker).on_toggle(Message::JoinSpeakerToggled),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Merge consecutive lines from the same speaker",
                        &scheme,
                    ),
                    with_tip(
                        row![
                            text("SMS Mode"),
                            horizontal_space(),
                            toggler(self.sms_mode).on_toggle(Message::SmsToggled),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Show the transcript as chat bubbles",
                        &scheme,
                    ),
                    with_tip(
                        row![
                            text("Speakers"),
                            horizontal_space(),
                            pick_list(
                                SpeakerOpt::ALL,
                                Some(SpeakerOpt::from_value(self.num_speakers)),
                                Message::SpeakerCountChanged,
                            )
                            .style(pill_pick),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Expected number of speakers",
                        &scheme,
                    ),
                    with_tip(
                        row![
                            text("Timestamps"),
                            horizontal_space(),
                            toggler(self.timestamps).on_toggle(Message::TimestampsToggled),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        "Prefix each line with its start time",
                        &scheme,
                    ),
                ]
                .spacing(8)
                .padding([0, 2]),
            );
        }

        controls = controls.push(
            row![
                with_tip(
                    button(if pending { "Start Queue" } else { "Start" })
                        .on_press_maybe(can_start.then_some(Message::Start))
                        .style(color_button(scheme.blue, scheme.ink)),
                    "Transcribe the queued files in order",
                    &scheme,
                ),
                with_tip(
                    button("Summary")
                        .on_press_maybe(
                            (!running && self.stats.is_some()).then_some(Message::OpenSummary)
                        )
                        .style(color_button(scheme.pink, scheme.ink)),
                    "Show the last run summary and rename speakers",
                    &scheme,
                ),
                with_tip(
                    button("Cancel")
                        .on_press_maybe(can_cancel.then_some(Message::Cancel))
                        .style(color_button(scheme.pink, scheme.ink)),
                    "Stop the current run",
                    &scheme,
                ),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );

        let mut status_row = row![
            text(self.elapsed().unwrap_or_default()).width(Length::Fixed(70.0)),
            stack![
                progress_bar(0.0..=1.0, self.progress_shown)
                    .width(Length::Fill)
                    .height(Length::Fixed(28.0))
                    .style(move |_theme| progress_bar::Style {
                        background: scheme.track.into(),
                        bar: scheme.blue.into(),
                        border: iced::border::rounded(14.0),
                    }),
                container(text(format!("{} — {}", self.stage, self.status_text())))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            ]
            .width(Length::Fill),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        if self.stats.as_ref().is_some_and(|stats| stats.cache_hit) {
            status_row = status_row.push(with_tip(
                container(text("cache hit").size(12))
                    .padding([3, 8])
                    .style(move |_theme| container::Style {
                        background: Some(scheme.blue.into()),
                        text_color: Some(scheme.ink),
                        border: iced::border::rounded(PILL),
                        ..container::Style::default()
                    }),
                "Diarization was reused from the cache for this run",
                &scheme,
            ));
        }

        let mut middle = column![status_row].spacing(8).width(Length::Fill);
        if let Some(timeline) = self.timeline_element(&scheme) {
            middle = middle.push(timeline);
        }
        if self.search_open {
            middle = middle.push(self.search_element(&scheme));
        }

        let base = column![
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
                    ..container::Style::default()
                }),
            container(
                row![
                    self.player_element(&scheme),
                    horizontal_space(),
                    with_tip(
                        button("Import Transcription")
                            .on_press_maybe((!running).then_some(Message::PickImport))
                            .style(color_button(scheme.blue, scheme.ink)),
                        "Open an existing transcript (.txt or .json)",
                        &scheme,
                    ),
                    with_tip(
                        button("Close").on_press(Message::Close).style(pill_button_secondary),
                        "Quit transiel",
                        &scheme,
                    ),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill),
        ]
        .spacing(8)
        .padding(12);

        if self.batch_warning {
            let dialog = container(
                column![
                    text("Warning").size(20),
                    text("Speakers can not be renamed in batch mode."),
                    container(
                        row![
                            button("Cancel")
                                .on_press(Message::BatchCancel)
                                .style(color_button(scheme.pink, scheme.ink)),
                            button("Continue")
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
                ..container::Style::default()
            });

            let overlay = container(dialog)
                .center(Length::Fill)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme| container::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.4).into()),
                    ..container::Style::default()
                });

            return stack![base, overlay].into();
        }

        if self.edit_open {
            let dialog = container(
                column![
                    text("Edit line").size(20),
                    text_input("Speaker: text", &self.edit_text)
                        .id(edit_id())
                        .on_input(Message::EditText)
                        .on_submit(Message::EditSave)
                        .width(Length::Fill)
                        .style(pill_input),
                    container(
                        row![
                            button("Save")
                                .on_press(Message::EditSave)
                                .style(color_button(scheme.blue, scheme.ink)),
                            button("Cancel")
                                .on_press(Message::EditCancel)
                                .style(color_button(scheme.pink, scheme.ink)),
                        ]
                        .spacing(8),
                    )
                    .width(Length::Fill)
                    .align_x(iced::Alignment::End),
                ]
                .spacing(12)
                .width(Length::Fixed(520.0)),
            )
            .padding(16)
            .style(move |_theme| container::Style {
                background: Some(scheme.dialog_bg.into()),
                border: iced::border::rounded(16.0),
                ..container::Style::default()
            });

            let overlay = container(dialog)
                .center(Length::Fill)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme| container::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.4).into()),
                    ..container::Style::default()
                });

            return stack![base, overlay].into();
        }

        if !self.rename_open {
            return base.into();
        }

        let mut col = column![text("Summary").size(20)].spacing(8);
        for line in self.stats_lines() {
            col = col.push(text(line));
        }
        col = col.push(text("Rename speakers").size(20));
        col = col.push(
            text_input("SPEAKER 1", &self.name1)
                .id(name1_id())
                .on_input(Message::RenameName1)
                .width(Length::Fill)
                .style(pill_input),
        );
        col = col.push(
            text_input("SPEAKER 2", &self.name2)
                .id(name2_id())
                .on_input(Message::RenameName2)
                .width(Length::Fill)
                .style(pill_input),
        );
        col = col.push(
            container(
                row![
                    button("Save")
                        .on_press(Message::RenameSave)
                        .style(color_button(scheme.blue, scheme.ink)),
                    button("Cancel")
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
                ..container::Style::default()
            });

        let overlay = container(dialog)
            .center(Length::Fill)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.4).into()),
                ..container::Style::default()
            });

        stack![base, overlay].into()
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
}

impl Timeline {
    fn time_at(&self, x: f32, width: f32) -> f32 {
        if width <= 0.0 || self.duration <= 0.0 {
            return 0.0;
        }
        (x / width).clamp(0.0, 1.0) * self.duration
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
            let bar_width = width / count as f32;
            let half = mid * 0.92;
            for (index, peak) in self.peaks.iter().enumerate() {
                let amplitude = (peak / peak_max).clamp(0.0, 1.0) * half;
                if amplitude <= 0.0 {
                    continue;
                }
                let x = index as f32 * bar_width;
                frame.fill_rectangle(
                    Point::new(x, mid - amplitude),
                    Size::new(bar_width.max(0.7), amplitude * 2.0),
                    self.wave,
                );
            }
        }

        if self.duration > 0.0 {
            for turn in self.turns.iter() {
                let x0 = (turn.start / self.duration).clamp(0.0, 1.0) * width;
                let x1 = (turn.end / self.duration).clamp(0.0, 1.0) * width;
                frame.fill_rectangle(
                    Point::new(x0, height - band_height),
                    Size::new((x1 - x0).max(1.0), band_height),
                    turn.color,
                );
            }
            let x = (self.playhead / self.duration).clamp(0.0, 1.0) * width;
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
}

impl Player {
    fn new(path: &Path) -> Option<Player> {
        let (stream, handle) = OutputStream::try_default().ok()?;
        let sink = Sink::try_new(&handle).ok()?;
        let file = std::fs::File::open(path).ok()?;
        let decoder = Decoder::new(std::io::BufReader::new(file)).ok()?;
        sink.append(decoder);
        sink.pause();
        Some(Player {
            _stream: stream,
            sink,
        })
    }

    fn is_playing(&self) -> bool {
        !self.sink.is_paused() && !self.sink.empty()
    }

    fn finished(&self) -> bool {
        self.sink.empty()
    }

    fn position(&self) -> f32 {
        self.sink.get_pos().as_secs_f32()
    }

    fn toggle(&self) {
        if self.sink.empty() {
            let _ = self.sink.try_seek(Duration::from_secs(0));
            self.sink.play();
        } else if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    fn seek(&self, seconds: f32) {
        let _ = self.sink.try_seek(Duration::from_secs_f32(seconds.max(0.0)));
    }
}

/// Play/pause glyph drawn on a canvas (the bundled font has no such icon).
struct PlayPause {
    playing: bool,
    color: Color,
}

impl canvas::Program<Message> for PlayPause {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let h = bounds.height;
        if self.playing {
            let bar = w * 0.28;
            frame.fill_rectangle(
                Point::new(w * 0.18, h * 0.14),
                Size::new(bar, h * 0.72),
                self.color,
            );
            frame.fill_rectangle(
                Point::new(w * 0.54, h * 0.14),
                Size::new(bar, h * 0.72),
                self.color,
            );
        } else {
            let path = canvas::Path::new(|path| {
                path.move_to(Point::new(w * 0.24, h * 0.12));
                path.line_to(Point::new(w * 0.84, h * 0.5));
                path.line_to(Point::new(w * 0.24, h * 0.88));
                path.close();
            });
            frame.fill(&path, self.color);
        }
        vec![frame.into_geometry()]
    }
}

struct RightArrow {
    color: Color,
}

impl<Message> canvas::Program<Message> for RightArrow {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let h = bounds.height;
        let cy = h / 2.0;
        let head_len = w * 0.55;
        let head_half = h * 0.42;
        let shaft_half = h * 0.14;
        let shaft = canvas::Path::rectangle(
            Point::new(0.0, cy - shaft_half),
            Size::new(w - head_len, shaft_half * 2.0),
        );
        let head = canvas::Path::new(|path| {
            path.move_to(Point::new(w - head_len, cy - head_half));
            path.line_to(Point::new(w, cy));
            path.line_to(Point::new(w - head_len, cy + head_half));
            path.close();
        });
        frame.fill(&shaft, self.color);
        frame.fill(&head, self.color);
        vec![frame.into_geometry()]
    }
}

fn arrow_glyph(color: Color) -> Element<'static, Message> {
    canvas(RightArrow { color })
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(14.0))
        .into()
}

fn with_tip<'a>(
    content: impl Into<Element<'a, Message>>,
    label: &'static str,
    scheme: &Scheme,
) -> Element<'a, Message> {
    let background = scheme.dialog_bg;
    let text_color = scheme.ink;
    let border_color = scheme.track;
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
                ..container::Style::default()
            }),
        tooltip::Position::Top,
    )
    .into()
}

/// Parse "[time] Speaker: text" transcript lines into the render model.
fn parse_lines(text: &str) -> Vec<DocLine> {
    text.lines()
        .map(|line| DocLine {
            raw: line.to_string(),
            speaker: speaker_of(line),
            confidence: 1.0,
        })
        .collect()
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

fn name1_id() -> text_input::Id {
    text_input::Id::new("rename-name1")
}

fn name2_id() -> text_input::Id {
    text_input::Id::new("rename-name2")
}

fn find_id() -> text_input::Id {
    text_input::Id::new("find-query")
}

fn edit_id() -> text_input::Id {
    text_input::Id::new("edit-line")
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
) -> Element<'static, Message> {
    let background = tint.unwrap_or(color);
    let bubble = container(rich_text(styled_spans(content, query, Some(ink), highlight)))
        .width(Length::Fill)
        .align_x(iced::Alignment::Start)
        .padding(6)
        .style(move |_theme| container::Style {
            background: Some(background.into()),
            border: iced::border::rounded(8.0),
            ..container::Style::default()
        });
    if left {
        row![bubble, horizontal_space()].spacing(8).into()
    } else {
        row![horizontal_space(), bubble].spacing(8).into()
    }
}

fn line_row(
    line: &str,
    tint: Option<Color>,
    query: &str,
    highlight: Color,
) -> Element<'static, Message> {
    let content = rich_text(styled_spans(line, query, None, highlight));
    let mut row = container(content)
        .width(Length::Fill)
        .padding([2, 4]);
    if let Some(color) = tint {
        row = row.style(move |_theme| container::Style {
            background: Some(color.into()),
            border: iced::border::rounded(4.0),
            ..container::Style::default()
        });
    }
    row.into()
}

fn sms_color(index: usize, scheme: &Scheme) -> Color {
    if index % 2 == 0 {
        scheme.sms0
    } else {
        scheme.sms1
    }
}

const PILL: f32 = 1000.0;

fn pill_button_secondary(theme: &iced::Theme, status: button::Status) -> button::Style {
    let mut style = button::secondary(theme, status);
    style.border.radius = PILL.into();
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
            ..button::Style::default()
        }
    }
}

fn styled_spans(
    raw: &str,
    query: &str,
    color: Option<Color>,
    highlight: Color,
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
        let mut span = text::Span::new(raw[a..b].to_string());
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
        spans.push(text::Span::new(raw.to_string()));
    }
    spans
}
