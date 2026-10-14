use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, Modifiers};
use iced::widget::{
    button, column, container, horizontal_space, mouse_area, pick_list, progress_bar, rich_text,
    row, scrollable, stack, text, text_input,
};
use iced::{font, Color, Element, Font, Length, Subscription, Task};

use crate::backend::{self, Event};

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
    events: Arc<Mutex<VecDeque<Event>>>,
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            input: None,
            output_dir: None,
            preset: Preset::default(),
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
            events: Arc::new(Mutex::new(VecDeque::new())),
            pid: Arc::new(AtomicU32::new(0)),
            cancelled: Arc::new(AtomicBool::new(false)),
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
                async { rfd::FileDialog::new().pick_file() },
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

    fn start(&mut self) {
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
                Event::Done { txt, stats, .. } => {
                    self.stage = "done".to_string();
                    self.progress = 1.0;
                    self.progress_shown = 1.0;
                    self.elapsed_final = self.run_start.map(|s| s.elapsed().as_secs_f64());
                    self.status = Status::Done;
                    self.stats = Some(stats);
                    if let Ok(contents) = std::fs::read_to_string(&txt) {
                        self.transcript = contents;
                    }
                    self.output_txt = Some(PathBuf::from(&txt));
                    self.name1.clear();
                    self.name2.clear();
                    self.rename_open = true;
                    self.rename_focus = 0;
                    self.focus_pending = true;
                }
                Event::Error { message } => {
                    self.stage = "error".to_string();
                    self.status = Status::Error(message);
                }
            }
        }
    }

    fn duration_label(&self) -> String {
        match self.duration {
            Some(secs) => format!("Duration: {}", format_secs(secs)),
            None if self.input.is_some() => "Duration: …".to_string(),
            None => String::new(),
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
                    format!("Speakers: {}", s.speakers),
                    format!("Segments: {}", s.lines),
                    format!("Preset: {}", s.preset),
                    format!("Diarizer: {}", s.diarizer),
                    format!("Transcriber: {}", s.transcriber),
                    format!("Device: {}", s.device),
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
                    spans.push(text::Span::new(format!("{head}: ")).font(bold).color(Color::BLACK));
                    spans.push(text::Span::new(format!("{rest}\n")).color(Color::BLACK));
                }
                None => spans.push(text::Span::new(format!("{line}\n")).color(Color::BLACK)),
            }
        }
        spans
    }

    pub fn view(&self) -> Element<'_, Message> {
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
        let can_select = !running;
        let can_output = !running && has_input;
        let can_preset = !running && has_input;
        let can_start = !running && has_input;
        let can_cancel = running;

        let preset: Element<Message> = if can_preset {
            pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged).into()
        } else {
            // Keep the real pick_list (so iced draws its arrow) but put a
            // click-capturing layer on top so it cannot be opened.
            let greyed = |theme: &iced::Theme, status: pick_list::Status| {
                let mut style = pick_list::default(theme, status);
                let grey = Color::from_rgb(0.55, 0.55, 0.55);
                style.text_color = grey;
                style.handle_color = grey;
                style.background = Color::from_rgb(0.9, 0.9, 0.9).into();
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

        let controls = column![
            row![
                button("Select File").on_press_maybe(can_select.then_some(Message::PickInput)),
                text(input.clone()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            text(self.duration_label()),
            row![
                button("Save Location").on_press_maybe(can_output.then_some(Message::PickOutput)),
                text(output.clone()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            row![text("Preset:"), preset]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            row![
                button("Start").on_press_maybe(can_start.then_some(Message::Start)),
                button("Summary")
                    .on_press_maybe((!running && self.stats.is_some()).then_some(Message::OpenSummary)),
                button("Cancel").on_press_maybe(can_cancel.then_some(Message::Cancel)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        ]
        .spacing(16);

        let base = column![
            controls,
            row![
                text(self.elapsed().unwrap_or_default()).width(Length::Fixed(70.0)),
                stack![
                    progress_bar(0.0..=1.0, self.progress_shown)
                        .width(Length::Fill)
                        .height(Length::Fixed(28.0))
                        .style(|_theme| progress_bar::Style {
                            background: Color::from_rgb(0.9, 0.9, 0.9).into(),
                            bar: Color::from_rgb(0.29, 0.53, 0.91).into(),
                            border: iced::border::rounded(6.0),
                        }),
                    container(
                        text(format!("{} — {}", self.stage, self.status_text()))
                            .color(Color::from_rgb(0.1, 0.1, 0.1)),
                    )
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
                ]
                .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            container(
                scrollable(rich_text(self.transcript_spans()))
                    .height(Length::Fill)
                    .width(Length::Fill)
                    .style(|theme, status| {
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
                    }),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::WHITE.into()),
                ..container::Style::default()
            }),
            row![button("Close").on_press(Message::Close)],
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
                .width(Length::Fill),
        );
        col = col.push(
            text_input("SPEAKER 2", &self.name2)
                .id(name2_id())
                .on_input(Message::RenameName2)
                .width(Length::Fill),
        );
        col = col.push(
            row![
                button("Cancel")
                    .on_press(Message::RenameCancel)
                    .style(if self.rename_focus == 3 {
                        button::primary
                    } else {
                        button::secondary
                    }),
                button("Save")
                    .on_press(Message::RenameSave)
                    .style(if self.rename_focus == 2 {
                        button::primary
                    } else {
                        button::secondary
                    }),
            ]
            .spacing(8),
        );
        let dialog = container(col.width(Length::Fixed(380.0)))
            .padding(16)
            .style(|_theme| container::Style {
                background: Some(Color::WHITE.into()),
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
