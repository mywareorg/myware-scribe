use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, Modifiers};
use iced::widget::{
    button, column, container, pick_list, progress_bar, rich_text, row, scrollable, stack, text,
    text_input,
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
}

pub struct App {
    input: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    preset: Preset,
    stage: String,
    progress: f32,
    run_start: Option<Instant>,
    status: Status,
    transcript: String,
    output_txt: Option<PathBuf>,
    rename_open: bool,
    rename_focus: usize,
    focus_pending: bool,
    name1: String,
    name2: String,
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
            run_start: None,
            status: Status::Idle,
            transcript: String::new(),
            output_txt: None,
            rename_open: false,
            rename_focus: 0,
            focus_pending: false,
            name1: String::new(),
            name2: String::new(),
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
                    self.input = Some(path);
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
        self.run_start = Some(Instant::now());
        self.transcript.clear();
        self.output_txt = None;
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
        self.run_start = None;
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
                Event::Done { txt, .. } => {
                    self.stage = "done".to_string();
                    self.progress = 1.0;
                    self.status = Status::Done;
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
        if !matches!(self.status, Status::Running | Status::Done) {
            return None;
        }
        let start = self.run_start?;
        Some(format_secs(start.elapsed().as_secs_f64()))
    }

    fn status_line(&self) -> String {
        let status = format!("{} — {}", self.stage, self.status_text());
        match self.elapsed() {
            Some(elapsed) => format!("{elapsed} · {status}"),
            None => status,
        }
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

        let preset = pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged).style(
            move |theme, status| {
                let mut style = iced::widget::pick_list::default(theme, status);
                if !can_preset {
                    let grey = Color::from_rgb(0.55, 0.55, 0.55);
                    style.text_color = grey;
                    style.handle_color = grey;
                    style.background = Color::from_rgb(0.9, 0.9, 0.9).into();
                }
                style
            },
        );

        let controls = column![
            row![
                button("Select File").on_press_maybe(can_select.then_some(Message::PickInput)),
                text(input.clone()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
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
                button("Cancel").on_press_maybe(can_cancel.then_some(Message::Cancel)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        ]
        .spacing(16);

        let base = column![
            controls,
            progress_bar(0.0..=1.0, self.progress),
            text(self.status_line()),
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

        let dialog = container(
            column![
                text("Rename speakers"),
                text_input("SPEAKER 1", &self.name1)
                    .id(name1_id())
                    .on_input(Message::RenameName1)
                    .width(Length::Fill),
                text_input("SPEAKER 2", &self.name2)
                    .id(name2_id())
                    .on_input(Message::RenameName2)
                    .width(Length::Fill),
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
            ]
            .spacing(8)
            .width(Length::Fixed(320.0)),
        )
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
