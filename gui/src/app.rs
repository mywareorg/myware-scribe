use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use iced::keyboard::{self, Key, Modifiers};
use iced::widget::{
    button, canvas, column, container, horizontal_rule, horizontal_space, mouse_area, pick_list,
    progress_bar, rich_text, row, scrollable, stack, text, text_editor, text_input, tooltip,
};
use iced::{
    font, mouse, Color, Element, Font, Length, Pixels, Point, Rectangle, Renderer, Shadow, Size,
    Subscription, Task, Theme, Vector,
};
use rodio::{Decoder, OutputStream, Sink};
use serde::{Deserialize, Serialize};

use crate::backend::{self, Event};
use hippo_transcribe_core::license;
use crate::theme::{self, AppTheme, Scheme};
use crate::{documents::*, files::*, format::*, options::*};
use hippo_transcribe_core::download::{self, DownloadEvent, ModelStatus};
use hippo_transcribe_core::models;

const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "wma", "mp4", "mkv", "mov", "webm", "avi",
];

/// Height of the waveform/speaker timeline strip.
const TIMELINE_HEIGHT: f32 = 72.0;

/// Logical 4:3 sizing for the window (default) and floors.
const DEFAULT_W: f32 = 1024.0;
const DEFAULT_H: f32 = 768.0; // 1024 * 3 / 4
const MIN_W: f32 = 800.0;
const MIN_H: f32 = 600.0;

/// Aspect-ratio snap grid (pixels). 4 keeps things crisp.
const ASPECT_SNAP_PX: f32 = 4.0;
const ASPECT: f32 = 4.0 / 3.0; // w/h
const ASPECT_INV: f32 = 3.0 / 4.0; // h/w

const PANEL_GAP: f32 = 12.0;
const OUTER_PADDING: f32 = 12.0;
const LEFT_PANEL_WIDTH: f32 = 340.0;

/// UI chrome never wraps; labels are shortened or ellipsized instead.
const NO_WRAP: iced::widget::text::Wrapping = iced::widget::text::Wrapping::None;

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
    /// Paths included in the most recent transcription batch.
    #[serde(default)]
    last_batch: Vec<String>,
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

/// Cadence at which streamed utterances are revealed in the textbox.
const REVEAL_INTERVAL_S: f32 = 0.15;
/// Faster cadence used to catch up when the reveal backlog grows.
const REVEAL_CATCHUP_S: f32 = 0.04;
const REVEAL_BACKLOG: usize = 8;

#[derive(Serialize, Deserialize)]
struct Settings {
    preset: Option<String>,
    sms_mode: Option<bool>,
    num_speakers: Option<i32>,
    language: Option<String>,
    #[serde(default = "timestamps_default")]
    show_timestamps: bool,
    #[serde(default = "notifications_default")]
    notifications: bool,
    font_scale: Option<f32>,
    playback_speed: Option<f32>,
    waveform_zoom: Option<f32>,
    follow_playhead: Option<bool>,
    window_width: Option<f32>,
    window_height: Option<f32>,
    window_x: Option<f32>,
    window_y: Option<f32>,
    output_dir: Option<String>,
    theme: Option<String>,
}

fn notifications_default() -> bool {
    true
}

fn timestamps_default() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: None,
            sms_mode: None,
            num_speakers: None,
            language: None,
            show_timestamps: true,
            notifications: true,
            font_scale: None,
            playback_speed: None,
            waveform_zoom: None,
            follow_playhead: None,
            window_width: None,
            window_height: None,
            window_x: None,
            window_y: None,
            output_dir: None,
            theme: None,
        }
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

/// State of an in-flight model download.
struct ModelDownload {
    label: String,
    done: u64,
    total: Option<u64>,
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
    SelectRelative(i32),
    SelectRelativeChecked(i32, bool),
    EditActiveLine,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFollowPlayhead,
    ToggleTheme,
    SetTheme(AppTheme),
    OpenSettings,
    CloseSettings,
    OpenPaywall,
    ClosePaywall,
    PurchasePro,
    RestorePro,
    StartTrial,
    DownloadModels,
    CancelModelDownload,
    ClearPartialModels,
    DismissToast,
    WaveZoom(f32),
    HelpChoice(HelpKind),
    CloseHelp,
    Noop,
    OpenSummary,
    SmsToggled(bool),
    NotificationsToggled(bool),
    TimestampsToggled(bool),
    SpeakerCountChanged(SpeakerOpt),
    LanguageChanged(LanguageOpt),
    TranscriptScrolled(scrollable::Viewport),
    WindowEvent(iced::window::Event),
    Seek(f32),
    PlayerToggle,
    FocusFind,
    SearchChanged(String),
    SpeakerFilterChanged(String),
    FindPrev,
    FindNext,
    SpeedChanged(Speed),
    Escape,
    ShortcutPlay,
    ShortcutPlayChecked(bool),
    SeekBy(f32),
    SeekByChecked(f32, bool),
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
    CopyLineEdit,
    MergeNextLine,
    SplitLineAtPlayhead,
    DeleteLineEdit,
    ConfirmDeleteLine,
    CancelDeleteLine,
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
    RetryItem(usize),
    MoveItemUp(usize),
    MoveItemDown(usize),
    ResetOutputDir,
    OpenModelsFolder,
}

pub struct App {
    output_dir: Option<PathBuf>,
    preset: Preset,
    theme_mode: AppTheme,
    license: license::State,
    provider: Box<dyn license::Provider>,
    paywall_open: bool,
    stage: String,
    progress: f32,
    progress_shown: f32,
    overall_shown: f32,
    run_start: Option<Instant>,
    elapsed_final: Option<f64>,
    eta_shown: Option<f32>,
    eta_rate: Option<f32>,
    eta_sample: Option<(Instant, f32)>,
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
    notifications: bool,
    show_timestamps: bool,
    num_speakers: Option<i32>,
    language: Option<String>,
    /// Derived orders, invalidated whenever the shown lines change.
    display_order_cache: RefCell<Option<Vec<usize>>>,
    speaker_order_cache: RefCell<Option<Vec<String>>>,
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
    settings_open: bool,
    search_query: String,
    speaker_filter: Option<String>,
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
    /// Queue paths belonging to the current (or most recent) transcription
    /// batch, including follow-up passes for files added during a run.
    batch_paths: Vec<PathBuf>,
    current: Option<usize>,
    current_index: Option<usize>,
    /// Index of the completed queue item currently shown, if any.
    selected: Option<usize>,
    selected_view: Option<ItemView>,
    /// Error shown in the transcript pane when a completed item cannot load.
    view_error: Option<String>,
    /// Right-clicked line currently being edited.
    line_edit: Option<LineEdit>,
    /// The line editor asked to delete; confirmation is required before saving.
    delete_confirm: bool,
    /// Background waveform backfill for completed items without cached peaks.
    backfill_active: bool,
    waveform_tried: HashSet<PathBuf>,
    /// Presence of the bundled-on-demand models, refreshed periodically.
    models: Vec<ModelStatus>,
    model_download: Option<ModelDownload>,
    model_error: Option<String>,
    model_events: Arc<Mutex<VecDeque<DownloadEvent>>>,
    model_cancel: Arc<AtomicBool>,
    last_model_check: Option<Instant>,
    /// A file is hovering over the window.
    drag_hover: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Snap a requested window size to the 4:3 grid, honoring `MIN_W`/`MIN_H`.
/// Pick the dimension the user actually moved (heuristic: the one that
/// changed more from `last`) and derive the other from it. Both outputs
/// are rounded to `ASPECT_SNAP_PX` and clamped to the floors without
/// breaking the ratio.
fn aspect_clamp(requested: Size, last: Size) -> Size {
    let dw = (requested.width - last.width).abs();
    let dh = (requested.height - last.height).abs();
    // 4:3 means w/h = 4/3, so h = w * 3/4 and w = h * 4/3.
    let (raw_w, raw_h) = if dw >= dh {
        let h = (requested.width * ASPECT_INV / ASPECT_SNAP_PX).round() * ASPECT_SNAP_PX;
        (requested.width, h)
    } else {
        let w = (requested.height * ASPECT / ASPECT_SNAP_PX).round() * ASPECT_SNAP_PX;
        (w, requested.height)
    };
    let (mut w, mut h) = (raw_w, raw_h);
    // Enforce min floor without breaking the ratio.
    if w < MIN_W {
        h = (MIN_W * ASPECT_INV / ASPECT_SNAP_PX).round() * ASPECT_SNAP_PX;
        w = MIN_W;
    }
    if h < MIN_H {
        w = (MIN_H * ASPECT / ASPECT_SNAP_PX).round() * ASPECT_SNAP_PX;
        h = MIN_H;
    }
    Size::new(w, h)
}

impl App {
    pub fn new() -> Self {
        let settings = Self::load_settings();
        let (queue, batch_paths) = Self::load_queue_and_batch();
        let now = SystemTime::now();
        let provider = license::provider();
        let pro_override = provider.override_tier();
        let mut license_state = license::State::load(
            &Self::license_path().unwrap_or_else(|| PathBuf::from("license.json")),
            now,
        );
        license_state.refresh(pro_override, now);
        // A downgrade must never leave a Pro-only preset selected.
        let mut preset = settings
            .preset
            .as_deref()
            .and_then(Preset::from_arg)
            .unwrap_or_default();
        if preset != Preset::Speed
            && !license::tier_allows(
                license_state.access.tier,
                license::Feature::BalancedPreset,
            )
        {
            preset = Preset::Speed;
        }
        Self {
            output_dir: settings.output_dir.as_ref().map(PathBuf::from),
            preset,
            theme_mode: settings
                .theme
                .as_deref()
                .and_then(AppTheme::from_arg)
                .unwrap_or_default(),
            license: license_state,
            provider,
            paywall_open: false,
            stage: "idle".to_string(),
            progress: 0.0,
            progress_shown: 0.0,
            overall_shown: 0.0,
            run_start: None,
            elapsed_final: None,
            eta_shown: None,
            eta_rate: None,
            eta_sample: None,
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
            notifications: settings.notifications,
            show_timestamps: settings.show_timestamps,
            num_speakers: match settings.num_speakers {
                Some(count) if count > 0 => Some(count),
                _ => Some(2),
            },
            language: settings.language.clone(),
            display_order_cache: RefCell::new(None),
            speaker_order_cache: RefCell::new(None),
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
            settings_open: false,
            waveform_zoom: settings.waveform_zoom.unwrap_or(1.0),
            search_query: String::new(),
            speaker_filter: None,
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
            queue,
            batch: false,
            batch_paths,
            current: None,
            current_index: None,
            selected: None,
            selected_view: None,
            view_error: None,
            line_edit: None,
            delete_confirm: false,
            backfill_active: false,
            waveform_tried: HashSet::new(),
            models: download::status(),
            model_download: None,
            model_error: None,
            model_events: Arc::new(Mutex::new(VecDeque::new())),
            model_cancel: Arc::new(AtomicBool::new(false)),
            last_model_check: Some(Instant::now()),
            drag_hover: false,
        }
    }

    /// Current configuration directory (`.../hippo-transcribe`).
    fn config_dir() -> Option<PathBuf> {
        if cfg!(windows) {
            std::env::var("APPDATA")
                .ok()
                .map(|base| PathBuf::from(base).join("hippo-transcribe"))
        } else {
            let base = std::env::var("XDG_CONFIG_HOME").ok().or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|home| format!("{home}/.config"))
            })?;
            Some(PathBuf::from(base).join("hippo-transcribe"))
        }
    }

    /// Configuration directory used before the rename; read-only fallback.
    fn legacy_config_dir() -> Option<PathBuf> {
        if cfg!(windows) {
            std::env::var("APPDATA")
                .ok()
                .map(|base| PathBuf::from(base).join("transiel"))
        } else {
            let base = std::env::var("XDG_CONFIG_HOME").ok().or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|home| format!("{home}/.config"))
            })?;
            Some(PathBuf::from(base).join("transiel"))
        }
    }

    fn settings_path() -> Option<PathBuf> {
        Self::config_dir().map(|dir| dir.join("settings.json"))
    }

    fn license_path() -> Option<PathBuf> {
        license::license_path()
    }

    fn legacy_settings_path() -> Option<PathBuf> {
        Self::legacy_config_dir().map(|dir| dir.join("settings.json"))
    }

    fn load_settings() -> Settings {
        Self::settings_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .or_else(|| {
                Self::legacy_settings_path().and_then(|path| std::fs::read_to_string(path).ok())
            })
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
            num_speakers: Some(self.num_speakers.unwrap_or(2)),
            language: self.language.clone(),
            notifications: self.notifications,
            show_timestamps: self.show_timestamps,
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
            theme: Some(self.theme_mode.arg().to_string()),
        };
        if let Ok(text) = serde_json::to_string_pretty(&settings) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Queue state lives beside the settings file.
    fn queue_path() -> Option<PathBuf> {
        Self::settings_path().map(|path| path.with_file_name("queue.json"))
    }

    fn legacy_queue_path() -> Option<PathBuf> {
        Self::legacy_settings_path().map(|path| path.with_file_name("queue.json"))
    }

    /// Load the queue plus the paths from the most recent batch.
    fn load_queue_and_batch() -> (Vec<QueueItem>, Vec<PathBuf>) {
        let text = Self::queue_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .or_else(|| {
                Self::legacy_queue_path().and_then(|path| std::fs::read_to_string(path).ok())
            });
        let Some(text) = text else {
            return (Vec::new(), Vec::new());
        };
        let Ok(state) = serde_json::from_str::<QueueState>(&text) else {
            return (Vec::new(), Vec::new());
        };
        let batch_paths = state.last_batch.into_iter().map(PathBuf::from).collect();
        let items = state
            .items
            .into_iter()
            .filter_map(|record| {
                let audio = record.audio.map(PathBuf::from);
                let transcript = record.transcript.map(PathBuf::from);
                let path = audio.clone().or_else(|| transcript.clone())?;
                let mut status = match record.status.as_str() {
                    // A run interrupted by a restart is simply pending again.
                    "running" => ItemStatus::Pending,
                    "failed" => {
                        ItemStatus::Failed(record.message.unwrap_or_else(|| "failed".to_string()))
                    }
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
            .collect();
        (items, batch_paths)
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
                    transcript: item
                        .transcript
                        .as_ref()
                        .map(|path| path.display().to_string()),
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
            last_batch: self
                .batch_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(path, text);
        }
    }

    // ---- displayed view (live run vs. a selected completed item) ----

    /// Drop the cached display/speaker orders after lines or speakers change.
    fn invalidate_derived(&mut self) {
        self.display_order_cache.replace(None);
        self.speaker_order_cache.replace(None);
    }

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
        if matches!(item.status, ItemStatus::Pending) {
            self.speaker_filter = None;
            self.player = None;
            self.playhead = 0.0;
            self.last_tick = None;
            self.search_query.clear();
            self.search_index = 0;
            self.view_error = None;
            self.selected = Some(index);
            self.selected_view = Some(ItemView {
                index,
                source: item.path.clone(),
                transcript: String::new(),
                lines: Vec::new(),
                turns: item.turns.clone(),
                peaks: item.peaks.clone(),
                duration: item.duration.map(|seconds| seconds as f32),
                audio: item.audio.clone(),
                output: None,
            });
            self.invalidate_derived();
            return Task::none();
        }
        if !matches!(item.status, ItemStatus::Done) {
            return Task::none();
        }
        let Some(source) = item.transcript.clone() else {
            return Task::none();
        };
        self.speaker_filter = None;
        let audio = item.audio.clone();
        let duration = item.duration.map(|seconds| seconds as f32);
        let turns = item.turns.clone();
        let peaks = item.peaks.clone();

        self.invalidate_derived();
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
            None => match backend::read_transcript(&source) {
                Some(text) => {
                    // JSON imports are displayed read-only; only plain .txt
                    // transcripts are safe to edit in place.
                    let editable = source
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("txt"))
                        .unwrap_or(false);
                    Some((text, editable.then(|| source.clone())))
                }
                None => {
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
                    self.view_error = Some(format!("Could not read {}", source.display()));
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
                    move |turns| Message::ViewDetailsLoaded(index, source.clone(), turns, None),
                ));
            }
        }
        Task::batch(tasks)
    }

    /// Drop a pinned completed-item view and show the live run again.
    fn return_to_live(&mut self) {
        self.invalidate_derived();
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
            min_size: Some(Size::new(MIN_W, MIN_H)),
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
        match (settings.window_width, settings.window_height) {
            (Some(width), Some(height)) if width >= 100.0 && height >= 100.0 => {
                // Re-snap whatever was persisted to the 4:3 grid, in case a
                // legacy size from an earlier version was stored without
                // aspect enforcement.
                aspect_clamp(Size::new(width, height), Size::new(width, height))
            }
            _ => Size::new(DEFAULT_W, DEFAULT_H),
        }
    }

    pub fn theme(&self) -> iced::Theme {
        self.theme_mode.iced()
    }

    /// Colours for the current appearance mode.
    fn scheme(&self) -> Scheme {
        self.theme_mode.scheme()
    }

    /// True when the effective tier unlocks `feature`.
    fn can(&self, feature: license::Feature) -> bool {
        license::tier_allows(self.license.access.tier, feature)
    }

    /// Free runs ASR only; Pro adds diarization and speaker labels.
    fn diarized(&self) -> bool {
        self.can(license::Feature::Diarization)
    }

    /// Whether this run actually executes the diarization stage. One speaker
    /// takes the fastest path even on Pro, matching `pipeline::route`.
    fn pipelines_diarize(&self) -> bool {
        self.diarized() && self.num_speakers != Some(1)
    }

    fn save_license(&self) {
        if let Some(path) = Self::license_path() {
            self.license.save(&path);
        }
    }

    /// Drop back to the free preset after a trial/downgrade.
    fn clamp_preset(&mut self) {
        if self.preset != Preset::Speed && !self.can(license::Feature::BalancedPreset) {
            self.preset = Preset::Speed;
            self.save_settings();
        }
    }

    /// Header control showing the current tier and opening the paywall.
    fn license_button<'a>(&'a self, scheme: &Scheme) -> Element<'a, Message> {
        let label = match self.license.access.tier {
            license::Tier::Pro if self.license.access.source == license::Source::Trial => "Pro trial",
            license::Tier::Pro => "Pro",
            license::Tier::Free => "Get Pro",
        };
        let button: Element<Message> = if self.license.access.tier == license::Tier::Pro {
            action_text(label)
                .on_press(Message::OpenPaywall)
                .style(pill_button_secondary)
                .into()
        } else {
            action_text(label)
                .on_press(Message::OpenPaywall)
                .style(color_button(scheme.blue, scheme.on_accent))
                .into()
        };
        self.tip(
            button,
            "Speaker labels, batch transcription and larger models",
            scheme,
        )
    }

    /// The Pro paywall / license dialog.
    fn paywall_dialog<'a>(&'a self, scheme: &Scheme) -> Element<'a, Message> {
        let scheme = *scheme;
        let price = self
            .provider
            .price()
            .unwrap_or_else(|| license::FALLBACK_PRICE.to_string());
        let mut body = column![
            text("Hippo Transcribe Pro").size(20),
            text(format!("{price} once \u{00b7} lifetime license"))
                .size(theme::TEXT_SM)
                .color(scheme.muted),
        ]
        .spacing(10);

        for feature in license::Feature::ALL {
            body = body.push(
                row![
                    text("\u{2713}").size(theme::TEXT_SM).color(scheme.green),
                    text(feature.label()).size(theme::TEXT_SM),
                ]
                .spacing(6),
            );
        }

        body = body.push(
            text(
                "Free keeps transcription with the speed model, editing, copy, \
                 save, playback and search.",
            )
            .size(theme::TEXT_XS)
            .color(scheme.muted),
        );

        let mut actions = row![].spacing(8).align_y(iced::Alignment::Center);
        if self.license.access.tier == license::Tier::Pro {
            actions = actions.push(
                text(format!(
                    "Pro is active ({}).",
                    self.license.access.source.label()
                ))
                .size(theme::TEXT_SM)
                .color(scheme.green),
            );
        } else {
            if !self.license.trial_used() {
                actions = actions.push(
                    action_text("Start 7-day trial")
                        .on_press(Message::StartTrial)
                        .style(pill_button_secondary),
                );
            }
            if self.provider.name() == "none" {
                actions = actions.push(
                    text("Purchasing is not available in this build yet.")
                        .size(theme::TEXT_XS)
                        .color(scheme.muted),
                );
            } else {
                actions = actions.push(
                    action_text("Buy Pro")
                        .on_press(Message::PurchasePro)
                        .style(color_button(scheme.blue, scheme.on_accent)),
                );
            }
            actions = actions.push(
                action_text("Restore purchase")
                    .on_press(Message::RestorePro)
                    .style(pill_button_secondary),
            );
        }

        body = body.push(
            container(actions)
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
        );
        body = body.push(
            container(
                row![horizontal_space(),
                    action_text("Close").on_press(Message::ClosePaywall).style(pill_button_secondary)]
                .spacing(8),
            )
            .width(Length::Fill)
            .align_x(iced::Alignment::End),
        );

        modal_card(body.spacing(10), 460.0, scheme)
    }

    /// True when the selected preset has every model file it needs.
    fn models_ready(&self) -> bool {
        download::ready_for(self.preset.arg(), self.pipelines_diarize())
    }

    fn refresh_models(&mut self) {
        self.models = download::status();
        self.last_model_check = Some(Instant::now());
        if self.models_ready() {
            self.model_error = None;
        }
    }

    /// Start the one-time model download on a background thread. Progress is
    /// delivered through `model_events` and drained on `Tick`.
    fn start_model_download(&mut self) {
        if self.model_download.is_some() {
            return;
        }
        self.model_error = None;
        self.model_cancel.store(false, Ordering::SeqCst);
        self.model_download = Some(ModelDownload {
            label: "starting".to_string(),
            done: 0,
            total: None,
        });
        let events = self.model_events.clone();
        let cancel = self.model_cancel.clone();
        if let Ok(mut queue) = events.lock() {
            queue.clear();
        }
        let ids = download::required_for(self.preset.arg(), self.pipelines_diarize());
        let spawned = std::thread::Builder::new()
            .name("model-download".to_string())
            .spawn(move || {
                download::download_ids(&ids, false, false, cancel, &mut |event| {
                    if let Ok(mut queue) = events.lock() {
                        queue.push_back(event);
                    }
                });
            });
        if spawned.is_err() {
            self.model_download = None;
            self.model_error = Some("Could not start the download".to_string());
        }
    }

    fn drain_model_download(&mut self) {
        loop {
            let event = self
                .model_events
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front());
            let Some(event) = event else {
                break;
            };
            match event {
                DownloadEvent::Started { label } => {
                    if let Some(state) = &mut self.model_download {
                        state.label = friendly_model(label).to_string();
                        state.done = 0;
                        state.total = None;
                    }
                }
                DownloadEvent::Progress { label, done, total } => {
                    if let Some(state) = &mut self.model_download {
                        state.label = friendly_model(label).to_string();
                        state.done = done;
                        state.total = total;
                    }
                }
                DownloadEvent::Completed { label, .. } => {
                    if let Some(state) = &mut self.model_download {
                        state.label = friendly_model(label).to_string();
                        if let Some(total) = state.total {
                            state.done = total;
                        }
                    }
                }
                DownloadEvent::Failed { label, message } => {
                    self.model_error = Some(format!("{}: {message}", friendly_model(label)));
                }
                DownloadEvent::Finished { ok } => {
                    self.model_download = None;
                    self.model_cancel.store(false, Ordering::SeqCst);
                    self.refresh_models();
                    if ok {
                        self.toast = Some(("Models downloaded".to_string(), Instant::now()));
                    } else {
                        if self.model_error.is_none() {
                            self.model_error =
                                Some("One or more model files could not be downloaded".to_string());
                        }
                        self.toast = Some(("Model download failed".to_string(), Instant::now()));
                    }
                }
            }
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
            Key::Named(Named::Enter) if modifiers.alt() => Some(Message::EditActiveLine),
            Key::Named(Named::Enter) if modifiers.command() => Some(Message::Start),
            Key::Named(Named::Enter) if modifiers.shift() => Some(Message::ShiftEnterSubmit),
            Key::Named(Named::Enter) => Some(Message::EnterSubmit),
            Key::Named(Named::Escape) => Some(Message::Escape),
            Key::Named(Named::Space) => Some(Message::ShortcutPlay),
            Key::Named(Named::ArrowUp) => Some(Message::SelectRelative(-1)),
            Key::Named(Named::ArrowDown) => Some(Message::SelectRelative(1)),
            Key::Named(Named::ArrowRight) => Some(Message::SeekBy(5.0)),
            Key::Named(Named::ArrowLeft) => Some(Message::SeekBy(-5.0)),
            Key::Character(character) if modifiers.command() => match character.as_str() {
                "," => Some(Message::OpenSettings),
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

    /// Ask the widget tree whether a focusable input currently owns focus.
    fn focus_probe() -> Task<bool> {
        iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::count())
            .map(|count| count.focused.is_some())
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SmsToggled(value) => {
                self.sms_mode = value;
                self.save_settings();
                Task::none()
            }
            Message::NotificationsToggled(value) => {
                self.notifications = value;
                self.save_settings();
                Task::none()
            }
            Message::TimestampsToggled(value) => {
                self.show_timestamps = value;
                self.save_settings();
                Task::none()
            }
            Message::SpeakerCountChanged(option) => {
                if matches!(self.status, Status::Running) {
                    return Task::none();
                }
                self.num_speakers = option.value();
                self.save_settings();
                Task::none()
            }
            Message::LanguageChanged(option) => {
                if matches!(self.status, Status::Running) {
                    return Task::none();
                }
                self.language = (option != LanguageOpt::Auto).then(|| option.code().to_string());
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
                let programmatic = self
                    .scroll_target
                    .take()
                    .is_some_and(|target| (offset - target).abs() <= 6.0);

                // Scrolling never disables follow; only the toggle does. With
                // follow on, snap back to the followed line immediately. With
                // it off, treat the scroll as a seek to the visible line.
                if following {
                    if let Some(expected) = self.follow_offset {
                        if !programmatic && (offset - expected).abs() > 6.0 {
                            self.scroll_target = Some(expected);
                            return scrollable::scroll_to(
                                transcript_id(),
                                scrollable::AbsoluteOffset {
                                    x: 0.0,
                                    y: expected,
                                },
                            );
                        }
                    }
                } else if !programmatic {
                    self.seek_from_scroll(offset, viewport_h);
                }
                Task::none()
            }
            Message::WindowEvent(event) => {
                match event {
                    iced::window::Event::Resized(size) => {
                        // Iced 0.13 reports the OS-driven size; we cannot push
                        // a corrected resize back to the OS during a manual
                        // drag. Snap the size we *record* (and persist) to the
                        // 4:3 grid so the next launch and the layout math
                        // always see an aspect-locked window.
                        let previous = self.window_size;
                        self.window_size = aspect_clamp(size, previous);
                        self.window_dirty = true;
                        self.last_window_change = Some(Instant::now());
                    }
                    iced::window::Event::Moved(point) => {
                        self.window_position = Some(point);
                        self.window_dirty = true;
                        self.last_window_change = Some(Instant::now());
                    }
                    iced::window::Event::FileHovered(_) => {
                        self.drag_hover = true;
                    }
                    iced::window::Event::FilesHoveredLeft => {
                        self.drag_hover = false;
                    }
                    iced::window::Event::FileDropped(path) => {
                        self.drag_hover = false;
                        let paths = if path.is_dir() {
                            audio_files_in(&path)
                        } else if is_audio(&path) {
                            vec![path]
                        } else {
                            Vec::new()
                        };
                        if paths.is_empty() {
                            self.toast = Some((
                                "Drop audio files to add them to the queue".to_string(),
                                Instant::now(),
                            ));
                            return Task::none();
                        }
                        let added = self.add_paths(paths);
                        if added.is_empty() {
                            self.toast = Some(("Already in the queue".to_string(), Instant::now()));
                            return Task::none();
                        }
                        let count = added.len();
                        self.toast = Some((
                            format!(
                                "Added {count} file{} to the queue",
                                if count == 1 { "" } else { "s" }
                            ),
                            Instant::now(),
                        ));
                        return Self::probe_items(added);
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
                    self.invalidate_derived();
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
                self.invalidate_derived();
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
            Message::RetryItem(index) => {
                if matches!(self.status, Status::Running) {
                    return Task::none();
                }
                let retried = self.queue.get_mut(index).is_some_and(|item| {
                    if matches!(item.status, ItemStatus::Failed(_)) {
                        item.status = ItemStatus::Pending;
                        true
                    } else {
                        false
                    }
                });
                if retried {
                    self.invalidate_derived();
                    self.save_queue();
                }
                Task::none()
            }
            Message::MoveItemUp(index) => {
                if !matches!(self.status, Status::Running) {
                    self.move_queue_item(index, index.saturating_sub(1));
                }
                Task::none()
            }
            Message::MoveItemDown(index) => {
                if !matches!(self.status, Status::Running) {
                    self.move_queue_item(index, index.saturating_add(1));
                }
                Task::none()
            }
            Message::ResetOutputDir => {
                if !matches!(self.status, Status::Running) {
                    self.output_dir = None;
                    self.save_settings();
                }
                Task::none()
            }
            Message::OpenModelsFolder => {
                let dir = models::models_dir();
                let _ = std::fs::create_dir_all(&dir);
                open_path(&dir);
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
            Message::SpeakerFilterChanged(choice) => {
                let next = if choice == "All speakers" {
                    None
                } else {
                    Some(choice)
                };
                if next != self.speaker_filter {
                    self.speaker_filter = next;
                    self.search_index = 0;
                    self.invalidate_derived();
                    let first = self.display_order().first().copied();
                    match first {
                        Some(index) => self.scroll_to_line(index),
                        None => Task::none(),
                    }
                } else {
                    Task::none()
                }
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
            Message::SelectRelative(delta) => {
                if self.rename_open || self.line_edit.is_some() || self.settings_open {
                    return Task::none();
                }
                Self::focus_probe()
                    .map(move |focused| Message::SelectRelativeChecked(delta, focused))
            }
            Message::SelectRelativeChecked(delta, focused) => {
                if focused
                    || self.rename_open
                    || self.line_edit.is_some()
                    || self.settings_open
                    || self.delete_confirm
                {
                    return Task::none();
                }
                let order = self.display_order();
                if order.is_empty() {
                    return Task::none();
                }
                let current = self
                    .active_line_index()
                    .and_then(|index| order.iter().position(|item| *item == index))
                    .unwrap_or(0);
                let next = (current as i32 + delta).clamp(0, order.len() as i32 - 1) as usize;
                let index = order[next];
                let scroll = self.scroll_to_line(index);
                Task::batch([scroll, Task::done(Message::SelectLine(index))])
            }
            Message::EditActiveLine => {
                if self.rename_open || self.settings_open || self.delete_confirm {
                    return Task::none();
                }
                let index = self
                    .active_line_index()
                    .or_else(|| self.display_order().first().copied());
                match index {
                    Some(index) => Task::done(Message::EditLine(index)),
                    None => Task::none(),
                }
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
                if self.settings_open || self.delete_confirm {
                    return Task::none();
                }
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
                if self.settings_open || self.delete_confirm {
                    return Task::none();
                }
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
            Message::ToggleTheme => {
                self.theme_mode = self.theme_mode.toggled();
                self.save_settings();
                Task::none()
            }
            Message::SetTheme(mode) => {
                self.theme_mode = mode;
                self.save_settings();
                Task::none()
            }
            Message::OpenSettings => {
                self.settings_open = true;
                self.help_open = None;
                self.toast = None;
                Task::none()
            }
            Message::OpenPaywall => {
                self.paywall_open = true;
                Task::none()
            }
            Message::ClosePaywall => {
                self.paywall_open = false;
                Task::none()
            }
            Message::StartTrial => {
                let now = SystemTime::now();
                if self.license.start_trial(now) {
                    self.save_license();
                    self.clamp_preset();
                    self.toast = Some(("Pro trial started".to_string(), Instant::now()));
                } else {
                    self.toast = Some((
                        "The Pro trial has already been used".to_string(),
                        Instant::now(),
                    ));
                }
                Task::none()
            }
            Message::PurchasePro => {
                let now = SystemTime::now();
                let state = self.provider.purchase();
                self.license.purchase = state;
                match state {
                    license::PurchaseState::Active => {
                        self.license.grant_pro(now);
                        self.save_license();
                        self.clamp_preset();
                        self.paywall_open = false;
                        self.toast = Some(("Pro unlocked".to_string(), Instant::now()));
                    }
                    license::PurchaseState::Unavailable => {
                        self.toast = Some((
                            "Purchasing is not available in this build yet".to_string(),
                            Instant::now(),
                        ));
                    }
                    license::PurchaseState::Cancelled => {
                        self.toast = Some(("Purchase cancelled".to_string(), Instant::now()));
                    }
                    license::PurchaseState::Failed => {
                        self.toast = Some(("Purchase failed".to_string(), Instant::now()));
                    }
                    license::PurchaseState::Pending | license::PurchaseState::Idle => {
                        self.toast =
                            Some((state.label().to_string(), Instant::now()));
                    }
                }
                Task::none()
            }
            Message::RestorePro => {
                let now = SystemTime::now();
                let state = self.provider.restore();
                self.license.purchase = state;
                if state == license::PurchaseState::Active {
                    self.license.grant_pro(now);
                    self.save_license();
                    self.clamp_preset();
                    self.paywall_open = false;
                    self.toast = Some(("Purchase restored".to_string(), Instant::now()));
                } else {
                    self.toast = Some((
                        format!("No purchase to restore ({})", state.label()),
                        Instant::now(),
                    ));
                }
                Task::none()
            }
            Message::CloseSettings => {
                self.settings_open = false;
                Task::none()
            }
            Message::DownloadModels => {
                self.start_model_download();
                Task::none()
            }
            Message::CancelModelDownload => {
                self.model_cancel.store(true, Ordering::SeqCst);
                self.toast = Some(("Stopping model download".to_string(), Instant::now()));
                Task::none()
            }
            Message::ClearPartialModels => {
                let cleared = download::clear_partial();
                self.toast = Some((
                    if cleared.is_empty() {
                        "No partial downloads to clear".to_string()
                    } else {
                        format!("Cleared {} partial file(s)", cleared.len())
                    },
                    Instant::now(),
                ));
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
                if self.delete_confirm {
                    self.delete_confirm = false;
                } else if self.line_edit.is_some() {
                    self.line_edit = None;
                } else if self.rename_open {
                    self.rename_open = false;
                } else if self.settings_open {
                    self.settings_open = false;
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
                if self.rename_open || self.line_edit.is_some() || self.settings_open {
                    return Task::none();
                }
                Self::focus_probe().map(Message::ShortcutPlayChecked)
            }
            Message::ShortcutPlayChecked(focused) => {
                if !focused && !self.rename_open && self.line_edit.is_none() && !self.settings_open
                {
                    self.toggle_playback();
                }
                Task::none()
            }
            Message::SeekBy(delta) => {
                if self.rename_open || self.line_edit.is_some() || self.settings_open {
                    return Task::none();
                }
                Self::focus_probe().map(move |focused| Message::SeekByChecked(delta, focused))
            }
            Message::SeekByChecked(delta, focused) => {
                if focused || self.rename_open || self.line_edit.is_some() || self.settings_open {
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
                    self.toast =
                        Some(("Could not read that transcript".to_string(), Instant::now()));
                    return Task::none();
                };
                let imported_name = transcript
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| transcript.display().to_string());
                self.toast = Some((format!("Imported {imported_name}"), Instant::now()));
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
                    if added.is_empty() {
                        self.toast = Some(("Already in the queue".to_string(), Instant::now()));
                        return Task::none();
                    }
                    self.toast = Some((
                        format!(
                            "Added {} {} to the queue",
                            added.len(),
                            if added.len() == 1 { "file" } else { "files" }
                        ),
                        Instant::now(),
                    ));
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
                    let files = audio_files_in(&dir);
                    if files.is_empty() {
                        self.toast =
                            Some(("No audio files in that folder".to_string(), Instant::now()));
                        return Task::none();
                    }
                    let added = self.add_paths(files);
                    if added.is_empty() {
                        self.toast = Some(("Already in the queue".to_string(), Instant::now()));
                        return Task::none();
                    }
                    self.toast = Some((
                        format!(
                            "Added {} {} to the queue",
                            added.len(),
                            if added.len() == 1 { "file" } else { "files" }
                        ),
                        Instant::now(),
                    ));
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
                    self.selected = self
                        .selected
                        .and_then(|selected| match selected.cmp(&index) {
                            std::cmp::Ordering::Equal => None,
                            std::cmp::Ordering::Greater => Some(selected - 1),
                            std::cmp::Ordering::Less => Some(selected),
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
                    self.invalidate_derived();
                    self.save_queue();
                }
                Task::none()
            }
            Message::ClearQueue => {
                if !matches!(self.status, Status::Running) {
                    // "Clear" is a full reset: queue, transcript and run state.
                    self.queue.clear();
                    self.batch_paths.clear();
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
                    self.eta_shown = None;
                    self.progress = 0.0;
                    self.progress_shown = 0.0;
                    self.overall_shown = 0.0;
                    self.stage = "idle".to_string();
                    self.status = Status::Idle;
                    self.invalidate_derived();
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
            Message::CopyLineEdit => {
                let Some(edit) = &self.line_edit else {
                    return Task::none();
                };
                let text = edit.content.text();
                let text = text.trim_end().replace(['\n', '\r'], " ");
                let line = if edit.speaker.trim().is_empty() {
                    text
                } else {
                    format!("{}: {text}", edit.speaker.trim())
                };
                self.toast = Some(("Copied line".to_string(), Instant::now()));
                iced::clipboard::write(line)
            }
            Message::MergeNextLine => {
                self.merge_next_line();
                Task::none()
            }
            Message::SplitLineAtPlayhead => {
                self.split_line_at_playhead();
                Task::none()
            }
            Message::CancelLineEdit => {
                self.line_edit = None;
                Task::none()
            }
            Message::DeleteLineEdit => {
                if self.line_edit.is_some() {
                    self.delete_confirm = true;
                }
                Task::none()
            }
            Message::ConfirmDeleteLine => {
                self.delete_confirm = false;
                self.delete_edit_line();
                Task::none()
            }
            Message::CancelDeleteLine => {
                self.delete_confirm = false;
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
                        if write_transcript_file(path, text).is_ok() {
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
                        self.toast = Some(("Already in the queue".to_string(), Instant::now()));
                        return Task::none();
                    }
                    let added = self.add_paths(vec![path]);
                    if let Some(path) = added.first() {
                        let name = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("file")
                            .to_string();
                        self.toast = Some((format!("Added {name} to the queue"), Instant::now()));
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
                if matches!(self.status, Status::Running) {
                    return Task::none();
                }
                let feature = match preset {
                    Preset::Speed => None,
                    Preset::Balanced => Some(license::Feature::BalancedPreset),
                    Preset::Accuracy => Some(license::Feature::AccuracyPreset),
                };
                if let Some(feature) = feature {
                    if !self.can(feature) {
                        self.paywall_open = true;
                        self.toast = Some((
                            format!("{} is a Pro feature", feature.label()),
                            Instant::now(),
                        ));
                        return Task::none();
                    }
                }
                self.preset = preset;
                self.save_settings();
                Task::none()
            }
            Message::Start => {
                if !self.settings_open {
                    self.start();
                }
                Task::none()
            }
            Message::Cancel => {
                self.cancel();
                Task::none()
            }
            Message::Tick => {
                let mut tasks = vec![self.drain_events()];
                self.drain_model_download();
                if self.model_download.is_none()
                    && self
                        .last_model_check
                        .is_none_or(|last| last.elapsed() > Duration::from_secs(5))
                {
                    self.refresh_models();
                }
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

                // Ease an overall bar weighted by audio duration, so long files
                // move it proportionally further than short ones.
                let target_overall = self.overall_progress_target();
                self.overall_shown += (target_overall - self.overall_shown) * 0.25;
                self.overall_shown = self.overall_shown.clamp(0.0, 1.0);

                // Estimate remaining wall-clock time from the observed work
                // rate, not from elapsed / fraction. Chunked progress gives
                // frequent samples, and a short EMA rejects outliers.
                if matches!(self.status, Status::Running) {
                    if let Some(start) = self.run_start {
                        // Keep the estimate monotonic: a phase change or a
                        // newly probed duration can move the weighted target
                        // backwards, which would otherwise spike the ETA.
                        let target = self.overall_progress_target();
                        let fraction = self
                            .eta_sample
                            .map_or(target, |(_, previous)| target.max(previous));
                        let now = Instant::now();
                        // Only advance the sampling window when there is a
                        // usable amount of progress; overwriting it every
                        // 100ms tick would make `dt` too small to ever
                        // produce a rate sample.
                        match self.eta_sample {
                            Some((last, previous)) => {
                                let dt = (now - last).as_secs_f32();
                                let df = fraction - previous;
                                if dt >= 0.5 && df > 0.0005 {
                                    let sample = df / dt;
                                    self.eta_rate = Some(match self.eta_rate {
                                        Some(rate) => rate * 0.75 + sample * 0.25,
                                        None => sample,
                                    });
                                    self.eta_sample = Some((now, fraction));
                                } else if dt >= 5.0 {
                                    // Stalled (decoding, phase change): drop
                                    // the stale window instead of averaging
                                    // the pause into the next sample.
                                    self.eta_sample = Some((now, fraction));
                                }
                            }
                            None => self.eta_sample = Some((now, fraction)),
                        }
                        if fraction > 0.01 && fraction < 0.995 {
                            // Prefer the measured rate. Before one exists,
                            // only extrapolate once enough of the run has
                            // completed that `elapsed / fraction` is sane.
                            let raw = self
                                .eta_rate
                                .filter(|rate| *rate > 0.0001)
                                .map(|rate| ((1.0 - fraction) / rate).max(0.0))
                                .or_else(|| {
                                    (fraction > 0.1).then(|| {
                                        let elapsed = start.elapsed().as_secs_f32();
                                        (elapsed * (1.0 - fraction) / fraction).max(0.0)
                                    })
                                });
                            match raw {
                                Some(raw) => {
                                    self.eta_shown = Some(match self.eta_shown {
                                        Some(previous) => previous + (raw - previous) * 0.15,
                                        None => raw,
                                    });
                                }
                                None => self.eta_shown = None,
                            }
                        } else {
                            self.eta_shown = None;
                        }
                    }
                } else {
                    self.eta_shown = None;
                    self.eta_rate = None;
                    self.eta_sample = None;
                }

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
                            self.playhead = (self.playhead + dt * self.playback_speed).min(limit);
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
        self.rename_fields = seen.into_iter().map(|name| (name, String::new())).collect();
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
            let _ = write_transcript_file(path, &self.transcript);
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
        open_path(&dir);
    }

    /// Swap one queue entry with another, keeping selection indices in sync.
    fn move_queue_item(&mut self, from: usize, to: usize) {
        if from == to || to >= self.queue.len() || from >= self.queue.len() {
            return;
        }
        self.queue.swap(from, to);
        let remap = |index: &mut Option<usize>| match *index {
            Some(value) if value == from => *index = Some(to),
            Some(value) if value == to => *index = Some(from),
            _ => {}
        };
        remap(&mut self.current);
        remap(&mut self.current_index);
        remap(&mut self.selected);
        if let Some(view) = self.selected_view.as_mut() {
            if view.index == from {
                view.index = to;
            } else if view.index == to {
                view.index = from;
            }
        }
        self.invalidate_derived();
        self.save_queue();
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let mut paths = paths;
        if !self.can(license::Feature::Batch) {
            let has_work = self
                .queue
                .iter()
                .any(|item| matches!(item.status, ItemStatus::Pending | ItemStatus::Running));
            if has_work {
                self.paywall_open = true;
                self.toast = Some((
                    "Batch transcription is a Pro feature".to_string(),
                    Instant::now(),
                ));
                return Vec::new();
            }
            if paths.len() > 1 {
                self.paywall_open = true;
                self.toast = Some((
                    "Free transcribes one file per run".to_string(),
                    Instant::now(),
                ));
                paths.truncate(1);
            }
        }
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
        if self.notifications {
            notify("Hippo Transcribe", "Finished \u{00b7} saved locally");
        }
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
        self.invalidate_derived();
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
                self.toast = Some(("Could not open the audio file".to_string(), Instant::now()));
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
        if self.queue.is_empty() {
            self.status = Status::Error("add at least one file".to_string());
            return;
        }
        let has_pending = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending));
        if !has_pending {
            // Re-run the queue, including files transcribed with a previous
            // preset or files that failed.
            for item in &mut self.queue {
                if matches!(item.status, ItemStatus::Done | ItemStatus::Failed(_)) {
                    item.status = ItemStatus::Pending;
                    item.transcript = None;
                    item.stats = None;
                    item.turns.clear();
                    item.peaks = Arc::new(Vec::new());
                }
            }
            self.invalidate_derived();
            self.save_queue();
        }
        self.begin_batch(false);
    }

    /// Spawn a single pipeline process for all pending queue items, so the
    /// models load once and are reused across the whole batch. `preserve_view`
    /// keeps a completed transcript (and its playback) visible across an
    /// automatic follow-up pass; explicit starts always return to live.
    fn begin_batch(&mut self, preserve_view: bool) {
        if !self.models_ready() {
            self.status = Status::Error("transcription models are missing".to_string());
            self.toast = Some((
                "Download the transcription models before starting".to_string(),
                Instant::now(),
            ));
            return;
        }
        let preserve = preserve_view && self.keep_pinned_view();
        let mut inputs: Vec<PathBuf> = self
            .queue
            .iter()
            .filter(|item| matches!(item.status, ItemStatus::Pending))
            .map(|item| item.path.clone())
            .collect();
        // A downgrade must not grant a batch that was queued while Pro.
        if !self.can(license::Feature::Batch) && inputs.len() > 1 {
            inputs.truncate(1);
            self.toast = Some((
                "Free transcribes one file per run; the rest stay queued".to_string(),
                Instant::now(),
            ));
        }
        if inputs.is_empty() {
            self.status = Status::Error("add at least one file".to_string());
            return;
        }

        if preserve_view {
            // A follow-up pass is part of the same user-visible batch.
            for path in &inputs {
                if !self.batch_paths.contains(path) {
                    self.batch_paths.push(path.clone());
                }
            }
        } else {
            self.batch_paths = inputs.clone();
        }
        self.save_queue();

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
        self.eta_shown = None;
        self.eta_rate = None;
        self.eta_sample = None;
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
        // Each explicit transcription starts in follow mode; only the user's
        // toggle can turn it off, and a follow-up pass keeps that choice.
        if !preserve {
            self.follow_playhead = true;
            self.save_settings();
        }
        self.turns.clear();
        self.peaks = Arc::new(Vec::new());
        self.peaks_for = None;
        self.timeline_duration = None;
        self.output_txt = None;
        self.rename_open = false;
        self.invalidate_derived();
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
            self.language.clone(),
            true, // diarization cache is always on
            self.pipelines_diarize(),
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
        self.eta_shown = None;
        self.eta_rate = None;
        self.eta_sample = None;
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
                let _ = write_transcript_file(path, &view.transcript);
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
        self.invalidate_derived();
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
        let text = edit.content.text().trim_end().replace(['\n', '\r'], " ");
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
                let _ = write_transcript_file(path, &view.transcript);
            }
        } else if let Some(line) = self.lines.get_mut(index) {
            rewrite_line(line, &speaker, &text);
            self.refresh_transcript();
        }
        self.invalidate_derived();
    }

    /// Append the next line to the one open in the editor.
    fn merge_next_line(&mut self) {
        let Some(edit) = self.line_edit.take() else {
            return;
        };
        let index = edit.index;
        let speaker = edit.speaker.trim().to_string();
        let text = edit.content.text().trim_end().replace(['\n', '\r'], " ");
        let mut merged = false;

        if let Some(view) = self.selected_view.as_mut() {
            if index < view.lines.len() {
                rewrite_line(&mut view.lines[index], &speaker, &text);
                if index + 1 < view.lines.len() {
                    let next = line_text(&view.lines[index + 1]);
                    let joined = join_line_text(&text, &next);
                    rewrite_line(&mut view.lines[index], &speaker, &joined);
                    view.lines.remove(index + 1);
                    merged = true;
                }
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
                    let _ = write_transcript_file(path, &view.transcript);
                }
            }
        } else if index < self.lines.len() {
            rewrite_line(&mut self.lines[index], &speaker, &text);
            if index + 1 < self.lines.len() {
                let next = line_text(&self.lines[index + 1]);
                let joined = join_line_text(&text, &next);
                rewrite_line(&mut self.lines[index], &speaker, &joined);
                self.lines.remove(index + 1);
                merged = true;
            }
            self.refresh_transcript();
        }

        self.invalidate_derived();
        self.toast = Some((
            if merged {
                "Merged with next line".to_string()
            } else {
                "No next line to merge".to_string()
            },
            Instant::now(),
        ));
    }

    /// Split the edited line at the current playhead position.
    fn split_line_at_playhead(&mut self) {
        let Some(edit) = self.line_edit.as_ref() else {
            return;
        };
        let index = edit.index;
        let speaker = edit.speaker.trim().to_string();
        let text = edit.content.text().trim_end().replace(['\n', '\r'], " ");
        let (line_start, next_start) = if let Some(view) = self.selected_view.as_ref() {
            (
                view.lines.get(index).and_then(|line| line.start),
                view.lines.get(index + 1).and_then(|line| line.start),
            )
        } else {
            (
                self.lines.get(index).and_then(|line| line.start),
                self.lines.get(index + 1).and_then(|line| line.start),
            )
        };
        let start = line_start.unwrap_or(0.0);
        let end = next_start
            .or(self.timeline_duration)
            .unwrap_or(start + 5.0)
            .max(start + 0.1);
        let ratio = ((self.playhead - start) / (end - start)).clamp(0.15, 0.85);
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        if chars.len() < 2 {
            self.toast = Some(("Line is too short to split".to_string(), Instant::now()));
            return;
        }
        let target = ((chars.len() as f32) * ratio).round() as usize;
        let mut split_at = None;
        let mut best = usize::MAX;
        for (count, (byte, ch)) in chars.iter().enumerate() {
            if ch.is_whitespace() {
                let distance = count.abs_diff(target);
                if distance < best {
                    best = distance;
                    split_at = Some(*byte);
                }
            }
        }
        let split_at = split_at
            .filter(|byte| *byte > 0 && *byte < text.len())
            .or_else(|| chars.get(target).map(|(byte, _)| *byte))
            .unwrap_or(text.len());
        let left = text[..split_at].trim_end();
        let right = text[split_at..].trim_start();
        if left.is_empty() || right.is_empty() {
            self.toast = Some(("Cannot split here".to_string(), Instant::now()));
            return;
        }
        self.line_edit = None;
        let prefix = if line_start.is_some() {
            format!("[{}] ", format_timestamp(self.playhead as f64))
        } else {
            String::new()
        };
        let new_raw = if speaker.is_empty() {
            format!("{prefix}{right}")
        } else {
            format!("{prefix}{speaker}: {right}")
        };
        let new_line = DocLine {
            raw: new_raw,
            speaker: speaker.clone(),
            start: line_start.map(|_| self.playhead.max(start)),
        };

        if let Some(view) = self.selected_view.as_mut() {
            if index < view.lines.len() {
                rewrite_line(&mut view.lines[index], &speaker, left);
                view.lines.insert(index + 1, new_line);
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
                    let _ = write_transcript_file(path, &view.transcript);
                }
            }
        } else if index < self.lines.len() {
            rewrite_line(&mut self.lines[index], &speaker, left);
            self.lines.insert(index + 1, new_line);
            self.refresh_transcript();
        }
        self.invalidate_derived();
        self.toast = Some(("Split line at playhead".to_string(), Instant::now()));
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
                    let _ = write_transcript_file(path, &view.transcript);
                }
                removed = true;
            }
        } else if index < self.lines.len() {
            self.lines.remove(index);
            self.refresh_transcript();
            removed = true;
        }
        if removed {
            self.invalidate_derived();
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
                Event::Stage {
                    stage,
                    label,
                    progress,
                } => {
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
                    self.eta_shown = None;
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
                    self.invalidate_derived();
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
                    self.invalidate_derived();
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
                    self.invalidate_derived();
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
                                if matches!(item.status, ItemStatus::Pending | ItemStatus::Running)
                                {
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
                    self.eta_shown = None;
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
            // Restrict the automatic summary to the files this run processed,
            // not every completed item left in the queue from earlier runs.
            if !self.batch_paths.is_empty() && !self.batch_paths.contains(&item.path) {
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
            None => s.speakers.to_string(),
        };
        let mut lines = vec![
            format!("Total run time: {}", format_secs(s.run_seconds)),
            format!("Audio length: {}", format_secs(s.audio_seconds)),
            format!("Speed: {:.1}\u{00d7} realtime", speed),
            format!("Speakers: {speakers}"),
            format!("Preset: {}", s.preset),
        ];
        if !s.speaker_stats.is_empty() {
            let mut stats: Vec<(&String, &backend::SpeakerStat)> = s.speaker_stats.iter().collect();
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
            return self
                .run_start
                .map(|start| format_secs(start.elapsed().as_secs_f64()));
        }
        None
    }

    /// Overall progress weighted by audio duration. Only the items captured
    /// when the run began contribute; queue edits during a run are ignored.
    /// Outside a run the bar is frozen, so adding/removing queue items cannot
    /// move it.
    fn overall_progress_target(&self) -> f32 {
        if !matches!(self.status, Status::Running) && !self.completion_pending {
            return self.overall_shown;
        }
        if self.batch_paths.is_empty() {
            return 0.0;
        }

        let batch: Vec<&QueueItem> = self
            .queue
            .iter()
            .filter(|item| self.batch_paths.contains(&item.path))
            .collect();
        if batch.is_empty() {
            return 0.0;
        }
        let known: Vec<f32> = batch
            .iter()
            .filter_map(|item| item.duration.map(|duration| duration as f32))
            .filter(|duration| *duration > 0.0)
            .collect();
        let fallback = if known.is_empty() {
            1.0
        } else {
            known.iter().sum::<f32>() / known.len() as f32
        };
        let total: f32 = batch.iter().map(|item| queue_weight(item, fallback)).sum();
        if total <= 0.0 {
            return 0.0;
        }

        let mut completed = 0.0;
        for item in &batch {
            let finished = matches!(item.status, ItemStatus::Done | ItemStatus::Failed(_));
            let current = self
                .current_index
                .and_then(|index| self.queue.get(index))
                .is_some_and(|current| current.path == item.path);
            if finished && !current {
                completed += queue_weight(item, fallback);
            } else if current {
                let fraction = if finished || self.stage.eq_ignore_ascii_case("done") {
                    1.0
                } else {
                    self.progress.clamp(0.0, 1.0)
                };
                completed += queue_weight(item, fallback) * fraction;
            }
        }
        (completed / total).clamp(0.0, 1.0)
    }

    fn timer_text(&self) -> String {
        let elapsed = self.elapsed().unwrap_or_else(|| "00:00".to_string());
        if matches!(self.status, Status::Running) {
            if let Some(eta) = self.eta_shown {
                if eta >= 1.0 && eta.is_finite() {
                    return format!("{elapsed} \u{00b7} ~{} left", format_secs(eta as f64));
                }
            }
        }
        elapsed
    }

    /// Collapsible console showing backend log output.
    fn console_panel(&self, scheme: &Scheme) -> Element<'static, Message> {
        let background = scheme.track;
        let mut col = column![].spacing(1).padding(4);
        for line in &self.console_lines {
            col = col.push(text(line.clone()).size(12));
        }
        container(
            scrollable(col)
                .id(console_id())
                .height(Length::Fill)
                .width(Length::Fill),
        )
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
        if self.search_query.trim().is_empty() {
            return Vec::new();
        }
        let lines = self.shown_lines();
        self.display_order()
            .into_iter()
            .filter(|index| {
                lines.get(*index).is_some_and(|line| {
                    !search_match_ranges(&line.raw, &self.search_query, false, false).is_empty()
                })
            })
            .collect()
    }

    /// Distinct speakers in transcript order (matches the SMS bubble ordering).
    fn speaker_order(&self) -> Vec<String> {
        if let Some(order) = self.speaker_order_cache.borrow().as_ref() {
            return order.clone();
        }
        let order = distinct_speaker_order(self.shown_lines());
        *self.speaker_order_cache.borrow_mut() = Some(order.clone());
        order
    }

    /// Line indices sorted by start time (untimed lines last), stable.
    fn display_order(&self) -> Vec<usize> {
        if let Some(order) = self.display_order_cache.borrow().as_ref() {
            return order.clone();
        }
        let lines = self.shown_lines();
        let mut order = sorted_display_order(lines);
        if let Some(speaker) = &self.speaker_filter {
            order.retain(|index| {
                lines
                    .get(*index)
                    .is_some_and(|line| line.speaker == *speaker)
            });
        }
        *self.display_order_cache.borrow_mut() = Some(order.clone());
        order
    }

    /// Scroll the transcript so the given line (by index) is roughly centred.
    /// Pixel scroll offset that centres the given line in the transcript.
    fn follow_target(&self, line: usize) -> Option<f32> {
        let order = self.display_order();
        let total = order.len().max(1) as f32;
        let content = self.transcript_content;
        let viewport = self.transcript_viewport;
        if content <= 0.0 || viewport <= 0.0 {
            return None;
        }
        let rank = order.iter().position(|index| *index == line)?;
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
                scrollable::scroll_to(transcript_id(), scrollable::AbsoluteOffset { x: 0.0, y })
            }
            None => Task::none(),
        }
    }

    /// Move the playhead to the line matching the scroll position after a user
    /// scroll. Reaches the first line at the top and the last line at the
    /// bottom instead of always centring.
    fn seek_from_scroll(&mut self, offset: f32, viewport: f32) {
        let order = self.display_order();
        let total = order.len();
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

    /// Friendly placeholder shown when there is no text to render yet.
    fn empty_transcript_state(&self, scheme: &Scheme) -> Element<'_, Message> {
        let running = matches!(self.status, Status::Running);
        let selected = self.selected_view.is_some();
        let has_queue = !self.queue.is_empty();
        let has_done = self.has_results();
        let pending = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending));

        let (glyph, tint, title, body) = if running {
            let detail = if self.stage.is_empty() || self.stage == "starting" {
                "Preparing the audio and models".to_string()
            } else {
                capitalize_first(&self.stage)
            };
            (
                ICON_HOURGLASS,
                scheme.warn_tint,
                "Listening for speech",
                format!("{detail}. Lines appear here as they are transcribed on this device."),
            )
        } else if self
            .selected
            .and_then(|index| self.queue.get(index))
            .is_some_and(|item| matches!(item.status, ItemStatus::Pending))
        {
            (
                ICON_PLAY,
                scheme.track,
                "Not transcribed yet",
                "Run the queue to create a transcript for this file.".to_string(),
            )
        } else if selected {
            (
                ICON_FILE_TEXT,
                scheme.track,
                "Empty transcript",
                "This transcript has no lines to display.".to_string(),
            )
        } else if !has_queue {
            (
                ICON_MIC,
                scheme.warn_tint,
                "Drop audio. It stays here.",
                "Drag audio files anywhere into the window, or add them from the left panel. Nothing is uploaded."
                    .to_string(),
            )
        } else if has_done {
            (
                ICON_FILE_TEXT,
                scheme.track,
                "Choose a transcript",
                "Select a completed file in the queue to read it here.".to_string(),
            )
        } else if pending {
            (
                ICON_PLAY,
                scheme.track,
                "Ready to transcribe",
                "Press Transcribe (or Ctrl+Enter). Processing happens on this device.".to_string(),
            )
        } else {
            (
                ICON_CIRCLE,
                scheme.track,
                "Nothing here yet",
                "Add audio files to the queue to get started.".to_string(),
            )
        };

        let mut actions = row![].spacing(8).align_y(iced::Alignment::Center);
        if !has_queue {
            actions = actions.push(
                action_text("Add File(s)")
                    .on_press(Message::AddFiles)
                    .style(color_button(scheme.blue, scheme.on_accent)),
            );
            actions = actions.push(
                action_text("Add Folder")
                    .on_press(Message::AddFolder)
                    .style(pill_button_secondary),
            );
        } else if pending && !has_done {
            actions = actions.push(
                action_text("Transcribe")
                    .on_press_maybe(self.models_ready().then_some(Message::Start))
                    .style(color_button(scheme.green, scheme.on_accent)),
            );
            actions = actions.push(
                action_text("Add File(s)")
                    .on_press(Message::AddFiles)
                    .style(pill_button_secondary),
            );
        }

        let icon = container(icon_colored(glyph, 26.0, scheme.blue))
            .width(Length::Fixed(56.0))
            .height(Length::Fixed(56.0))
            .center_x(Length::Fixed(56.0))
            .center_y(Length::Fixed(56.0))
            .style(move |_theme| container::Style {
                background: Some(tint.into()),
                border: iced::border::rounded(28.0),
                ..container::Style::default()
            });

        let mut content = column![
            icon,
            text(title).size(22).font(Font {
                weight: font::Weight::Bold,
                ..Font::DEFAULT
            }),
            text(body).size(14).color(scheme.muted).width(Length::Fill),
        ]
        .spacing(10)
        .align_x(iced::Alignment::Center);

        if !has_queue || (pending && !has_done) {
            content = content.push(container(actions).padding([8, 0]));
        }

        container(container(content).center_x(Length::Fill).max_width(380.0))
            .center(Length::Fill)
            .padding(24)
            .into()
    }

    /// Short summary shown beside the queue title, e.g. "3 files · 12:34".
    fn queue_summary_text(&self) -> String {
        let count = self.queue.len();
        if count == 0 {
            return "No files yet".to_string();
        }
        let known: f64 = self.queue.iter().filter_map(|item| item.duration).sum();
        let known_count = self
            .queue
            .iter()
            .filter(|item| item.duration.is_some())
            .count();
        let files = if count == 1 {
            "1 file".to_string()
        } else {
            format!("{count} files")
        };
        if known_count == 0 {
            files
        } else if known_count == count {
            format!("{files} \u{00b7} {}", format_secs(known))
        } else {
            format!("{files} \u{00b7} {} known", format_secs(known))
        }
    }

    /// The queue card in the left panel: header, add/import toolbar, list and
    /// run controls.
    fn queue_panel(&self, scheme: &Scheme, width: f32) -> Element<'_, Message> {
        let scheme = *scheme;
        let running = matches!(self.status, Status::Running);
        let has_queue = !self.queue.is_empty();
        let can_start = !running && has_queue && self.models_ready();

        let header = row![
            text("Queue")
                .size(theme::TEXT_MD)
                .wrapping(NO_WRAP)
                .font(Font {
                    weight: font::Weight::Bold,
                    ..Font::DEFAULT
                }),
            text(ellipsize(&self.queue_summary_text(), 26))
                .size(theme::TEXT_XS)
                .wrapping(NO_WRAP)
                .color(scheme.muted),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let add_button = self.tip(
            action(
                row![
                    icon_colored(ICON_PLUS, 14.0, scheme.on_accent),
                    text("Add").size(12).wrapping(NO_WRAP),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center),
            )
            .on_press(Message::AddFiles)
            .style(color_button(scheme.blue, scheme.on_accent)),
            "Add audio files",
            &scheme,
        );

        let folder_button = self.tip(
            action(icon(ICON_FOLDER_PLUS, 15.0))
                .on_press(Message::AddFolder)
                .style(pill_button_secondary),
            "Add a folder",
            &scheme,
        );

        let import_picker: Element<Message> =
            pick_list(ImportKind::ALL, None::<ImportKind>, Message::ImportChoice)
                .placeholder("Import")
                .style(ink_pick(scheme.ink))
                .handle(caret_handle(false))
                .width(Length::Fixed(126.0))
                .into();

        let clear_button = self.tip(
            action(icon(ICON_TRASH, 15.0))
                .on_press_maybe((!running && has_queue).then_some(Message::ClearQueue))
                .style(pill_button_secondary),
            "Clear queue",
            &scheme,
        );

        let toolbar = row![
            add_button,
            folder_button,
            import_picker,
            horizontal_space(),
            clear_button,
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center);

        let body: Element<Message> = if has_queue {
            let mut rows = column![].spacing(4).width(Length::Fill);
            for index in 0..self.queue.len() {
                rows = rows.push(self.queue_row(index, &scheme));
            }
            scrollable(rows)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(queue_scrollbar_style)
                .into()
        } else {
            container(
                column![
                    icon_colored(ICON_MIC, 20.0, scheme.muted),
                    text("No audio queued")
                        .size(theme::TEXT_SM)
                        .color(scheme.text),
                    text("Add files or drop them here")
                        .size(theme::TEXT_XS)
                        .color(scheme.muted),
                ]
                .spacing(6)
                .align_x(iced::Alignment::Center),
            )
            .center(Length::Fill)
            .into()
        };

        let output = self
            .output_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Audio source directory".to_string());
        let output_short = ellipsize_middle(&output, 20);
        let output_row = row![
            action_text("Save Location")
                .on_press_maybe((!running && has_queue).then_some(Message::PickOutput))
                .style(pill_button_secondary),
            text(output_short)
                .size(theme::TEXT_XS)
                .wrapping(NO_WRAP)
                .color(scheme.muted)
                .width(Length::Fill),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let preset: Element<Message> = if !running && has_queue {
            pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged)
                .style(pill_pick)
                .width(Length::Fill)
                .into()
        } else {
            // Keep the real pick_list (so iced draws its arrow) but put a
            // click-capturing layer on top so it cannot be opened.
            stack![
                pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged)
                    .style(greyed_pick(scheme))
                    .width(Length::Fill),
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

        let speaker_pick = pick_list(
            SpeakerOpt::ALL,
            Some(SpeakerOpt::from_value(self.num_speakers)),
            Message::SpeakerCountChanged,
        );
        let speakers: Element<Message> = if running {
            stack![
                speaker_pick.style(greyed_pick(scheme)).width(Length::Fill),
                mouse_area(
                    container(horizontal_space())
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .interaction(mouse::Interaction::Idle)
                .on_press(Message::Tick),
            ]
            .into()
        } else if !self.can(license::Feature::Diarization) {
            // Free runs ASR only, so the speaker count has no effect.
            stack![
                speaker_pick.style(greyed_pick(scheme)).width(Length::Fill),
                mouse_area(
                    container(horizontal_space())
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::OpenPaywall),
            ]
            .into()
        } else {
            speaker_pick.style(pill_pick).width(Length::Fill).into()
        };

        let option_label =
            |label: &'static str| text(label).size(11).wrapping(NO_WRAP).color(scheme.muted);
        let options = row![
            column![option_label("Preset"), preset]
                .spacing(2)
                .width(Length::Fill),
            column![option_label("Speakers"), speakers]
                .spacing(2)
                .width(Length::Fill),
        ]
        .spacing(8);

        let controls = column![
            output_row,
            options,
            row![
                action_text("Transcribe on this device")
                    .on_press_maybe(can_start.then_some(Message::Start))
                    .style(color_button(scheme.green, scheme.on_accent)),
                action_text("Cancel")
                    .on_press_maybe(running.then_some(Message::Cancel))
                    .style(color_button(scheme.danger, scheme.on_accent)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        ]
        .spacing(12);

        let content = column![header, toolbar, body, horizontal_rule(1), controls,]
            .spacing(12)
            .height(Length::Fill);

        container(content)
            .padding(12)
            .width(Length::Fixed(width))
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(scheme.panel_bg.into()),
                border: iced::border::rounded(16.0),
                shadow: soft_shadow(),
                ..container::Style::default()
            })
            .into()
    }

    /// One queue row: status, two-line label, optional progress and remove.
    fn queue_row(&self, index: usize, scheme: &Scheme) -> Element<'static, Message> {
        let item = &self.queue[index];
        let scheme = *scheme;
        let running = matches!(self.status, Status::Running);
        let can_remove = !running;
        let viewing = self.selected == Some(index) && self.selected_view.is_some();
        let selectable = matches!(item.status, ItemStatus::Running)
            || (!running && matches!(item.status, ItemStatus::Pending))
            || (matches!(item.status, ItemStatus::Done) && item.transcript.is_some());

        let name = item
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.path.display().to_string());
        let name = ellipsize_middle(&name, 16);
        let duration = item
            .duration
            .map(format_secs)
            .unwrap_or_else(|| "--:--".to_string());

        let (meta, meta_color) = match &item.status {
            ItemStatus::Pending => (format!("{duration} \u{00b7} Queued"), scheme.muted),
            ItemStatus::Running => (
                format!(
                    "{duration} \u{00b7} Transcribing {:.0}%",
                    self.progress_shown * 100.0
                ),
                scheme.muted,
            ),
            ItemStatus::Done => (format!("{duration} \u{00b7} Done"), scheme.muted),
            ItemStatus::Failed(message) => (
                format!("{duration} \u{00b7} {}", ellipsize(message, 22)),
                scheme.danger,
            ),
        };

        let badge: Element<'static, Message> = match &item.status {
            ItemStatus::Pending => queue_status_icon(ICON_CIRCLE, 18.0, scheme.text),
            ItemStatus::Running => {
                queue_status_badge(ICON_HOURGLASS, 14.0, scheme.track, scheme.text)
            }
            ItemStatus::Done => {
                queue_status_badge(ICON_CHECK, 18.0, scheme.green, scheme.on_accent)
            }
            ItemStatus::Failed(_) => {
                queue_status_badge(ICON_X, 15.0, scheme.danger, scheme.on_accent)
            }
        };

        let label = column![
            text(name)
                .size(theme::TEXT_SM)
                .wrapping(NO_WRAP)
                .color(scheme.text),
            text(meta).size(11).wrapping(NO_WRAP).color(meta_color),
        ]
        .spacing(1)
        .width(Length::Fill);

        let mut content = row![badge, label]
            .spacing(8)
            .align_y(iced::Alignment::Center);

        if matches!(item.status, ItemStatus::Running) {
            content = content.push(
                progress_bar(0.0..=1.0, self.progress_shown)
                    .width(Length::Fixed(52.0))
                    .height(Length::Fixed(6.0))
                    .style(move |_theme| progress_bar::Style {
                        background: scheme.bar_track.into(),
                        bar: scheme.progress.into(),
                        border: iced::border::rounded(3.0),
                    }),
            );
        }

        if matches!(item.status, ItemStatus::Pending) {
            content = content.push(
                self.tip(
                    action(icon(ICON_ARROW_UP, 11.0))
                        .on_press_maybe(
                            (!running && index > 0).then_some(Message::MoveItemUp(index)),
                        )
                        .style(subtle_icon_button),
                    "Move up",
                    &scheme,
                ),
            );
            content = content.push(
                self.tip(
                    action(icon(ICON_ARROW_DOWN, 11.0))
                        .on_press_maybe(
                            (!running && index + 1 < self.queue.len())
                                .then_some(Message::MoveItemDown(index)),
                        )
                        .style(subtle_icon_button),
                    "Move down",
                    &scheme,
                ),
            );
        }

        if matches!(item.status, ItemStatus::Failed(_)) {
            content = content.push(
                self.tip(
                    action(icon(ICON_RETRY, 12.0))
                        .on_press_maybe((!running).then_some(Message::RetryItem(index)))
                        .style(subtle_icon_button),
                    "Retry",
                    &scheme,
                ),
            );
        }

        content = content.push(
            self.tip(
                action(icon(ICON_X, 11.0))
                    .on_press_maybe(can_remove.then_some(Message::RemoveItem(index)))
                    .style(subtle_icon_button),
                "Remove from queue",
                &scheme,
            ),
        );

        let row_tint = Color {
            a: 0.12,
            ..scheme.blue
        };
        let hover_tint = scheme.bar_track;
        let row_text = scheme.text;
        let row_button =
            button(content)
                .padding([6, 8])
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let background = if viewing {
                        row_tint
                    } else if selectable && matches!(status, button::Status::Hovered) {
                        hover_tint
                    } else {
                        Color::TRANSPARENT
                    };
                    let border_color = if viewing {
                        scheme.blue
                    } else {
                        Color::TRANSPARENT
                    };
                    button::Style {
                        background: Some(background.into()),
                        text_color: row_text,
                        border: iced::border::Border {
                            color: border_color,
                            width: 1.0,
                            radius: 10.0.into(),
                        },
                        shadow: Shadow::default(),
                    }
                });
        if selectable {
            row_button.on_press(Message::ViewItem(index)).into()
        } else {
            row_button.into()
        }
    }

    /// Transcript view: per-line (search tinted) or SMS bubbles.
    fn transcript_element(&self) -> Element<'_, Message> {
        let scheme = self.scheme();
        if self.queue.is_empty() {
            return self.empty_transcript_state(&scheme);
        }
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
                        action_text("Transcribe again")
                            .on_press(Message::RequeueItem(index))
                            .style(color_button(scheme.green, scheme.on_accent)),
                    );
                }
            }
            return col.into();
        }
        if self.shown_transcript().trim().is_empty() && self.shown_lines().is_empty() {
            return self.empty_transcript_state(&scheme);
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
        let single_speaker = speaker_order.len() <= 1;
        let display: Vec<usize> = self.display_order();
        let playhead_line = self.active_line_index();
        let lines = self.shown_lines();
        // Each speaker keeps a fixed side of the transcript in bubble mode.
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
                    .unwrap_or(if single_speaker { 1 } else { 0 });
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
                let stamp = if self.show_timestamps {
                    line.start
                        .map(|seconds| format_timestamp(seconds as f64))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let content = line_text(line);
                col = col.push(stack![
                    sms_row(
                        &content,
                        &line.speaker,
                        &stamp,
                        left,
                        single_speaker,
                        speaker_palette(index, &scheme),
                        scheme.ink,
                        tint,
                        &query,
                        false,
                        false,
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
                ]);
            } else {
                let accent = speaker_order
                    .iter()
                    .position(|name| *name == line.speaker)
                    .map(|index| speaker_palette(index, &scheme))
                    .unwrap_or_else(|| {
                        speaker_palette(if single_speaker { 1 } else { 0 }, &scheme)
                    });
                let search_tint = matched.then_some(if Some(idx) == active_line {
                    scheme.search_active
                } else {
                    scheme.search_hl
                });
                col = col.push(line_element(
                    line,
                    idx,
                    accent,
                    search_tint,
                    &query,
                    false,
                    false,
                    scheme.search_hl,
                    &scheme,
                    size,
                    active,
                    self.show_timestamps,
                ));
            }
        }

        scrollable(col)
            .id(transcript_id())
            .on_scroll(Message::TranscriptScrolled)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(scrollbar_style)
            .into()
    }

    /// Header above the transcript: what is on screen plus reading controls.
    fn transcript_header(&self, scheme: &Scheme) -> Option<Element<'_, Message>> {
        let scheme = *scheme;
        if self.queue.is_empty() {
            return None;
        }
        let has_lines = !self.shown_lines().is_empty();
        let viewing = self.selected_view.is_some();
        let running = matches!(self.status, Status::Running);
        if !has_lines && !viewing && !running {
            return None;
        }

        let (glyph, tint, title, meta) = if let Some(view) = &self.selected_view {
            let name = view
                .audio
                .as_ref()
                .or(Some(&view.source))
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("transcript");
            let mut meta = Vec::new();
            if let Some(duration) = view.duration.filter(|duration| *duration > 0.0) {
                meta.push(format_secs(duration as f64));
            }
            if view.output.is_none()
                && self.view_error.is_none()
                && self
                    .selected
                    .and_then(|index| self.queue.get(index))
                    .is_some_and(|item| matches!(item.status, ItemStatus::Done))
            {
                meta.push("read-only".to_string());
            }
            (
                ICON_FILE_TEXT,
                scheme.blue,
                ellipsize_middle(name, 28),
                meta.join(" \u{00b7} "),
            )
        } else if running {
            (
                ICON_MIC,
                scheme.green,
                self.current_file_name()
                    .unwrap_or_else(|| "Live transcript".to_string()),
                "Transcribing now".to_string(),
            )
        } else {
            (
                ICON_FILE_TEXT,
                scheme.blue,
                "Transcript".to_string(),
                String::new(),
            )
        };

        let badge = container(icon_colored(glyph, 13.0, tint))
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(26.0))
            .center_x(Length::Fixed(26.0))
            .center_y(Length::Fixed(26.0))
            .style(move |_theme| container::Style {
                background: Some(Color { a: 0.16, ..tint }.into()),
                border: iced::border::rounded(13.0),
                ..container::Style::default()
            });
        let mut label = column![text(title).size(13).wrapping(NO_WRAP)].spacing(1);
        if !meta.is_empty() {
            label = label.push(text(meta).size(11).wrapping(NO_WRAP).color(scheme.muted));
        }

        let controls: Element<Message> = if has_lines {
            self.transcript_controls(&scheme)
        } else {
            horizontal_space().into()
        };

        let header = container(
            row![
                row![badge, label]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                horizontal_space(),
                controls,
            ]
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding([2, 8]);
        let divider = container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_theme| container::Style {
                background: Some(scheme.border.into()),
                ..container::Style::default()
            });
        Some(column![header, divider].spacing(4).into())
    }

    /// Zoom / Follow / SMS controls in the transcript header.
    fn transcript_controls(&self, scheme: &Scheme) -> Element<'_, Message> {
        let follow: Element<Message> = if self.follow_playhead {
            row![icon(ICON_CHECK, 12.0), text("Follow").wrapping(NO_WRAP)]
                .spacing(4)
                .align_y(iced::Alignment::Center)
                .into()
        } else {
            text("Follow").wrapping(NO_WRAP).into()
        };
        let sms: Element<Message> = if self.sms_mode {
            row![icon(ICON_CHECK, 12.0), text("SMS").wrapping(NO_WRAP)]
                .spacing(4)
                .align_y(iced::Alignment::Center)
                .into()
        } else {
            text("SMS").wrapping(NO_WRAP).into()
        };
        let timestamps: Element<Message> = if self.show_timestamps {
            row![icon(ICON_CHECK, 12.0), text("Time").wrapping(NO_WRAP)]
                .spacing(4)
                .align_y(iced::Alignment::Center)
                .into()
        } else {
            text("Time").wrapping(NO_WRAP).into()
        };
        let follow_style: fn(&iced::Theme, button::Status) -> button::Style =
            if self.follow_playhead {
                pill_button_active
            } else {
                pill_button_secondary
            };
        let sms_style: fn(&iced::Theme, button::Status) -> button::Style = if self.sms_mode {
            pill_button_active
        } else {
            pill_button_secondary
        };
        let time_style: fn(&iced::Theme, button::Status) -> button::Style = if self.show_timestamps
        {
            pill_button_active
        } else {
            pill_button_secondary
        };
        let follow_tip = if self.follow_playhead {
            "Stop keeping the active line in view"
        } else {
            "Keep the active line in view"
        };
        let sms_tip = if self.sms_mode {
            "Show transcripts as lines"
        } else {
            "Show the transcript as chat bubbles"
        };
        row![
            self.tip(
                action(icon(ICON_ZOOM_OUT, 14.0))
                    .on_press(Message::ZoomOut)
                    .style(pill_button_secondary),
                "Zoom text out",
                scheme,
            ),
            self.tip(
                action(icon(ICON_ZOOM_IN, 14.0))
                    .on_press(Message::ZoomIn)
                    .style(pill_button_secondary),
                "Zoom text in",
                scheme,
            ),
            self.tip(
                action(follow)
                    .on_press(Message::ToggleFollowPlayhead)
                    .style(follow_style),
                follow_tip,
                scheme,
            ),
            self.tip(
                action(sms)
                    .on_press(Message::SmsToggled(!self.sms_mode))
                    .style(sms_style),
                sms_tip,
                scheme,
            ),
            self.tip(
                action(timestamps)
                    .on_press(Message::TimestampsToggled(!self.show_timestamps))
                    .style(time_style),
                if self.show_timestamps {
                    "Hide timestamps"
                } else {
                    "Show timestamps"
                },
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
        let lines = self.shown_lines();
        for index in self.display_order() {
            if let Some(line) = lines.get(index) {
                if let Some(start) = line.start {
                    if start <= t && best.map_or(true, |(_, best_start)| start >= best_start) {
                        best = Some((index, start));
                    }
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

    /// Colour swatches + names for the speakers on the timeline. Chunks into
    /// rows so a large cast does not overflow the panel.
    fn timeline_legend(&self, scheme: &Scheme) -> Element<'_, Message> {
        let order = self.speaker_order();
        let mut rows = column![].spacing(6);
        for (chunk_index, chunk) in order.chunks(4).enumerate() {
            let mut row = row![].spacing(12).align_y(iced::Alignment::Center);
            for (offset, name) in chunk.iter().enumerate() {
                let color = speaker_palette(chunk_index * 4 + offset, scheme);
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
                        text(label).size(12).wrapping(NO_WRAP),
                    ]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
                );
            }
            rows = rows.push(row);
        }
        rows.into()
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
        let playing = self
            .player
            .as_ref()
            .is_some_and(|player| player.is_playing());
        let glyph = if ready { scheme.ink } else { scheme.muted };

        let play_icon = if playing { ICON_PAUSE } else { ICON_PLAY };
        let play_button: Element<Message> = if ready {
            action(icon_colored(play_icon, 15.0, scheme.on_accent))
                .on_press(Message::PlayerToggle)
                .style(color_button(scheme.blue, scheme.on_accent))
                .padding([8, 10])
                .into()
        } else {
            action(icon_colored(ICON_PLAY, 15.0, glyph))
                .style(greyed_button(*scheme))
                .padding([8, 10])
                .into()
        };
        let skip_back: Element<Message> = self.tip(
            action(icon_colored(ICON_REWIND, 13.0, glyph))
                .on_press_maybe(ready.then_some(Message::SeekBy(-5.0)))
                .style(pill_button_secondary),
            "Back 5 seconds",
            scheme,
        );
        let skip_forward: Element<Message> = self.tip(
            action(icon_colored(ICON_FORWARD, 13.0, glyph))
                .on_press_maybe(ready.then_some(Message::SeekBy(5.0)))
                .style(pill_button_secondary),
            "Forward 5 seconds",
            scheme,
        );

        let time_color = if ready { scheme.ink } else { scheme.muted };
        let total = if duration > 0.0 {
            format_secs(duration as f64)
        } else {
            "--:--".to_string()
        };
        let time = text(format!("{} / {}", format_secs(self.playhead as f64), total))
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
                    .style(greyed_pick(*scheme))
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

        row![skip_back, play_button, skip_forward, time, speed]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
    }

    /// Find controls, always shown on the player row.
    fn search_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let count = self.matching_lines().len();
        let (status, status_color) = if self.search_query.trim().is_empty() {
            (String::new(), scheme.muted)
        } else if count == 0 {
            ("no matches".to_string(), scheme.danger)
        } else {
            (
                format!("{} / {}", self.search_index + 1, count),
                scheme.muted,
            )
        };
        let query_input = text_input("Find", &self.search_query)
            .id(find_id())
            .on_input(Message::SearchChanged)
            .width(Length::Fixed(90.0))
            .style(pill_input);
        let prev = action(icon(ICON_ARROW_UP, 14.0))
            .on_press(Message::FindPrev)
            .padding([4, 6])
            .style(pill_button_secondary);
        let next = action(icon(ICON_ARROW_DOWN, 14.0))
            .on_press(Message::FindNext)
            .padding([4, 6])
            .style(pill_button_secondary);
        let mut controls = row![
            query_input,
            text(status).size(11).wrapping(NO_WRAP).color(status_color),
            prev,
            next,
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        let speaker_order = self.speaker_order();
        if speaker_order.len() > 1 {
            let mut speakers = vec!["All speakers".to_string()];
            speakers.extend(speaker_order);
            let selected = self
                .speaker_filter
                .clone()
                .unwrap_or_else(|| "All speakers".to_string());
            controls = controls.push(
                pick_list(speakers, Some(selected), Message::SpeakerFilterChanged)
                    .style(pill_pick)
                    .width(Length::Fixed(110.0)),
            );
        }
        controls.into()
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
        let border_color = scheme.border;
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

    /// First-run / missing-model notice with an inline download action.
    fn model_notice(&self, scheme: &Scheme) -> Option<Element<'_, Message>> {
        let scheme = *scheme;
        if self.models_ready() && self.model_download.is_none() {
            return None;
        }

        let missing = download::missing_for(self.preset.arg(), self.pipelines_diarize())
            .iter()
            .map(|model| friendly_model(model.label))
            .collect::<Vec<_>>()
            .join(", ");
        let downloading = self.model_download.is_some();

        let (title, body) = if let Some(error) = &self.model_error {
            (
                "Could not download models".to_string(),
                format!("{error}. Check your connection and try again."),
            )
        } else if downloading {
            ("Downloading models".to_string(), String::new())
        } else if missing.is_empty() {
            (
                "Transcription models are missing".to_string(),
                "Download the model files once to enable offline transcription. Audio never leaves this device."
                    .to_string(),
            )
        } else {
            (
                "Transcription models are missing".to_string(),
                format!(
                    "Missing: {missing}. One-time download, about 650 MB. Audio never leaves this device."
                ),
            )
        };

        let details: Element<Message> = if let Some(state) = &self.model_download {
            let transfer = match state.total {
                Some(total) if total > 0 => {
                    format!("{} of {}", format_bytes(state.done), format_bytes(total))
                }
                _ => format!("{} downloaded", format_bytes(state.done)),
            };
            column![
                text(format!("Downloading {}", state.label))
                    .size(14)
                    .font(Font {
                        weight: font::Weight::Bold,
                        ..Font::DEFAULT
                    }),
                text(transfer).size(12).color(scheme.muted),
            ]
            .spacing(2)
            .into()
        } else {
            column![
                text(title).size(14).font(Font {
                    weight: font::Weight::Bold,
                    ..Font::DEFAULT
                }),
                text(body).size(12).color(scheme.muted),
            ]
            .spacing(2)
            .into()
        };

        let details = container(details).width(Length::Fill);

        let icon = container(icon_colored(ICON_DOWNLOAD, 16.0, scheme.ink))
            .width(Length::Fixed(32.0))
            .height(Length::Fixed(32.0))
            .center_x(Length::Fixed(32.0))
            .center_y(Length::Fixed(32.0))
            .style(move |_theme| container::Style {
                background: Some(scheme.warn_tint.into()),
                border: iced::border::rounded(16.0),
                ..container::Style::default()
            });

        let mut header = row![icon, details]
            .spacing(10)
            .align_y(iced::Alignment::Center);

        if let Some(state) = &self.model_download {
            let percent = state
                .total
                .filter(|total| *total > 0)
                .map(|total| format!("{:.0}%", state.done as f32 / total as f32 * 100.0))
                .unwrap_or_default();
            header = header.push(text(percent).size(13)).push(
                action_text("Cancel")
                    .on_press(Message::CancelModelDownload)
                    .style(pill_button_secondary),
            );
        } else {
            let label = if self.model_error.is_some() {
                "Retry download"
            } else {
                "Download models"
            };
            header = header.push(
                action(label)
                    .on_press(Message::DownloadModels)
                    .style(color_button(scheme.blue, scheme.on_accent)),
            );
        }

        let mut content = column![header].spacing(8);
        if let Some(state) = &self.model_download {
            let fraction = state
                .total
                .filter(|total| *total > 0)
                .map(|total| (state.done as f32 / total as f32).clamp(0.0, 1.0))
                .unwrap_or(0.0);
            content = content.push(
                progress_bar(0.0..=1.0, fraction)
                    .width(Length::Fill)
                    .height(Length::Fixed(8.0))
                    .style(move |_theme| progress_bar::Style {
                        background: scheme.bar_track.into(),
                        bar: scheme.progress.into(),
                        border: iced::border::rounded(4.0),
                    }),
            );
        }

        Some(
            container(content)
                .width(Length::Fill)
                .padding(12)
                .style(move |_theme| container::Style {
                    background: Some(scheme.dialog_bg.into()),
                    border: iced::border::Border {
                        color: scheme.border,
                        width: 1.0,
                        radius: 12.0.into(),
                    },
                    shadow: soft_shadow(),
                    ..container::Style::default()
                })
                .into(),
        )
    }

    fn settings_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let running = matches!(self.status, Status::Running);

        let theme = settings_control(
            pick_list(AppTheme::ALL, Some(self.theme_mode), Message::SetTheme)
                .style(ink_pick(scheme.ink))
                .handle(caret_handle(false))
                .width(Length::Fixed(180.0)),
        );

        let font_on = self.font_scale;
        let font_controls = settings_control(
            row![
                action(icon(ICON_ZOOM_OUT, 14.0))
                    .on_press_maybe((font_on > 0.6).then_some(Message::ZoomOut))
                    .style(pill_button_secondary),
                container(text(format!("{:.0}%", font_on * 100.0)).size(theme::TEXT_XS))
                    .width(Length::Fixed(44.0))
                    .center_x(Length::Fixed(44.0)),
                action(icon(ICON_ZOOM_IN, 14.0))
                    .on_press_maybe((font_on < 2.5).then_some(Message::ZoomIn))
                    .style(pill_button_secondary),
                action_text("Reset")
                    .on_press(Message::ZoomReset)
                    .style(pill_button_secondary),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );

        let preset: Element<Message> = if running {
            text("Locked while transcribing")
                .wrapping(NO_WRAP)
                .size(theme::TEXT_XS)
                .color(scheme.muted)
                .into()
        } else {
            pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged)
                .style(pill_pick)
                .handle(caret_handle(false))
                .width(Length::Fixed(180.0))
                .into()
        };
        let speakers: Element<Message> = if running {
            text("Locked while transcribing")
                .wrapping(NO_WRAP)
                .size(theme::TEXT_XS)
                .color(scheme.muted)
                .into()
        } else if !self.can(license::Feature::Diarization) {
            stack![
                pick_list(
                    SpeakerOpt::ALL,
                    Some(SpeakerOpt::from_value(self.num_speakers)),
                    Message::SpeakerCountChanged,
                )
                .style(greyed_pick(scheme))
                .handle(caret_handle(false))
                .width(Length::Fixed(180.0)),
                mouse_area(
                    container(horizontal_space())
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::OpenPaywall),
            ]
            .into()
        } else {
            pick_list(
                SpeakerOpt::ALL,
                Some(SpeakerOpt::from_value(self.num_speakers)),
                Message::SpeakerCountChanged,
            )
            .style(pill_pick)
            .handle(caret_handle(false))
            .width(Length::Fixed(180.0))
            .into()
        };

        let language: Element<Message> = if running {
            text("Locked while transcribing")
                .wrapping(NO_WRAP)
                .size(theme::TEXT_XS)
                .color(scheme.muted)
                .into()
        } else {
            pick_list(
                LanguageOpt::ALL,
                Some(LanguageOpt::from_code(self.language.as_deref())),
                Message::LanguageChanged,
            )
            .style(pill_pick)
            .handle(caret_handle(false))
            .width(Length::Fixed(180.0))
            .into()
        };

        let speed = settings_control(
            pick_list(
                Speed::ALL,
                Some(Speed::from_factor(self.playback_speed)),
                Message::SpeedChanged,
            )
            .style(speed_pick)
            .handle(caret_handle(false))
            .width(Length::Fixed(180.0)),
        );

        let follow_on = self.follow_playhead;
        let follow = settings_control(
            action(text(if follow_on { "On" } else { "Off" }).size(theme::TEXT_XS))
                .on_press(Message::ToggleFollowPlayhead)
                .style(move |theme, status| {
                    if follow_on {
                        pill_button_active(theme, status)
                    } else {
                        pill_button_secondary(theme, status)
                    }
                })
                .width(Length::Fixed(64.0)),
        );

        let timestamps_on = self.show_timestamps;
        let timestamps = settings_control(
            action(text(if timestamps_on { "On" } else { "Off" }).size(theme::TEXT_XS))
                .on_press(Message::TimestampsToggled(!timestamps_on))
                .style(move |theme, status| {
                    if timestamps_on {
                        pill_button_active(theme, status)
                    } else {
                        pill_button_secondary(theme, status)
                    }
                })
                .width(Length::Fixed(64.0)),
        );
        let notifications_on = self.notifications;
        let notifications = settings_control(
            action(text(if notifications_on { "On" } else { "Off" }).size(theme::TEXT_XS))
                .on_press(Message::NotificationsToggled(!notifications_on))
                .style(move |theme, status| {
                    if notifications_on {
                        pill_button_active(theme, status)
                    } else {
                        pill_button_secondary(theme, status)
                    }
                })
                .width(Length::Fixed(64.0)),
        );

        let location = self
            .output_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Audio source directory".to_string());
        let change = action_text("Change")
            .on_press_maybe((!running).then_some(Message::PickOutput))
            .style(pill_button_secondary);
        let use_audio_folder = action_text("Use audio folder")
            .on_press_maybe(
                (!running && self.output_dir.is_some()).then_some(Message::ResetOutputDir),
            )
            .style(pill_button_secondary);
        let save_controls = settings_control(
            row![change, use_audio_folder]
                .spacing(6)
                .align_y(iced::Alignment::Center),
        );
        let save_row = row![
            column![
                text("Save location")
                    .size(theme::TEXT_SM)
                    .wrapping(NO_WRAP)
                    .color(scheme.text),
                text(ellipsize_middle(&location, 46))
                    .size(11)
                    .wrapping(NO_WRAP)
                    .color(scheme.muted),
            ]
            .spacing(1)
            .width(Length::Fill),
            save_controls,
        ]
        .spacing(theme::SPACE_LG)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

        let models_dir = models::models_dir();
        let models_location = ellipsize_middle(&models_dir.display().to_string(), 40);
        let present_models = self.models.iter().filter(|model| model.present()).count();
        let model_bytes: u64 = self
            .models
            .iter()
            .filter(|model| model.present())
            .map(|model| model.bytes)
            .sum();
        let models_status = if self.model_download.is_some() {
            "Downloading missing files".to_string()
        } else if self.models_ready() {
            format!(
                "All {} files \u{00b7} {}",
                self.models.len(),
                format_bytes(model_bytes)
            )
        } else {
            format!(
                "{present_models} of {} files \u{00b7} {} downloaded",
                self.models.len(),
                format_bytes(model_bytes)
            )
        };
        let open_models = action(
            row![icon(ICON_FOLDER_OPEN, 12.0), text("Open folder").size(11),]
                .spacing(4)
                .align_y(iced::Alignment::Center),
        )
        .on_press(Message::OpenModelsFolder)
        .style(pill_button_secondary);
        let download_models = action_text("Download missing")
            .on_press_maybe(
                (!self.models_ready() && self.model_download.is_none())
                    .then_some(Message::DownloadModels),
            )
            .style(pill_button_secondary);
        let clear_partial = action_text("Clear partial")
            .on_press_maybe((self.model_download.is_none()).then_some(Message::ClearPartialModels))
            .style(pill_button_secondary);
        let models_row = column![
            column![
                text("Model storage")
                    .size(theme::TEXT_SM)
                    .color(scheme.text),
                text(models_location).size(11).color(scheme.muted),
                text(models_status).size(11).color(scheme.muted),
            ]
            .spacing(1)
            .width(Length::Fill),
            row![open_models, download_models, clear_partial]
                .spacing(6)
                .align_y(iced::Alignment::Center),
        ]
        .spacing(theme::SPACE_SM)
        .width(Length::Fill);

        let privacy_processing = settings_control(
            text("On this device")
                .size(theme::TEXT_XS)
                .color(scheme.green),
        );
        let privacy_cloud = settings_control(text("None").size(theme::TEXT_XS).color(scheme.text));
        let privacy_telemetry =
            settings_control(text("None").size(theme::TEXT_XS).color(scheme.text));
        let privacy_network = settings_control(
            text("Model downloads only")
                .size(theme::TEXT_XS)
                .color(scheme.text),
        );
        let privacy_storage =
            settings_control(text("Local files").size(theme::TEXT_XS).color(scheme.text));

        let mut body = column![].spacing(theme::SPACE_SM).width(Length::Fill);
        body = body.push(settings_section_label("Privacy", scheme));
        body = body.push(settings_row("Processing", privacy_processing, scheme));
        body = body.push(settings_row("Cloud & accounts", privacy_cloud, scheme));
        body = body.push(settings_row("Telemetry", privacy_telemetry, scheme));
        body = body.push(settings_row("Network", privacy_network, scheme));
        body = body.push(settings_row("Audio & transcripts", privacy_storage, scheme));
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Appearance", scheme));
        body = body.push(settings_row("Theme", theme, scheme));
        body = body.push(settings_row("Transcript text size", font_controls, scheme));
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Transcription", scheme));
        body = body.push(settings_row("Preset", preset, scheme));
        body = body.push(settings_row("Speakers", speakers, scheme));
        body = body.push(settings_row("Language", language, scheme));
        body = body.push(settings_row("Show timestamps", timestamps, scheme));
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Playback", scheme));
        body = body.push(settings_row("Speed", speed, scheme));
        body = body.push(settings_row("Follow playhead", follow, scheme));
        body = body.push(settings_row(
            "Completion notifications",
            notifications,
            scheme,
        ));
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Storage", scheme));
        body = body.push(save_row);
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Models", scheme));
        body = body.push(models_row);

        let height = (self.window_size.height * 0.46).clamp(300.0, 360.0);
        let scroll = scrollable(body)
            .width(Length::Fill)
            .height(Length::Fixed(height));

        let title = row![
            column![
                text("Settings").size(theme::TEXT_LG),
                text("Appearance and defaults")
                    .size(theme::TEXT_XS)
                    .color(scheme.muted),
            ]
            .spacing(1),
            horizontal_space(),
            action(icon(ICON_X, 14.0))
                .on_press(Message::CloseSettings)
                .style(subtle_icon_button),
        ]
        .align_y(iced::Alignment::Center);

        let footer = row![
            text("Changes save automatically")
                .size(11)
                .color(scheme.muted),
            horizontal_space(),
            action_text("Done")
                .on_press(Message::CloseSettings)
                .style(color_button(scheme.blue, scheme.on_accent)),
        ]
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

        modal_card(
            column![title, scroll, footer].spacing(theme::SPACE_LG),
            480.0,
            scheme,
        )
    }

    /// Draw the toast above whatever the view built (including modals).
    fn with_toast<'a>(&self, base: Element<'a, Message>, scheme: Scheme) -> Element<'a, Message> {
        let Some((message, _)) = &self.toast else {
            return base;
        };
        let toast = mouse_area(
            container(text(message.clone()).size(13))
                .padding([8, 14])
                .style(move |_theme| container::Style {
                    background: Some(scheme.toast_bg.into()),
                    text_color: Some(scheme.toast_text),
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
        stack![base, overlay].into()
    }

    pub fn view(&self) -> Element<'_, Message> {
        let scheme = self.scheme();
        let running = matches!(self.status, Status::Running);
        let has_queue = !self.queue.is_empty();
        let global_progress = self.overall_shown;

        let clock = row![
            icon_colored(ICON_CLOCK, 14.0, scheme.text),
            text(self.timer_text())
                .size(12)
                .width(Length::Shrink)
                .style(move |_theme| text::Style {
                    color: Some(scheme.text),
                }),
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
                dots.push(text::Span::new(".".repeat(5 - active)).color(Color::TRANSPARENT));
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

        let bar_style = move |_theme: &Theme| progress_bar::Style {
            background: linear_gradient(
                theme::darken(scheme.bar_track, 0.12),
                theme::lighten(scheme.bar_track, 0.05),
            ),
            bar: linear_gradient(
                theme::lighten(scheme.progress, 0.28),
                theme::darken(scheme.progress, 0.06),
            ),
            border: iced::border::Border {
                color: theme::darken(scheme.bar_track, 0.20),
                width: 1.0,
                radius: 15.0.into(),
            },
        };
        let progress_stack = stack![
            progress_bar(0.0..=1.0, global_progress)
                .width(Length::Fill)
                .height(Length::Fixed(30.0))
                .style(bar_style),
            container(label_el)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        ]
        .width(Length::Fill);
        let progress_shell = container(progress_stack)
            .width(Length::Fill)
            .style(move |_theme| container::Style {
                border: iced::border::Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: 15.0.into(),
                },
                shadow: Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.20),
                    offset: Vector::new(0.0, 3.0),
                    blur_radius: 10.0,
                },
                ..container::Style::default()
            });

        let status_row = row![clock, progress_shell]
            .spacing(4)
            .align_y(iced::Alignment::Center);

        let mut middle = column![status_row].spacing(8).width(Length::Fill);
        middle = middle.push(self.timeline_element(&scheme));
        middle = middle.push(
            row![
                self.player_element(&scheme),
                horizontal_space(),
                action_text("Summary")
                    .on_press_maybe(
                        (!running && self.has_results()).then_some(Message::OpenSummary)
                    )
                    .style(pill_button_secondary),
                self.tip(
                    action_text("Rename")
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
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill),
        );

        let middle = container(middle)
            .width(Length::Fill)
            .padding(12)
            .style(move |_theme| container::Style {
                background: Some(scheme.panel_bg.into()),
                border: iced::border::rounded(16.0),
                shadow: soft_shadow(),
                ..container::Style::default()
            });

        let mut transcript_box = column![]
            .spacing(6)
            .width(Length::Fill)
            .height(Length::Fill);
        if let Some(header) = self.transcript_header(&scheme) {
            transcript_box = transcript_box.push(header);
        }
        transcript_box = transcript_box.push(self.transcript_element());

        let left_panel: Element<Message> = self.queue_panel(&scheme, LEFT_PANEL_WIDTH);

        let actions_row = row![
            action_text("Copy")
                .on_press_maybe(
                    (!self.shown_transcript().is_empty()).then_some(Message::CopyTranscript)
                )
                .style(pill_button_secondary),
            action_text("Save")
                .on_press_maybe(
                    (self.shown_output().is_some() && !self.shown_transcript().is_empty())
                        .then_some(Message::SaveTranscript)
                )
                .style(pill_button_secondary),
            action_text("Reveal")
                .on_press_maybe(
                    (self.shown_output().is_some()
                        || self.shown_audio().is_some()
                        || self.output_dir.is_some())
                    .then_some(Message::RevealOutput)
                )
                .style(pill_button_secondary),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);

        let search_actions_row: Element<Message> = row![
            self.search_element(&scheme),
            horizontal_space(),
            actions_row
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .into();

        let mut right = column![]
            .spacing(12)
            .width(Length::Fill)
            .height(Length::Fill);
        if let Some(notice) = self.model_notice(&scheme) {
            right = right.push(notice);
        }
        // In the empty state the progress/timeline/player chrome has nothing to
        // act on; let the welcome card own the panel instead.
        let show_middle = has_queue || self.selected_view.is_some() || running;
        if show_middle {
            right = right.push(middle);
        }
        right = right.push(
            container(transcript_box)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(move |_theme| container::Style {
                    background: Some(scheme.panel_bg.into()),
                    border: iced::border::rounded(16.0),
                    shadow: soft_shadow(),
                    ..container::Style::default()
                }),
        );
        if has_queue {
            right = right.push(search_actions_row);
        }
        let right_panel: Element<Message> = right.into();

        let (theme_icon, theme_tip) = match self.theme_mode {
            AppTheme::Light => (ICON_MOON, "Switch to dark theme"),
            AppTheme::Dark => (ICON_DISPLAY, "Switch to system theme"),
            AppTheme::System => (ICON_SUN, "Switch to light theme"),
        };

        let privacy_badge = container(
            row![
                icon_colored(ICON_SHIELD, 12.0, scheme.green),
                text("Offline \u{00b7} On this device")
                    .size(theme::TEXT_XS)
                    .color(scheme.muted),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        )
        .padding([4, 8])
        .style(move |_theme| container::Style {
            background: Some(scheme.track.into()),
            border: iced::border::rounded(999.0),
            ..container::Style::default()
        });

        let header = row![
            iced::widget::image(logo_handle())
                .width(Length::Fixed(32.0))
                .height(Length::Fixed(32.0)),
            column![
                text("Hippo Transcribe").size(theme::TEXT_XL).font(Font {
                    weight: font::Weight::Bold,
                    ..Font::DEFAULT
                }),
                text("Private, offline transcription & diarization")
                    .size(theme::TEXT_XS)
                    .color(scheme.muted),
            ]
            .spacing(theme::SPACE_2XS),
            privacy_badge,
            horizontal_space(),
            self.license_button(&scheme),
            self.tip(
                action(icon(theme_icon, 16.0))
                    .on_press(Message::ToggleTheme)
                    .style(pill_button_secondary),
                theme_tip,
                &scheme,
            ),
            self.tip(
                action(icon(ICON_GEAR, 16.0))
                    .on_press(Message::OpenSettings)
                    .style(pill_button_secondary),
                "Settings (Ctrl+,)",
                &scheme,
            ),
            pick_list(HelpKind::ALL, None::<HelpKind>, Message::HelpChoice)
                .placeholder("Help")
                .style(pill_pick)
                .handle(caret_handle(true))
                .width(Length::Fixed(92.0)),
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center);

        let mut base = column![
            header,
            row![left_panel, right_panel]
                .spacing(PANEL_GAP)
                .height(Length::Fill),
        ]
        .spacing(8)
        .padding(OUTER_PADDING);

        if self.console_open {
            base = base.push(self.console_panel(&scheme));
        }

        let mut base: Element<Message> = base.into();
        if self.drag_hover {
            base = drop_overlay(base, scheme);
        }

        if self.delete_confirm {
            if let Some(edit) = &self.line_edit {
                let preview = edit.content.text().trim_end().replace(['\n', '\r'], " ");
                let preview = ellipsize(preview.trim(), 72);
                let speaker = edit.speaker.trim();
                let preview = if speaker.is_empty() {
                    preview
                } else {
                    format!("{speaker}: {preview}")
                };
                let body = column![
                    text("Delete line?").size(20),
                    text(
                        "This removes the line from the transcript and saves the file. \
                         A backup is kept next to it."
                    )
                    .size(theme::TEXT_SM)
                    .color(scheme.muted),
                    container(
                        text(preview)
                            .size(theme::TEXT_SM)
                            .color(scheme.text)
                            .width(Length::Fill),
                    )
                    .width(Length::Fill)
                    .padding(10)
                    .style(move |_theme| container::Style {
                        background: Some(scheme.track.into()),
                        border: iced::border::rounded(8.0),
                        ..container::Style::default()
                    }),
                    row![
                        horizontal_space(),
                        action_text("Cancel")
                            .on_press(Message::CancelDeleteLine)
                            .style(pill_button_secondary),
                        action_text("Delete")
                            .on_press(Message::ConfirmDeleteLine)
                            .style(color_button(scheme.danger, scheme.on_accent)),
                    ]
                    .spacing(8),
                ]
                .spacing(10);
                let dialog = modal_card(body, 440.0, scheme);
                return self.with_toast(modal_overlay(base, dialog, scheme), scheme);
            }
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
                row![
                    action_text("Merge next")
                        .on_press(Message::MergeNextLine)
                        .style(pill_button_secondary),
                    action_text("Split at playhead")
                        .on_press(Message::SplitLineAtPlayhead)
                        .style(pill_button_secondary),
                ]
                .spacing(8),
            );
            col = col.push(
                container(
                    row![
                        action_text("Delete")
                            .on_press(Message::DeleteLineEdit)
                            .style(color_button(scheme.danger, scheme.on_accent)),
                        action_text("Copy line")
                            .on_press(Message::CopyLineEdit)
                            .style(pill_button_secondary),
                        horizontal_space(),
                        action_text("Save")
                            .on_press(Message::SaveLineEdit)
                            .style(color_button(scheme.blue, scheme.on_accent)),
                        action_text("Cancel")
                            .on_press(Message::CancelLineEdit)
                            .style(pill_button_secondary),
                    ]
                    .spacing(8),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            );
            let dialog = modal_card(col, 560.0, scheme);
            return self.with_toast(modal_overlay(base, dialog, scheme), scheme);
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
                        body =
                            body.push(container(text(line.clone())).width(Length::Fill).padding(
                                iced::Padding {
                                    top: 0.0,
                                    right: 0.0,
                                    bottom: 0.0,
                                    left: BODY_INDENT,
                                },
                            ));
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
                text("Saved locally \u{00b7} Nothing uploaded.")
                    .size(theme::TEXT_XS)
                    .color(scheme.muted),
                container(
                    row![action_text("Close")
                        .on_press(Message::CloseSummary)
                        .style(color_button(scheme.blue, scheme.on_accent))]
                    .spacing(8),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            ]
            .spacing(8);

            let dialog = modal_card(col, 380.0, scheme);
            return self.with_toast(modal_overlay(base, dialog, scheme), scheme);
        }

        if self.paywall_open {
            let dialog = self.paywall_dialog(&scheme);
            return self.with_toast(modal_overlay(base, dialog, scheme), scheme);
        }

        if self.settings_open {
            let dialog = self.settings_dialog(&scheme);
            return self.with_toast(modal_overlay(base, dialog, scheme), scheme);
        }

        if let Some(kind) = self.help_open {
            let mut col = column![text(kind.label()).size(20)].spacing(8);
            match kind {
                HelpKind::Shortcuts => {
                    // Key in a fixed-width column so the descriptions line up.
                    const KEY_WIDTH: f32 = 150.0;
                    for (key, description) in [
                        ("Ctrl+O", "Add files"),
                        ("Ctrl+,", "Settings"),
                        ("Ctrl+F", "Find"),
                        ("Ctrl+Enter", "Transcribe"),
                        ("Ctrl + / - / 0", "Transcript font zoom"),
                        ("Space", "Play / pause"),
                        ("Left / Right", "Seek \u{00b1}5s"),
                        ("Up / Down", "Move through transcript"),
                        ("Alt+Enter", "Edit active line"),
                        ("Esc", "Dismiss"),
                    ] {
                        col = col.push(
                            row![text(key).width(Length::Fixed(KEY_WIDTH)), text(description),]
                                .spacing(12),
                        );
                    }
                }
                HelpKind::About => {
                    col = col.push(
                        column![
                            text(format!("Hippo Transcribe {}", env!("CARGO_PKG_VERSION")))
                                .size(theme::TEXT_MD)
                                .font(Font {
                                    weight: font::Weight::Medium,
                                    ..Font::DEFAULT
                                }),
                            text("Private, offline speaker diarization + transcription.")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                            text("Hippo is named for HIPAA. Built so sensitive recordings never leave this machine.")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                            text("Diarization: pyannote segmentation + wespeaker (sherpa-onnx).")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                            text("Transcription: whisper.cpp.")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                            text("Runs fully offline; models download once.")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                        ]
                        .spacing(4),
                    );
                    col = col.push(settings_divider(scheme));
                    col = col.push(
                        column![
                            settings_section_label("Privacy", scheme),
                            text("No cloud. No account. No telemetry.")
                                .size(theme::TEXT_SM)
                                .color(scheme.text),
                            text("Audio and transcripts stay on this device.")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                        ]
                        .spacing(4),
                    );
                    col = col.push(settings_divider(scheme));
                    col = col.push(
                        column![
                            settings_section_label("License", scheme),
                            text("Free software, licensed under GPLv3.").size(theme::TEXT_MD),
                            text("Copyright (C) 2026 Sean Wu.")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                            // TODO: add "Source: <repository URL>" once published.
                            text("Provided without warranty; see the license for details.")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                            settings_section_label("Models", scheme),
                            text("pyannote (MIT), wespeaker (CC BY 4.0), Whisper (MIT).")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                        ]
                        .spacing(4),
                    );
                }
                HelpKind::Console => {}
            }
            col = col.push(
                container(
                    row![action_text("Close")
                        .on_press(Message::CloseHelp)
                        .style(color_button(scheme.blue, scheme.on_accent))]
                    .spacing(8),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            );
            let dialog = modal_card(col, 460.0, scheme);
            return self.with_toast(modal_overlay(base, dialog, scheme), scheme);
        }

        if !self.rename_open {
            return self.with_toast(base, scheme);
        }

        let mut col = column![text("Rename Speakers").size(20)].spacing(8);
        if self.rename_fields.is_empty() {
            col = col.push(text("No speakers to rename."));
        }
        for (index, (original, value)) in self.rename_fields.iter().enumerate() {
            col = col.push(
                row![
                    text(original.clone())
                        .wrapping(NO_WRAP)
                        .width(Length::Fixed(120.0)),
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
                    action_text("Save")
                        .on_press(Message::RenameSave)
                        .style(color_button(scheme.blue, scheme.on_accent)),
                    action_text("Cancel")
                        .on_press(Message::RenameCancel)
                        .style(pill_button_secondary),
                ]
                .spacing(8),
            )
            .width(Length::Fill)
            .align_x(iced::Alignment::End),
        );
        let dialog = modal_card(col, 380.0, scheme);
        self.with_toast(modal_overlay(base, dialog, scheme), scheme)
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

        // Flat inset track.
        frame.fill_rectangle(Point::ORIGIN, Size::new(width, height), self.track);

        let band_height = (height * 0.16).clamp(6.0, 12.0);
        let wave_area = height - band_height - 4.0;
        let mid = wave_area / 2.0;

        let count = self.peaks.len();
        if count > 0 {
            let peak_max = self.peaks.iter().copied().fold(0.0_f32, f32::max).max(1e-6);
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
                let top = mid - amplitude;
                let bar_height = amplitude * 2.0;
                frame.fill_rectangle(Point::new(x, top), Size::new(bar, bar_height), self.wave);
            }
        }

        // A faint zero line gives the mirrored bars a shared axis.
        frame.fill_rectangle(
            Point::new(0.0, mid - 0.5),
            Size::new(width, 1.0),
            Color {
                a: 0.25,
                ..theme::lighten(self.wave, 0.65)
            },
        );

        if self.duration > 0.0 {
            let band_y = height - band_height;
            frame.fill_rectangle(
                Point::new(0.0, (band_y - 1.0).max(0.0)),
                Size::new(width, 1.0),
                Color {
                    a: 0.35,
                    ..theme::darken(self.track, 0.40)
                },
            );
            for turn in self.turns.iter() {
                let x0 = self.x_of(turn.start / self.duration, width);
                let x1 = self.x_of(turn.end / self.duration, width);
                if x1 < 0.0 || x0 > width {
                    continue;
                }
                let left = x0.max(0.0);
                let right = x1.min(width);
                let turn_width = (right - left).max(1.0);
                frame.fill_rectangle(
                    Point::new(left, band_y),
                    Size::new(turn_width, band_height),
                    turn.color,
                );
            }
            let x = self.x_of((self.playhead / self.duration).clamp(0.0, 1.0), width);
            frame.fill_rectangle(
                Point::new((x - 1.0).max(0.0), 0.0),
                Size::new(2.0, height),
                self.playhead_color,
            );
            frame.fill_rectangle(
                Point::new((x - 5.0).max(0.0), 1.0),
                Size::new(10.0, 3.0),
                self.playhead_color,
            );
        }

        vec![frame.into_geometry()]
    }
}

fn speaker_palette(index: usize, scheme: &Scheme) -> Color {
    scheme.speaker_colors[index % scheme.speaker_colors.len()]
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

/// Bundled brand mark, decoded once and shared with the image widget.
fn logo_handle() -> iced::widget::image::Handle {
    static LOGO: OnceLock<iced::widget::image::Handle> = OnceLock::new();
    LOGO.get_or_init(|| {
        let image =
            image::load_from_memory(include_bytes!("../../icons/hippo-transcribe-logo-256.png"))
                .expect("bundled Hippo Transcribe logo")
                .into_rgba8();
        let (width, height) = image.dimensions();
        iced::widget::image::Handle::from_rgba(width, height, image.into_raw())
    })
    .clone()
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
const ICON_REWIND: char = '\u{f818}';
const ICON_FORWARD: char = '\u{f7f3}';
const ICON_ARROW_UP: char = '\u{f148}';
const ICON_ARROW_DOWN: char = '\u{f128}';
const ICON_CIRCLE: char = '\u{f28a}';
const ICON_HOURGLASS: char = '\u{f421}';
const ICON_CHECK: char = '\u{f633}';
const ICON_X: char = '\u{f659}';
const ICON_CARET_DOWN: char = '\u{f229}';
const ICON_CARET_UP: char = '\u{f235}';
const ICON_ZOOM_IN: char = '\u{f62c}';
const ICON_ZOOM_OUT: char = '\u{f62d}';
const ICON_CLOCK: char = '\u{f293}';
const ICON_MOON: char = '\u{f494}';
const ICON_SUN: char = '\u{f5a1}';
const ICON_GEAR: char = '\u{f3e2}';
const ICON_MIC: char = '\u{f490}';
const ICON_FILE_TEXT: char = '\u{f38b}';
const ICON_DOWNLOAD: char = '\u{f30a}';
const ICON_PLUS: char = '\u{f4fe}';
const ICON_FOLDER_PLUS: char = '\u{f3d3}';
const ICON_FOLDER_OPEN: char = '\u{f3d8}';
const ICON_TRASH: char = '\u{f5de}';
const ICON_RETRY: char = '\u{f116}';
const ICON_SHIELD: char = '\u{f52f}';
const ICON_DISPLAY: char = '\u{f302}';

/// Full-window drop target shown while a file hovers over the app.
fn drop_overlay<'a>(base: impl Into<Element<'a, Message>>, scheme: Scheme) -> Element<'a, Message> {
    let card = container(
        column![
            icon_colored(ICON_DOWNLOAD, 32.0, scheme.blue),
            text("Drop to add audio").size(22).font(Font {
                weight: font::Weight::Bold,
                ..Font::DEFAULT
            }),
            text("MP3, WAV, M4A, FLAC, and more")
                .size(13)
                .color(scheme.muted),
        ]
        .spacing(8)
        .align_x(iced::Alignment::Center),
    )
    .padding(36)
    .style(move |_theme| container::Style {
        background: Some(scheme.dialog_bg.into()),
        border: iced::border::Border {
            color: scheme.blue,
            width: 2.0,
            radius: 16.0.into(),
        },
        shadow: dialog_shadow(),
        ..container::Style::default()
    });

    let overlay = container(card)
        .center(Length::Fill)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(24)
        .style(move |_theme| container::Style {
            background: Some(scheme.scrim.into()),
            ..container::Style::default()
        });

    stack![base.into(), overlay].into()
}

/// Standard dialog surface shared by every modal.
fn modal_card<'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    scheme: Scheme,
) -> Element<'a, Message> {
    container(content)
        .width(Length::Fixed(width))
        .padding(16)
        .style(move |_theme| container::Style {
            background: Some(scheme.dialog_bg.into()),
            border: iced::border::rounded(16.0),
            shadow: dialog_shadow(),
            ..container::Style::default()
        })
        .into()
}

fn settings_row<'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
    scheme: Scheme,
) -> Element<'a, Message> {
    row![
        text(label)
            .size(theme::TEXT_SM)
            .wrapping(NO_WRAP)
            .color(scheme.text)
            .width(Length::Fill),
        control.into(),
    ]
    .spacing(theme::SPACE_LG)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill)
    .into()
}

fn settings_control<'a>(control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(control)
        .width(Length::Fixed(230.0))
        .align_x(iced::Alignment::End)
        .into()
}

fn settings_section_label<'a>(label: &'a str, scheme: Scheme) -> Element<'a, Message> {
    text(label)
        .size(11)
        .wrapping(NO_WRAP)
        .font(Font {
            weight: font::Weight::Bold,
            ..Font::DEFAULT
        })
        .color(scheme.muted)
        .into()
}

fn settings_divider<'a>(scheme: Scheme) -> Element<'a, Message> {
    container(horizontal_space())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(move |_theme| container::Style {
            background: Some(scheme.border.into()),
            ..container::Style::default()
        })
        .into()
}

/// Layer `dialog` over `base` behind a dimming backdrop that swallows clicks,
/// so a visible modal blocks everything outside it.
fn modal_overlay<'a>(
    base: impl Into<Element<'a, Message>>,
    dialog: impl Into<Element<'a, Message>>,
    scheme: Scheme,
) -> Element<'a, Message> {
    let base = base.into();
    let dialog = dialog.into();
    let backdrop = mouse_area(
        container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(scheme.scrim.into()),
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

fn queue_weight(item: &QueueItem, fallback: f32) -> f32 {
    item.duration
        .map(|duration| duration as f32)
        .filter(|duration| *duration > 0.0)
        .unwrap_or(fallback)
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
    let ink = theme.palette().text;
    style.vertical_rail.scroller.color = if visible {
        Color { a: 0.5, ..ink }
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
    let ink = theme.palette().text;
    style.vertical_rail.scroller.color = if active {
        Color { a: 0.55, ..ink }
    } else {
        Color { a: 0.28, ..ink }
    };
    style
}

/// A speaker label filled with the same colour as its timeline swatch.
fn speaker_chip(label: &str, accent: Color, ink: Color, size: f32) -> Element<'static, Message> {
    container(
        text(label.to_string())
            .size(size)
            .font(Font {
                weight: font::Weight::Bold,
                ..Font::default()
            })
            .color(ink)
            .wrapping(NO_WRAP),
    )
    .padding([1, 5])
    .style(move |_theme| container::Style {
        background: Some(accent.into()),
        border: iced::border::rounded(4.0),
        ..container::Style::default()
    })
    .into()
}

fn sms_row(
    content: &str,
    speaker: &str,
    stamp: &str,
    left: bool,
    single_speaker: bool,
    color: Color,
    ink: Color,
    tint: Option<Color>,
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
    highlight: Color,
    size: f32,
    active: bool,
) -> Element<'static, Message> {
    let background = tint.unwrap_or(color);
    let border = if active {
        iced::border::Border {
            color: ink,
            width: 2.0,
            radius: 10.0.into(),
        }
    } else {
        iced::border::Border {
            color: Color { a: 0.45, ..color },
            width: 1.0,
            radius: 10.0.into(),
        }
    };
    let mut body = column![].spacing(2);
    if !speaker.is_empty() || !stamp.is_empty() {
        let mut meta = row![].spacing(6).align_y(iced::Alignment::Center);
        if !speaker.is_empty() {
            meta = meta.push(speaker_chip(speaker, color, ink, size * 0.72));
        }
        if !stamp.is_empty() {
            meta = meta.push(
                text(stamp.to_string())
                    .size(size * 0.72)
                    .color(Color { a: 0.70, ..ink }),
            );
        }
        body = body.push(meta);
    }
    body = body.push(rich_text(styled_spans(
        content,
        query,
        case_sensitive,
        whole_word,
        Some(ink),
        highlight,
        size,
    )));
    let bubble = container(body)
        .width(Length::Shrink)
        .max_width(Pixels((size * 28.0).clamp(240.0, 620.0)))
        .align_x(iced::Alignment::Start)
        .padding([6, 9])
        .style(move |_theme| container::Style {
            background: Some(background.into()),
            border,
            ..container::Style::default()
        });
    if single_speaker {
        bubble.into()
    } else if left {
        row![bubble, horizontal_space()].spacing(8).into()
    } else {
        row![horizontal_space(), bubble].spacing(8).into()
    }
}

/// A transcript line with a speaker-coloured accent stripe, an optional
/// timestamp, a bold speaker label, and clickable select/edit behaviour.
#[allow(clippy::too_many_arguments)]
fn line_element(
    line: &DocLine,
    idx: usize,
    accent: Color,
    search_tint: Option<Color>,
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
    highlight: Color,
    scheme: &Scheme,
    size: f32,
    active: bool,
    show_timestamps: bool,
) -> Element<'static, Message> {
    let ink = scheme.ink;

    let speaker = line.speaker.clone();
    let rest = line_text(line);
    let stamp = if show_timestamps {
        line.start
            .map(|s| format_timestamp(s as f64))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let mut content = row![]
        .spacing(6)
        .align_y(iced::Alignment::Start)
        .width(Length::Fill);

    if !stamp.is_empty() {
        content = content.push(text(stamp).size(size * 0.78).color(scheme.muted));
    }

    if !speaker.is_empty() {
        content = content.push(speaker_chip(&speaker, accent, ink, size));
    }

    content = content.push(rich_text(styled_spans(
        &rest,
        query,
        case_sensitive,
        whole_word,
        Some(ink),
        highlight,
        size,
    )));
    content = content.push(horizontal_space());

    let background = search_tint.unwrap_or(accent);
    let border = if active {
        iced::border::Border {
            color: ink,
            width: 2.0,
            radius: 6.0.into(),
        }
    } else {
        iced::border::Border {
            color: Color { a: 0.45, ..accent },
            width: 1.0,
            radius: 6.0.into(),
        }
    };

    let keyboard_hit = button(
        container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(0)
    .style(|_theme, _status| button::Style {
        background: None,
        border: iced::border::Border::default(),
        ..button::Style::default()
    })
    .on_press(Message::SelectLine(idx));

    let line: Element<'static, Message> = stack![
        container(content)
            .width(Length::Fill)
            .padding([3, 6])
            .style(move |_theme| container::Style {
                background: Some(background.into()),
                border,
                ..container::Style::default()
            }),
        mouse_area(
            container(horizontal_space())
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .interaction(mouse::Interaction::Pointer)
        .on_right_press(Message::EditLine(idx)),
        keyboard_hit,
    ]
    .into();
    let tip_bg = scheme.tip_bg;
    let tip_text = scheme.tip_text;
    let tip_border = scheme.border;
    tooltip(
        line,
        container(text("Right-click to edit").size(12))
            .padding(6)
            .style(move |_theme| container::Style {
                background: Some(tip_bg.into()),
                text_color: Some(tip_text),
                border: iced::border::Border {
                    color: tip_border,
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

const PILL: f32 = 1000.0;

/// A button (content-sized / minimum width).
fn action<'a>(content: impl Into<Element<'a, Message>>) -> iced::widget::Button<'a, Message> {
    button(content)
}

/// A button with a single-line text label.
fn action_text<'a>(label: &'a str) -> iced::widget::Button<'a, Message> {
    action(text(label).wrapping(NO_WRAP))
}

/// A subtle drop shadow that gives surfaces a soft, raised look.
fn soft_shadow() -> Shadow {
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.22),
        offset: Vector::new(0.0, 3.0),
        blur_radius: 9.0,
    }
}

/// A top-to-bottom gradient background.
fn linear_gradient(top: Color, bottom: Color) -> iced::Background {
    iced::Background::Gradient(iced::Gradient::Linear(
        iced::gradient::Linear::new(iced::Degrees(180.0))
            .add_stop(0.0, top)
            .add_stop(1.0, bottom),
    ))
}

fn style_background_color(style: &button::Style) -> Option<Color> {
    match &style.background {
        Some(iced::Background::Color(color)) => Some(*color),
        _ => None,
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
    let base = button::secondary(theme, status);
    let color = style_background_color(&base).unwrap_or(theme.palette().background);
    let (top, bottom, shadow) = match status {
        button::Status::Hovered => (
            theme::lighten(color, 0.16),
            theme::lighten(color, 0.02),
            soft_shadow(),
        ),
        button::Status::Pressed => (
            theme::darken(color, 0.12),
            theme::darken(color, 0.02),
            Shadow::default(),
        ),
        button::Status::Disabled => (color, color, Shadow::default()),
        button::Status::Active => (
            theme::lighten(color, 0.07),
            theme::darken(color, 0.05),
            soft_shadow(),
        ),
    };
    button::Style {
        background: Some(linear_gradient(top, bottom)),
        text_color: base.text_color,
        border: iced::border::Border {
            color: theme::lighten(color, 0.28),
            width: 1.0,
            radius: PILL.into(),
        },
        shadow,
    }
}

/// A secondary button tinted with the accent colour to show a toggled-on state.
fn pill_button_active(theme: &iced::Theme, status: button::Status) -> button::Style {
    let primary = theme.palette().primary;
    let (alpha, border_alpha, shadow): (f32, f32, Shadow) = match status {
        button::Status::Hovered => (0.30, 0.65, soft_shadow()),
        button::Status::Pressed => (0.38, 0.70, Shadow::default()),
        button::Status::Disabled => (0.12, 0.30, Shadow::default()),
        button::Status::Active => (0.22, 0.55, soft_shadow()),
    };
    button::Style {
        background: Some(linear_gradient(
            Color {
                a: (alpha + 0.10).min(0.95),
                ..primary
            },
            Color {
                a: alpha,
                ..primary
            },
        )),
        text_color: theme.palette().text,
        border: iced::border::Border {
            color: Color {
                a: border_alpha,
                ..primary
            },
            width: 1.0,
            radius: PILL.into(),
        },
        shadow,
    }
}

/// A quiet, borderless icon button for row-level actions.
fn subtle_icon_button(theme: &iced::Theme, status: button::Status) -> button::Style {
    let text = theme.palette().text;
    let (background, text_color) = match status {
        button::Status::Hovered => (Color { a: 0.10, ..text }, text),
        button::Status::Pressed => (Color { a: 0.16, ..text }, text),
        button::Status::Disabled => (Color::TRANSPARENT, Color { a: 0.30, ..text }),
        button::Status::Active => (Color::TRANSPARENT, Color { a: 0.65, ..text }),
    };
    button::Style {
        background: Some(background.into()),
        text_color,
        border: iced::border::rounded(6.0),
        shadow: Shadow::default(),
    }
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
        background: base.background.unwrap_or_else(|| Color::TRANSPARENT.into()),
        border: iced::border::Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: PILL.into(),
        },
    }
}

fn greyed_pick(scheme: Scheme) -> impl Fn(&iced::Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let mut style = pick_list::default(theme, status);
        style.text_color = scheme.muted;
        style.handle_color = scheme.muted;
        style.background = scheme.track.into();
        style.border = iced::border::Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: PILL.into(),
        };
        style
    }
}

fn greyed_button(scheme: Scheme) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = button::secondary(theme, status);
        style.background = Some(
            Color {
                a: 0.12,
                ..scheme.muted
            }
            .into(),
        );
        style.text_color = scheme.muted;
        style.border.radius = PILL.into();
        style
    }
}

fn color_button(bg: Color, ink: Color) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let (top, bottom, text, shadow) = match status {
            button::Status::Hovered => (
                theme::lighten(bg, 0.18),
                theme::lighten(bg, 0.02),
                ink,
                soft_shadow(),
            ),
            button::Status::Pressed => (
                theme::darken(bg, 0.16),
                theme::darken(bg, 0.04),
                ink,
                Shadow::default(),
            ),
            // Filled buttons keep their hue when disabled, but the label must
            // follow the active theme rather than the (possibly dark) accent ink.
            button::Status::Disabled => {
                let faded = Color { a: 0.45, ..bg };
                (
                    faded,
                    faded,
                    Color {
                        a: 0.85,
                        ..theme.palette().text
                    },
                    Shadow::default(),
                )
            }
            _ => (
                theme::lighten(bg, 0.12),
                theme::darken(bg, 0.04),
                ink,
                soft_shadow(),
            ),
        };
        button::Style {
            background: Some(linear_gradient(top, bottom)),
            text_color: text,
            border: iced::border::Border {
                color: Color {
                    a: 0.45,
                    ..theme::lighten(bg, 0.55)
                },
                width: 1.0,
                radius: PILL.into(),
            },
            shadow,
        }
    }
}

/// Byte ranges in `raw` matching `query`, honouring case and word options.
fn search_match_ranges(
    raw: &str,
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> Vec<(usize, usize)> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };

    // Match against a folded copy, but map every match back to original byte
    // offsets. Lowercasing can change a character's byte length, so raw offsets
    // into the folded string are not safe to slice with.
    let mut folded = String::with_capacity(raw.len());
    let mut folded_to_original: Vec<(usize, usize)> = Vec::with_capacity(raw.len() + 1);
    for (index, ch) in raw.char_indices() {
        folded_to_original.push((folded.len(), index));
        if case_sensitive {
            folded.push(ch);
        } else {
            folded.extend(ch.to_lowercase());
        }
    }
    folded_to_original.push((folded.len(), raw.len()));

    let mut matches: Vec<(usize, usize)> = Vec::new();
    let mut from = 0usize;
    while from <= folded.len() {
        let Some(position) = folded[from..].find(needle.as_str()) else {
            break;
        };
        let start = from + position;
        let end = start + needle.len();
        let original_start = folded_to_original
            .iter()
            .rev()
            .find(|(folded_index, _)| *folded_index <= start)
            .map(|(_, original)| *original)
            .unwrap_or(0);
        let original_end = folded_to_original
            .iter()
            .find(|(folded_index, _)| *folded_index >= end)
            .map(|(_, original)| *original)
            .unwrap_or(raw.len());
        let is_word = |ch: Option<char>| ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
        let boundary_ok = !whole_word
            || (!is_word(raw[..original_start].chars().next_back())
                && !is_word(raw[original_end..].chars().next()));
        if original_start < original_end && boundary_ok {
            matches.push((original_start, original_end));
        }
        from = end.max(start + 1);
        if from >= folded.len() {
            break;
        }
    }
    matches
}

fn styled_spans(
    raw: &str,
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
    color: Option<Color>,
    highlight: Color,
    size: f32,
) -> Vec<text::Span<'static, Message>> {
    let bold = Font {
        weight: font::Weight::Bold,
        ..Font::default()
    };
    let head_len = raw.find(": ").map(|index| index + 2).unwrap_or(0);
    let matches = search_match_ranges(raw, query, case_sensitive, whole_word);

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
            "hippo-transcribe-app-test-{tag}-{}",
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
        app.batch_paths = vec![audio.clone()];
        app.save_queue();

        let (loaded, batch) = App::load_queue_and_batch();
        assert_eq!(loaded.len(), 1);
        assert_eq!(batch, vec![audio.clone()]);
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
    fn line_text_drops_the_prefix_once() {
        let lines = parse_lines("[00:00] SPEAKER 1: hello there\n[00:05] plain text\n");
        assert_eq!(line_text(&lines[0]), "hello there");
        assert_eq!(line_text(&lines[1]), "plain text");
    }

    #[test]
    fn transcript_renders_line_and_bubble_modes_with_search() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("render-config");
        let media = temp_dir("render-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello there\n[00:05] SPEAKER 2: hi\n[00:07] SPEAKER 1: hello again\n",
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
            peaks: Arc::new(vec![0.2; 16]),
        });
        let _ = app.view_item(0);
        assert!(app.transcript_header(&app.scheme()).is_some());

        app.search_query = "hello".to_string();
        app.font_scale = 1.3;
        let _ = app.view();
        assert_eq!(app.matching_lines().len(), 2);

        app.sms_mode = true;
        let _ = app.view();
        app.theme_mode = AppTheme::Dark;
        let _ = app.view();

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn search_options_honour_case_and_word_boundaries() {
        let raw = "[00:00] SPEAKER 1: Hello hello hellos";
        assert_eq!(search_match_ranges(raw, "hello", false, false).len(), 3);
        assert_eq!(search_match_ranges(raw, "hello", false, true).len(), 2);
        assert_eq!(search_match_ranges(raw, "Hello", true, false).len(), 1);
        assert_eq!(search_match_ranges(raw, "hello", true, false).len(), 2);
        assert!(search_match_ranges(raw, "world", false, false).is_empty());
    }

    #[test]
    fn lines_can_be_merged_and_split_from_the_editor() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("merge-config");
        let media = temp_dir("merge-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello world\n[00:05] SPEAKER 1: again\n",
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
            peaks: Arc::new(vec![0.2; 16]),
        });
        let _ = app.view_item(0);
        app.line_edit = Some(LineEdit {
            index: 0,
            speaker: "SPEAKER 1".to_string(),
            content: text_editor::Content::with_text("hello world"),
        });
        let _ = app.update(Message::MergeNextLine);
        assert_eq!(app.selected_view.as_ref().unwrap().lines.len(), 1);
        assert!(app.shown_transcript().contains("hello world again"));

        app.timeline_duration = Some(10.0);
        app.playhead = 5.0;
        app.line_edit = Some(LineEdit {
            index: 0,
            speaker: "SPEAKER 1".to_string(),
            content: text_editor::Content::with_text("hello world again"),
        });
        let _ = app.update(Message::SplitLineAtPlayhead);
        assert_eq!(app.selected_view.as_ref().unwrap().lines.len(), 2);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn speaker_filter_limits_the_transcript() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("filter-config");
        let media = temp_dir("filter-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello there\n[00:05] SPEAKER 2: hi\n[00:07] SPEAKER 1: hello again\n",
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
            peaks: Arc::new(vec![0.2; 16]),
        });
        let _ = app.view_item(0);
        app.speaker_filter = Some("SPEAKER 1".to_string());
        app.invalidate_derived();

        let order = app.display_order();
        assert_eq!(order.len(), 2);
        assert!(order
            .iter()
            .all(|index| app.shown_lines()[*index].speaker == "SPEAKER 1"));

        app.search_query = "hello".to_string();
        assert_eq!(app.matching_lines().len(), 2);
        let _ = app.view();

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn line_prefix_handles_multibyte_after_timestamp() {
        assert_eq!(line_prefix("[00:00]日本語"), "[00:00]");
        assert_eq!(line_prefix("[01:02:03] SPEAKER 1: hi"), "[01:02:03] ");
        assert_eq!(line_prefix("plain text"), "");
    }

    #[test]
    fn styled_spans_preserves_multibyte_text() {
        let raw = "İstanbul and İstanbul";
        let spans = styled_spans(raw, "İstanbul", false, false, None, Color::BLACK, 14.0);
        let rebuilt: String = spans.iter().map(|span| span.text.as_ref()).collect();
        assert_eq!(rebuilt, raw);
        assert!(spans.iter().any(|span| span.highlight.is_some()));
    }

    #[test]
    fn checked_shortcuts_respect_focus() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("focus-config");
        let mut app = App::new();
        app.selected_view = Some(ItemView {
            index: 0,
            source: PathBuf::from("talk.txt"),
            transcript: String::new(),
            lines: Vec::new(),
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
            duration: Some(20.0),
            audio: None,
            output: None,
        });
        app.playhead = 1.0;

        let _ = app.update(Message::SeekByChecked(5.0, true));
        assert_eq!(app.playhead, 1.0);
        let _ = app.update(Message::SeekByChecked(5.0, false));
        assert_eq!(app.playhead, 6.0);
    }

    #[test]
    fn reopening_json_transcript_parses_lines() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("json-view-config");
        let media = temp_dir("json-view-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.json");
        std::fs::write(
            &transcript,
            r#"{"lines":[{"time":"00:01","speaker":"SPEAKER 1","text":"hello"}]}"#,
        )
        .unwrap();

        let mut app = App::new();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(2.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });

        let _ = app.view_item(0);
        assert!(app.shown_transcript().contains("[00:01] SPEAKER 1: hello"));
        assert!(!app.shown_transcript().contains("\"lines\""));
        assert!(app.selected_view.as_ref().unwrap().output.is_none());

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn summary_is_limited_to_the_current_batch() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("batch-summary-config");
        let media = temp_dir("batch-summary-media");

        let make_item = |name: &str| {
            let path = media.join(name);
            std::fs::write(&path, b"audio").unwrap();
            QueueItem {
                path: path.clone(),
                audio: Some(path),
                status: ItemStatus::Done,
                duration: Some(6.0),
                transcript: None,
                stats: None,
                turns: Vec::new(),
                peaks: Arc::new(Vec::new()),
            }
        };

        let mut app = App::new();
        app.queue.push(make_item("old.mp3"));
        app.queue.push(make_item("first.mp3"));
        app.queue.push(make_item("second.mp3"));
        app.batch_paths = vec![media.join("first.mp3"), media.join("second.mp3")];

        let files: Vec<String> = app
            .stats_lines()
            .into_iter()
            .filter_map(|line| match line {
                SummaryLine::File(name) => Some(name),
                SummaryLine::Body(_) => None,
            })
            .collect();
        assert_eq!(files, vec!["first.mp3", "second.mp3"]);

        let _ = std::fs::remove_dir_all(&media);
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
        assert!(app.shown_transcript().contains("SPEAKER 1: hello"));

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
        app.line_edit.as_mut().unwrap().content = text_editor::Content::with_text("hi there");
        let _ = app.update(Message::EnterSubmit);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("[00:05] SPEAKER 2: hi there"));

        // Delete asks for confirmation before removing the row.
        let _ = app.update(Message::EditLine(0));
        let _ = app.update(Message::DeleteLineEdit);
        assert!(app.delete_confirm);
        assert!(app.line_edit.is_some());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("ALICE: hello there"));

        // Enter must not save the editor behind the confirmation dialog.
        let _ = app.update(Message::EnterSubmit);
        assert!(app.line_edit.is_some());
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), saved);

        let _ = app.update(Message::CancelDeleteLine);
        assert!(!app.delete_confirm);
        assert!(app.line_edit.is_some());
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] ALICE: hello there\n[00:05] SPEAKER 2: hi there\n"
        );

        let _ = app.update(Message::DeleteLineEdit);
        let _ = app.update(Message::ConfirmDeleteLine);
        assert!(!app.delete_confirm);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(!saved.contains("ALICE: hello there"));
        assert!(saved.contains("SPEAKER 2: hi there"));

        let backup = transcript_backup_path(&transcript);
        let saved_backup = std::fs::read_to_string(backup).unwrap();
        assert!(saved_backup.contains("ALICE: hello there"));

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
    fn theme_toggle_round_trips_through_settings() {
        let _guard = env_lock();
        let _dir = ConfigEnv::new("theme");

        let mut app = App::new();
        assert_eq!(app.theme_mode, AppTheme::Light);

        let _ = app.update(Message::ToggleTheme);
        assert_eq!(app.theme_mode, AppTheme::Dark);

        let reloaded = App::new();
        assert_eq!(reloaded.theme_mode, AppTheme::Dark);
    }

    #[test]
    fn settings_modal_changes_defaults_and_persists() {
        let _guard = env_lock();
        let _dir = ConfigEnv::new("settings-modal");

        let mut app = App::new();
        assert!(!app.settings_open);

        let _ = app.update(Message::OpenSettings);
        assert!(app.settings_open);
        let _ = app.view();

        // Escape closes the settings dialog before anything else.
        let _ = app.update(Message::Escape);
        assert!(!app.settings_open);

        let _ = app.update(Message::OpenSettings);
        let _ = app.update(Message::SetTheme(AppTheme::Dark));
        let _ = app.update(Message::PresetChanged(Preset::Accuracy));
        let _ = app.update(Message::ZoomIn);
        let _ = app.update(Message::ToggleFollowPlayhead);
        let _ = app.update(Message::NotificationsToggled(false));
        let _ = app.update(Message::CloseSettings);
        assert!(!app.settings_open);

        let reloaded = App::new();
        assert_eq!(reloaded.theme_mode, AppTheme::Dark);
        assert_eq!(reloaded.preset, Preset::Accuracy);
        assert!((reloaded.font_scale - 1.1).abs() < 0.001);
        assert!(!reloaded.follow_playhead);
        assert!(!reloaded.notifications);
    }

    #[test]
    fn dropping_a_folder_queues_only_audio_files() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("drop-config");
        let media = temp_dir("drop-media");

        let first = media.join("one.mp3");
        std::fs::write(&first, b"audio").unwrap();
        std::fs::write(media.join("two.wav"), b"audio").unwrap();
        let notes = media.join("notes.txt");
        std::fs::write(&notes, b"text").unwrap();

        let mut app = App::new();
        let _ = app.update(Message::WindowEvent(iced::window::Event::FileHovered(
            first.clone(),
        )));
        assert!(app.drag_hover);

        let _ = app.update(Message::WindowEvent(iced::window::Event::FileDropped(
            media.clone(),
        )));
        assert!(!app.drag_hover);
        assert_eq!(app.queue.len(), 2);
        assert!(app
            .queue
            .iter()
            .all(|item| matches!(item.status, ItemStatus::Pending)));

        // Dropping the same file again, or a non-audio file, is a no-op.
        let _ = app.update(Message::WindowEvent(iced::window::Event::FileDropped(
            first,
        )));
        let _ = app.update(Message::WindowEvent(iced::window::Event::FileDropped(
            notes,
        )));
        assert_eq!(app.queue.len(), 2);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn missing_models_show_a_download_notice_with_progress() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("model-config");
        let models = temp_dir("model-empty");
        let old_models = std::env::var("HIPPO_TRANSCRIBE_MODELS_DIR").ok();
        std::env::set_var("HIPPO_TRANSCRIBE_MODELS_DIR", &models);

        let mut app = App::new();
        assert!(!app.models_ready());
        assert!(app.model_notice(&app.scheme()).is_some());
        let _ = app.view();

        app.model_download = Some(ModelDownload {
            label: "starting".to_string(),
            done: 0,
            total: None,
        });
        {
            let mut events = app.model_events.lock().unwrap();
            events.push_back(DownloadEvent::Started {
                label: "segmentation",
            });
            events.push_back(DownloadEvent::Progress {
                label: "segmentation",
                done: 5,
                total: Some(10),
            });
        }
        let _ = app.update(Message::Tick);
        let state = app.model_download.as_ref().unwrap();
        assert_eq!(state.done, 5);
        assert_eq!(state.total, Some(10));
        let _ = app.view();

        if let Some(old) = old_models {
            std::env::set_var("HIPPO_TRANSCRIBE_MODELS_DIR", old);
        } else {
            std::env::remove_var("HIPPO_TRANSCRIBE_MODELS_DIR");
        }
        let _ = std::fs::remove_dir_all(&models);
    }

    #[test]
    fn palette_has_distinct_colors_for_many_speakers() {
        for scheme in [Scheme::light(), Scheme::dark()] {
            let colors: Vec<Color> = (0..10)
                .map(|index| speaker_palette(index, &scheme))
                .collect();
            for i in 0..colors.len() {
                for j in (i + 1)..colors.len() {
                    assert_ne!(colors[i], colors[j], "speakers {i} and {j} share a color");
                }
            }
        }
    }

    #[test]
    fn ellipsize_keeps_names_readable() {
        assert_eq!(ellipsize_middle("talk.mp3", 20), "talk.mp3");
        let long = "an-interview-recording-with-a-very-long-name.mp3";
        let shortened = ellipsize_middle(long, 24);
        assert!(shortened.chars().count() <= 24);
        assert!(shortened.ends_with(".mp3"));
        assert!(shortened.contains('\u{2026}'));

        let unicode = "日本語のインタビュー録音ファイル";
        assert!(ellipsize(unicode, 8).chars().count() <= 8);
        assert!(ellipsize(unicode, 8).ends_with('\u{2026}'));
    }

    #[test]
    fn queue_summary_and_import_failure_feedback() {
        let _guard = env_lock();
        let config = ConfigEnv::new("queue-summary");
        let dir = config.dir.clone();

        let mut app = App::new();
        assert_eq!(app.queue_summary_text(), "No files yet");

        for (name, duration) in [("one.mp3", Some(60.0)), ("two.mp3", Some(30.0))] {
            let path = dir.join(name);
            app.queue.push(QueueItem {
                path: path.clone(),
                audio: Some(path),
                status: ItemStatus::Pending,
                duration,
                transcript: None,
                stats: None,
                turns: Vec::new(),
                peaks: Arc::new(Vec::new()),
            });
        }
        assert_eq!(app.queue_summary_text(), "2 files \u{00b7} 1:30");
        app.queue[1].duration = None;
        assert_eq!(app.queue_summary_text(), "2 files \u{00b7} 1:00 known");

        let _ = app.update(Message::ImportLoaded(None));
        assert_eq!(
            app.toast.as_ref().map(|(message, _)| message.as_str()),
            Some("Could not read that transcript")
        );
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
            progress_label(
                &Status::Done,
                "transcribing 3/4",
                0.75,
                true,
                1,
                Some("talk.mp3")
            ),
            "talk.mp3 \u{2014} Transcribing 3/4 (75%)"
        );
    }

    #[test]
    fn speaker_options_are_fixed_counts() {
        assert_eq!(SpeakerOpt::ALL.len(), 5);
        assert_eq!(SpeakerOpt::from_value(None), SpeakerOpt::Two);
        assert_eq!(SpeakerOpt::from_value(Some(0)), SpeakerOpt::Two);
        assert_eq!(SpeakerOpt::from_value(Some(2)), SpeakerOpt::Two);
        assert!(SpeakerOpt::ALL
            .iter()
            .all(|option| option.value().is_some()));
    }

    #[test]
    fn failed_items_can_be_retried_when_idle() {
        let _guard = env_lock();
        let config = ConfigEnv::new("retry-config");
        let dir = config.dir.clone();
        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();

        let mut app = App::new();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Failed("boom".to_string()),
            duration: None,
            transcript: None,
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });

        // A running batch cannot be mutated.
        app.status = Status::Running;
        let _ = app.update(Message::RetryItem(0));
        assert!(matches!(app.queue[0].status, ItemStatus::Failed(_)));

        app.status = Status::Idle;
        let _ = app.update(Message::RetryItem(0));
        assert!(matches!(app.queue[0].status, ItemStatus::Pending));

        // The pending state is persisted.
        let (loaded, _) = App::load_queue_and_batch();
        assert!(matches!(loaded[0].status, ItemStatus::Pending));
    }

    #[test]
    fn queue_items_can_be_reordered_when_idle() {
        let _guard = env_lock();
        let config = ConfigEnv::new("reorder-config");
        let dir = config.dir.clone();

        let mut app = App::new();
        for name in ["a.mp3", "b.mp3", "c.mp3"] {
            let path = dir.join(name);
            std::fs::write(&path, b"audio").unwrap();
            app.queue.push(QueueItem {
                path: path.clone(),
                audio: Some(path),
                status: ItemStatus::Pending,
                duration: None,
                transcript: None,
                stats: None,
                turns: Vec::new(),
                peaks: Arc::new(Vec::new()),
            });
        }
        app.selected = Some(0);
        app.selected_view = Some(ItemView {
            index: 0,
            source: dir.join("a.txt"),
            transcript: String::new(),
            lines: Vec::new(),
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
            duration: None,
            audio: None,
            output: None,
        });

        let names = |app: &App| -> Vec<String> {
            app.queue
                .iter()
                .map(|item| {
                    item.path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        };

        let _ = app.update(Message::MoveItemUp(1));
        assert_eq!(names(&app), vec!["b.mp3", "a.mp3", "c.mp3"]);
        // Selection follows the moved entry.
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.selected_view.as_ref().unwrap().index, 1);

        let _ = app.update(Message::MoveItemDown(0));
        assert_eq!(names(&app), vec!["a.mp3", "b.mp3", "c.mp3"]);

        // Reordering is locked while transcribing.
        app.status = Status::Running;
        let _ = app.update(Message::MoveItemUp(2));
        assert_eq!(names(&app), vec!["a.mp3", "b.mp3", "c.mp3"]);

        // The order is persisted.
        let (loaded, _) = App::load_queue_and_batch();
        let loaded_names: Vec<String> = loaded
            .iter()
            .map(|item| {
                item.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(loaded_names, vec!["a.mp3", "b.mp3", "c.mp3"]);
    }

    #[test]
    fn cached_orders_match_direct_derivation_after_edit() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("cache-order-config");
        let media = temp_dir("cache-order-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:05] SPEAKER 1: second\n[00:00] SPEAKER 2: first\n[00:03] SPEAKER 1: third\n",
        )
        .unwrap();

        let mut app = App::new();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(6.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });
        let _ = app.view_item(0);

        // Populate both caches before mutating a line.
        assert_eq!(app.display_order(), sorted_display_order(app.shown_lines()));
        assert_eq!(
            app.speaker_order(),
            distinct_speaker_order(app.shown_lines())
        );

        let _ = app.update(Message::EditLine(0));
        {
            let edit = app.line_edit.as_mut().unwrap();
            edit.speaker = "ZED".to_string();
            edit.content = text_editor::Content::with_text("changed");
        }
        let _ = app.update(Message::SaveLineEdit);

        assert_eq!(app.display_order(), sorted_display_order(app.shown_lines()));
        assert_eq!(
            app.speaker_order(),
            distinct_speaker_order(app.shown_lines())
        );
        assert_eq!(
            app.speaker_order(),
            vec![
                "ZED".to_string(),
                "SPEAKER 2".to_string(),
                "SPEAKER 1".to_string()
            ]
        );

        let _ = std::fs::remove_dir_all(&media);
    }
}
