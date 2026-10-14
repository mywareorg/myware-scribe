use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, Modifiers};
use serde::{Deserialize, Serialize};
use iced::widget::{
    button, column, container, horizontal_space, mouse_area, pick_list, progress_bar, rich_text,
    row, scrollable, stack, text, text_input, toggler,
};
use iced::{font, Color, Element, Font, Length, Subscription, Task};

use crate::backend::{self, Event};

const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "wma", "mp4", "mkv", "mov", "webm", "avi",
];

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
    PickInput,
    InputPicked(Option<PathBuf>),
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
    DurationProbed(Option<f64>),
    TimestampsToggled(bool),
    JoinSpeakerToggled(bool),
    ToggleAdvanced,
    SmsToggled(bool),
    DarkToggled(bool),
    PickImport,
    ImportPicked(Option<PathBuf>),
    ImportLoaded(Option<String>),
    AddFiles,
    FilesPicked(Option<Vec<PathBuf>>),
    AddFolder,
    FolderPicked(Option<PathBuf>),
    RemoveItem(usize),
    ClearQueue,
}

pub struct App {
    input: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    preset: Preset,
    stage: String,
    progress: f32,
    progress_shown: f32,
    run_start: Option<Instant>,
    elapsed_final: Option<f64>,
    status: Status,
    transcript: String,
    output_txt: Option<PathBuf>,
    rename_open: bool,
    rename_focus: usize,
    focus_pending: bool,
    name1: String,
    name2: String,
    duration: Option<f64>,
    stats: Option<backend::Stats>,
    timestamps: bool,
    join_speaker: bool,
    advanced_open: bool,
    sms_mode: bool,
    dark_mode: bool,
    events: Arc<Mutex<VecDeque<Event>>>,
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
    queue: Vec<QueueItem>,
    batch: bool,
    current: Option<usize>,
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
            input: None,
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
            output_txt: None,
            rename_open: false,
            rename_focus: 0,
            focus_pending: false,
            name1: String::new(),
            name2: String::new(),
            duration: None,
            stats: None,
            timestamps: settings.timestamps.unwrap_or(true),
            join_speaker: settings.join_speaker.unwrap_or(false),
            advanced_open: false,
            sms_mode: settings.sms_mode.unwrap_or(false),
            dark_mode: settings.dark_mode.unwrap_or(false),
            events: Arc::new(Mutex::new(VecDeque::new())),
            pid: Arc::new(AtomicU32::new(0)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue: Vec::new(),
            batch: false,
            current: None,
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
        };
        if let Ok(text) = serde_json::to_string_pretty(&settings) {
            let _ = std::fs::write(path, text);
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
        ])
    }

    fn key_press(key: Key, modifiers: Modifiers) -> Option<Message> {
        match key {
            Key::Named(keyboard::key::Named::Tab) => Some(Message::RenameMove(modifiers.shift())),
            Key::Named(keyboard::key::Named::Enter) => Some(Message::RenameActivate),
            _ => None,
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PickInput => Task::perform(
                async {
                    rfd::FileDialog::new()
                        .add_filter(
                            "Audio/Video",
                            &[
                                "mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "wma", "mp4",
                                "mkv", "mov", "webm", "avi",
                            ],
                        )
                        .pick_file()
                },
                Message::InputPicked,
            ),
            Message::InputPicked(path) => {
                if let Some(path) = path {
                    if self.output_dir.is_none() {
                        self.output_dir = path.parent().map(|p| p.to_path_buf());
                    }
                    self.duration = None;
                    self.input = Some(path.clone());
                    return Task::perform(
                        async move { backend::probe_duration(&path) },
                        Message::DurationProbed,
                    );
                }
                Task::none()
            }
            Message::DurationProbed(seconds) => {
                self.duration = seconds;
                Task::none()
            }
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
                    self.add_paths(paths);
                }
                Task::none()
            }
            Message::AddFolder => Task::perform(
                async { rfd::FileDialog::new().pick_folder() },
                Message::FolderPicked,
            ),
            Message::FolderPicked(dir) => {
                if let Some(dir) = dir {
                    self.add_paths(audio_files_in(&dir));
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
                let default_dir = self
                    .output_dir
                    .clone()
                    .or_else(|| self.input.as_ref().and_then(|p| p.parent().map(|p| p.to_path_buf())));
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
                if matches!(self.status, Status::Running) || self.input.is_none() {
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
                self.drain_events();
                // Ease the displayed bar toward the latest reported progress.
                self.progress_shown += (self.progress - self.progress_shown) * 0.25;
                self.progress_shown = self.progress_shown.clamp(0.0, 1.0);
                if self.focus_pending && self.rename_open {
                    self.focus_pending = false;
                    self.focus_task()
                } else {
                    Task::none()
                }
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

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            if self.queue.iter().any(|item| item.path == path) {
                continue;
            }
            self.queue.push(QueueItem {
                path,
                status: ItemStatus::Pending,
            });
        }
    }

    fn start(&mut self) {
        if self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending))
        {
            self.batch = true;
            self.transcript.clear();
            self.output_txt = None;
            self.stats = None;
            self.rename_open = false;
            self.name1.clear();
            self.name2.clear();
            self.cancelled.store(false, Ordering::SeqCst);
            self.run_next();
        } else {
            self.batch = false;
            self.start_single();
        }
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

        backend::spawn(
            python,
            root,
            input,
            base,
            self.preset.arg(),
            self.timestamps,
            self.join_speaker,
            self.events.clone(),
            self.pid.clone(),
            self.cancelled.clone(),
        );
    }

    fn start_single(&mut self) {
        let Some(input) = self.input.clone() else {
            self.status = Status::Error("select an audio file first".to_string());
            return;
        };
        let Some(root) = backend::repo_root() else {
            self.status = Status::Error(
                "cannot locate repo root (scripts/transiel.py); set TRANSIEL_ROOT".to_string(),
            );
            return;
        };
        let python = backend::venv_python(&root);
        if !python.is_file() {
            self.status = Status::Error(format!(
                "python not found at {}; run scripts/bootstrap.py",
                python.display()
            ));
            return;
        }

        let base = backend::output_base(self.output_dir.as_deref(), &input);

        self.status = Status::Running;
        self.stage = "starting".to_string();
        self.progress = 0.0;
        self.progress_shown = 0.0;
        self.run_start = Some(Instant::now());
        self.elapsed_final = None;
        self.transcript.clear();
        self.output_txt = None;
        self.stats = None;
        self.rename_open = false;
        self.name1.clear();
        self.name2.clear();
        self.cancelled.store(false, Ordering::SeqCst);

        backend::spawn(
            python,
            root,
            input,
            base,
            self.preset.arg(),
            self.timestamps,
            self.join_speaker,
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

        if let Some(path) = &self.output_txt {
            let _ = std::fs::write(path, &self.transcript);
        }
        self.rename_open = false;
    }

    fn drain_events(&mut self) {
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
                Event::Meta { .. } => {}
                Event::Turns { .. } => {}
                Event::Line { .. } => {}
                Event::Done { txt, stats, .. } => {
                    self.stage = "done".to_string();
                    self.progress = 1.0;
                    self.progress_shown = 1.0;
                    self.elapsed_final = self.run_start.map(|s| s.elapsed().as_secs_f64());
                    if let Ok(contents) = std::fs::read_to_string(&txt) {
                        self.transcript = contents;
                    }
                    self.output_txt = Some(PathBuf::from(&txt));
                    self.stats = Some(stats);
                    if self.batch {
                        if let Some(index) = self.current.take() {
                            self.queue[index].status = ItemStatus::Done(PathBuf::from(&txt));
                        }
                        self.run_next();
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
    }

    fn duration_label(&self) -> String {
        match self.duration {
            Some(secs) => format!("Duration: {}", format_secs(secs)),
            None if self.input.is_some() => "Duration: …".to_string(),
            None => "Duration: N/A".to_string(),
        }
    }

    fn stats_lines(&self) -> Vec<String> {
        match &self.stats {
            Some(s) => {
                let speed = if s.run_seconds > 0.0 {
                    s.audio_seconds / s.run_seconds
                } else {
                    0.0
                };
                vec![
                    format!("Total run time: {}", format_secs(s.run_seconds)),
                    format!("Audio length: {}", format_secs(s.audio_seconds)),
                    format!("Speed: {:.1}\u{00d7} realtime", speed),
                    format!("Segments: {}", s.lines),
                    format!("Preset: {}", s.preset),
                    format!("Diarizer: {}", s.diarizer),
                    format!("Transcriber: {}", s.transcriber),
                ]
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

    /// Full path of the human-readable output file for the current selection.
    fn output_file(&self) -> Option<PathBuf> {
        let input = self.input.as_ref()?;
        let base = backend::output_base(self.output_dir.as_deref(), input);
        Some(PathBuf::from(format!("{}.txt", base.display())))
    }

    /// Transcript lines with the timestamp and speaker name in bold.
    fn transcript_spans(&self) -> Vec<text::Span<'static, Message>> {
        let bold = Font {
            weight: font::Weight::Bold,
            ..Font::default()
        };
        let mut spans: Vec<text::Span<'static, Message>> = Vec::new();
        for line in self.transcript.lines() {
            match line.split_once(": ") {
                Some((head, rest)) => {
                    spans.push(text::Span::new(format!("{head}: ")).font(bold));
                    spans.push(text::Span::new(format!("{rest}\n")));
                }
                None => spans.push(text::Span::new(format!("{line}\n"))),
            }
        }
        spans
    }

    /// Transcript view: normal (rich text) or SMS-style bubbles.
    fn transcript_element(&self) -> Element<'_, Message> {
        if self.sms_mode {
            let scheme = Scheme::for_theme(self.dark_mode);
            let mut sides: Vec<String> = Vec::new();
            let mut col = column![].spacing(6).padding(8).width(Length::Fill);
            for line in self.transcript.lines() {
                let (label, _) = split_label(line);
                if label.is_empty() && line.trim().is_empty() {
                    continue;
                }
                let index = match sides.iter().position(|s| s == &label) {
                    Some(i) => i,
                    None => {
                        sides.push(label.clone());
                        sides.len() - 1
                    }
                };
                col = col.push(sms_row(
                    line.trim(),
                    index % 2 == 0,
                    sms_color(index, &scheme),
                    scheme.ink,
                ));
            }
            scrollable(col)
                .height(Length::Fill)
                .width(Length::Fill)
                .into()
        } else {
            scrollable(rich_text(self.transcript_spans()))
                .height(Length::Fill)
                .width(Length::Fill)
                .style(scrollbar_style)
                .into()
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let scheme = Scheme::for_theme(self.dark_mode);
        let input = self
            .input
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".to_string());
        let output = match self.output_file() {
            Some(path) => path.display().to_string(),
            None => self
                .output_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(default: audio directory)".to_string()),
        };

        let running = matches!(self.status, Status::Running);
        let has_input = self.input.is_some();
        let pending = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending));
        let has_queue = !self.queue.is_empty();
        let can_select = !running;
        let can_output = !running && has_input;
        let can_preset = !running && has_input;
        let can_start = !running && (has_input || pending);
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
                button("Select File")
                    .on_press_maybe(can_select.then_some(Message::PickInput))
                    .style(color_button(scheme.blue, scheme.ink)),
                text(input.clone()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            text(self.duration_label()),
            row![
                button("Save Location")
                    .on_press_maybe(can_output.then_some(Message::PickOutput))
                    .style(color_button(scheme.blue, scheme.ink)),
                text(output.clone()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            row![
                text("Preset:"),
                preset,
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
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        ]
        .spacing(16);

        let mut queue_list = column![].spacing(4).width(Length::Fill);
        for (index, item) in self.queue.iter().enumerate() {
            let (glyph, color) = match &item.status {
                ItemStatus::Pending => ("\u{2022}", scheme.ink),
                ItemStatus::Running => ("\u{25b6}", scheme.blue),
                ItemStatus::Done(_) => ("\u{2713}", Color::from_rgb(0.3, 0.72, 0.42)),
                ItemStatus::Failed(_) => ("\u{2717}", scheme.pink),
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
            queue_list = queue_list.push(
                row![
                    text(glyph.to_string()).style(move |_theme| text::Style { color: Some(color) }),
                    text(format!("{name}{detail}")).width(Length::Fill),
                    button("\u{00d7}")
                        .on_press_maybe(can_select.then_some(Message::RemoveItem(index)))
                        .style(pill_button_secondary),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }

        let mut queue_col = column![
            row![
                button("Add Files")
                    .on_press_maybe(can_select.then_some(Message::AddFiles))
                    .style(color_button(scheme.blue, scheme.ink)),
                button("Add Folder")
                    .on_press_maybe(can_select.then_some(Message::AddFolder))
                    .style(color_button(scheme.blue, scheme.ink)),
                button("Clear")
                    .on_press_maybe((!running && has_queue).then_some(Message::ClearQueue))
                    .style(color_button(scheme.pink, scheme.ink)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        ]
        .spacing(6);

        if has_queue {
            queue_col = queue_col.push(
                scrollable(queue_list)
                    .height(Length::Fixed(140.0))
                    .width(Length::Fill),
            );
        }

        controls = controls.push(queue_col);

        if self.advanced_open {
            controls = controls.push(
                column![
                    row![
                        text("Timestamps"),
                        horizontal_space(),
                        toggler(self.timestamps).on_toggle(Message::TimestampsToggled),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    row![
                        text("Join Speakers"),
                        horizontal_space(),
                        toggler(self.join_speaker).on_toggle(Message::JoinSpeakerToggled),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    row![
                        text("SMS Mode"),
                        horizontal_space(),
                        toggler(self.sms_mode).on_toggle(Message::SmsToggled),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    row![
                        text("Dark theme"),
                        horizontal_space(),
                        toggler(self.dark_mode).on_toggle(Message::DarkToggled),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                ]
                .spacing(8)
                .padding([0, 2]),
            );
        }

        controls = controls.push(
            row![
                button(if pending { "Start Queue" } else { "Start" })
                    .on_press_maybe(can_start.then_some(Message::Start))
                    .style(color_button(scheme.blue, scheme.ink)),
                button("Summary")
                    .on_press_maybe(
                        (!running && self.stats.is_some()).then_some(Message::OpenSummary)
                    )
                    .style(color_button(scheme.pink, scheme.ink)),
                button("Cancel")
                    .on_press_maybe(can_cancel.then_some(Message::Cancel))
                    .style(color_button(scheme.pink, scheme.ink)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );

        let base = column![
            text("Transiel").size(32),
            controls,
            row![
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
            .align_y(iced::Alignment::Center),
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
                    button("Import Transcription")
                        .on_press_maybe((!running).then_some(Message::PickImport))
                        .style(color_button(scheme.blue, scheme.ink)),
                    button("Close").on_press(Message::Close).style(pill_button_secondary),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .align_x(iced::Alignment::End),
        ]
        .spacing(8)
        .padding(12);

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

fn split_label(line: &str) -> (String, String) {
    let rest = match line.strip_prefix('[').and_then(|s| s.find("] ").map(|i| &s[i + 2..])) {
        Some(stripped) => stripped,
        None => line,
    };
    match rest.split_once(": ") {
        Some((label, text)) => (label.trim().to_string(), text.trim().to_string()),
        None => (String::new(), String::new()),
    }
}

fn sms_row(content: &str, left: bool, color: Color, ink: Color) -> Element<'static, Message> {
    let bubble = container(rich_text(line_spans(content, Some(ink))))
        .width(Length::Fill)
        .align_x(iced::Alignment::Start)
        .padding(6)
        .style(move |_theme| container::Style {
            background: Some(color.into()),
            border: iced::border::rounded(8.0),
            ..container::Style::default()
        });
    if left {
        row![bubble, horizontal_space()].spacing(8).into()
    } else {
        row![horizontal_space(), bubble].spacing(8).into()
    }
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

fn line_spans(line: &str, color: Option<Color>) -> Vec<text::Span<'static, Message>> {
    let bold = Font {
        weight: font::Weight::Bold,
        ..Font::default()
    };
    let mut spans = Vec::new();
    match line.split_once(": ") {
        Some((head, rest)) => {
            let mut h = text::Span::new(format!("{head}: ")).font(bold);
            if let Some(c) = color {
                h = h.color(c);
            }
            spans.push(h);
            let mut r = text::Span::new(rest.to_string());
            if let Some(c) = color {
                r = r.color(c);
            }
            spans.push(r);
        }
        None => {
            let mut s = text::Span::new(line.to_string());
            if let Some(c) = color {
                s = s.color(c);
            }
            spans.push(s);
        }
    }
    spans
}
