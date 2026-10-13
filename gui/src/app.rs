use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::widget::{button, column, pick_list, progress_bar, row, text, text_editor};
use iced::{Element, Length, Subscription, Task};

use crate::backend::{self, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    Speed,
    Balanced,
    #[default]
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
}

pub struct App {
    input: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    preset: Preset,
    stage: String,
    progress: f32,
    status: Status,
    transcript: text_editor::Content,
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
            status: Status::Idle,
            transcript: text_editor::Content::new(),
            events: Arc::new(Mutex::new(VecDeque::new())),
            pid: Arc::new(AtomicU32::new(0)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick)
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
                Task::none()
            }
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
        self.transcript = text_editor::Content::new();
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
                        self.transcript = text_editor::Content::with_text(&contents);
                    }
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

    pub fn view(&self) -> Element<'_, Message> {
        let input = self
            .input
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".to_string());
        let output = self
            .output_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(default: audio directory)".to_string());

        let controls = row![
            button("Select File").on_press(Message::PickInput),
            button("Save Location").on_press(Message::PickOutput),
            pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged),
            button("Start").on_press(Message::Start),
            button("Cancel").on_press(Message::Cancel),
        ]
        .spacing(8);

        column![
            controls,
            text(format!("Input: {input}")),
            text(format!("Output: {output}")),
            progress_bar(0.0..=1.0, self.progress),
            text(format!("{} — {}", self.stage, self.status_text())),
            text_editor(&self.transcript).height(Length::Fill),
        ]
        .spacing(8)
        .padding(12)
        .into()
    }
}
