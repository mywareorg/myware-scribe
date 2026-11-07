use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use iced::keyboard::{self, Key, Modifiers};
use iced::overlay::menu;
use iced::widget::{
    button, canvas, column, container, horizontal_rule, horizontal_space, mouse_area, pick_list,
    progress_bar, rich_text, row, scrollable, stack, text, text_editor, text_input, tooltip,
};
use iced::{
    font, mouse, Color, Element, Font, Length, Pixels, Point, Rectangle, Renderer, Shadow, Size,
    Subscription, Task, Theme, Vector,
};
use rodio::{OutputStream, OutputStreamBuilder, Sink, Source};
use serde::{Deserialize, Serialize};

use crate::backend::{self, Event};
use crate::theme::{self, AppTheme, Scheme};
use crate::{documents::*, files::*, format::*, options::*};
use hippo_transcribe_core::asr;
use hippo_transcribe_core::benchmark::{self, BenchmarkEvent};
use hippo_transcribe_core::download::{self, DownloadEvent, ModelStatus};
use hippo_transcribe_core::export::ExportFormat;
use hippo_transcribe_core::license;
use hippo_transcribe_core::models;
use hippo_briefing::{Briefing, BriefingLine};
use hippo_transcribe_core::sidecar::Word;
use hippo_transcribe_core::playback::AudioStream;
use hippo_transcribe_core::vocabulary::Vocabulary;

/// The Settings label that means "whatever Windows has as default".
const SYSTEM_DEFAULT_DEVICE: &str = "System default";

/// Third-party notices, shipped in the package and shown in Help → Licenses.
const LICENSES_TEXT: &str = include_str!("../../LICENSES.md");

const AUDIO_EXTS: &[&str] = &[
    "mp3", "mp2", "wav", "aif", "aiff", "m4a", "m4b", "m4r", "m4v", "aac", "flac", "ogg", "oga",
    "opus", "caf", "mp4", "mkv", "mov", "webm", "3gp", "3g2",
];

/// Height of the waveform/speaker timeline strip.
const TIMELINE_HEIGHT: f32 = 36.0;

/// Movement smaller than this is invisible, so the follow logic does not
/// issue a programmatic transcript scroll for it. Kept under the 6 px
/// programmatic-scroll tolerance in `Message::TranscriptScrolled`.
const FOLLOW_DEADBAND: f32 = 3.0;

/// Shortest gap between two follow scrolls. While the target keeps changing
/// (streamed text), issuing on every frame turns smooth growth into a series
/// of one-frame jumps.
const FOLLOW_MIN_INTERVAL: Duration = Duration::from_millis(120);

/// How far the target must move before a follow scroll is retried after one
/// that produced no movement. `scroll_to` clamps to the scrollable's own
/// content bounds, so a target a few pixels past the end can never be reached
/// and must not be re-requested every tick.
const FOLLOW_STUCK_STEP: f32 = 12.0;

/// How much the followed inputs (content height, viewport height, playhead)
/// must change before a follow scroll counts as a new situation rather than a
/// retry of the same one. Render layout jitters by a pixel or two without
/// anything actually happening.
const FOLLOW_INPUT_STEP: f32 = 12.0;

/// Time constant of the follow glide. Each step covers `1 - e^(-dt/tau)` of the
/// remaining distance: about 17 % per 60 Hz frame, 63 % after 90 ms and 95 %
/// after 270 ms, so a one-row move settles in roughly a fifth of a second.
const FOLLOW_GLIDE_TAU: Duration = Duration::from_millis(90);

/// Longest frame gap the glide will ease over. `window::frames()` stops while
/// the window is occluded, so coming back can present a gap of seconds; that
/// must land the move in one step rather than sweep the pane across.
const FOLLOW_GLIDE_MAX_GAP: Duration = Duration::from_millis(250);

/// Shortest gap between two glide steps, about 30-40 Hz. Every step is a
/// `scroll_to`, and the scrollbar reports each one back as a
/// `TranscriptScrolled`: one step is two updates and two full view builds.
const FOLLOW_GLIDE_FRAME: Duration = Duration::from_millis(25);

/// Movement smaller than this is not worth that pair of updates.
const FOLLOW_GLIDE_MIN_STEP: f32 = 0.5;

/// Remaining distance at which the glide lands exactly and ends.
const FOLLOW_GLIDE_SNAP: f32 = 0.5;

/// Routine follow moves glide only this many rows or fewer. A longer move that
/// nobody asked for (a view change, a stall) lands immediately.
const FOLLOW_GLIDE_ROWS: f32 = 2.0;

/// Time constant of a deliberate jump of [`FOLLOW_JUMP_ROWS`] rows or more. A
/// fixed routine constant made a screenful arrive in the same time as a
/// one-row nudge, which reads as a lurch: the longer move starts faster and has
/// further to go, so it needs longer to feel like travel rather than a cut.
const FOLLOW_JUMP_TAU: Duration = Duration::from_millis(170);

/// Distance at which a jump reaches [`FOLLOW_JUMP_TAU`]. Between the routine
/// ceiling and this the constant ramps, so a three-row jump is not slower than
/// a two-row nudge.
const FOLLOW_JUMP_ROWS: f32 = 10.0;

/// Two seeks closer together than this are one gesture — a scrubbed playhead or
/// a held arrow key — and must land at once. Animating each of them would leave
/// the pane trailing the pointer.
const FOLLOW_SEEK_QUIET: Duration = Duration::from_millis(200);

/// Logical 4:3 sizing for the window (default) and floors.
// The transcript header and bottom action row need this much horizontal room
// for the fixed-width Time, Reveal and Rename buttons to keep their size.
const MIN_W: f32 = 1024.0;
const MIN_H: f32 = 768.0;

/// Aspect-ratio snap grid (pixels). 4 keeps things crisp.
const ASPECT_SNAP_PX: f32 = 4.0;
const ASPECT: f32 = 4.0 / 3.0; // w/h
const ASPECT_INV: f32 = 3.0 / 4.0; // h/w

const PANEL_GAP: f32 = 12.0;
const OUTER_PADDING: f32 = 12.0;
const LEFT_PANEL_WIDTH: f32 = 340.0;

/// UI chrome never wraps; labels are shortened or ellipsized instead.
const NO_WRAP: iced::widget::text::Wrapping = iced::widget::text::Wrapping::None;

/// Transcript text scale when nothing has been stored yet: 70 % of the base size.
const DEFAULT_FONT_SCALE: f32 = 0.7;

/// Gap the right-hand column puts between its islands. The column is built with
/// `spacing(0)` and every gap is carried by a padding instead, so this one
/// number is the distance from the transcript card to the run card and from the
/// transcript card to the Find row.
const ISLAND_GAP: f32 = theme::SPACE_LG;

/// Height of the editor strip, which is inserted between the transcript header
/// and the list, inside the card. Its padding and bottom rule are inside it; the
/// tallest control in it is the Done button at 30.8 px, so this leaves a little
/// slack rather than clipping.
const LINE_EDIT_PANEL_HEIGHT: f32 = 38.0;

/// The strip's height: its own box while a line is being edited, nothing when no
/// editor is open, so the list runs right up to the header.
fn editor_strip_height(editing: bool) -> f32 {
    if editing {
        LINE_EDIT_PANEL_HEIGHT
    } else {
        0.0
    }
}

/// Where the scroll offset has to go when the editor strip appears (`delta` is
/// positive) or disappears, so the content does not jump on screen.
///
/// Inserting the strip above the list moves the list's top edge, and every row
/// with it, so the offset moves the other way by the same amount. Two bounds: the
/// document's top cannot be pulled in from above, so the offset stops at zero and
/// the content simply starts lower; and `ceiling` is the highest offset that still
/// leaves the edited row below the strip, because scrolled past that the field
/// would sit under it.
fn compensate_offset(offset: f32, delta: f32, ceiling: f32) -> f32 {
    (offset + delta).min(ceiling).max(0.0)
}

/// Gap between the strip's controls.
const LINE_EDIT_GAP: f32 = 4.0;

/// Outer height of a control that carries one line of text: iced's 1.3 line box
/// at the 16 px UI size (20.8 px) plus the 5 px of vertical padding every text
/// button uses. Icon-only controls take the same height so a row that mixes the
/// two is one control height rather than as tall as its tallest child.
const CONTROL_HEIGHT: f32 = 30.8;

/// The type size of every button label. One value on purpose: a button that
/// needs to look smaller changes its kind, never its text. Buttons used to set
/// their own size at the call site, which is how a 12 px label ended up beside a
/// 16 px one in the same row.
const BUTTON_LABEL_SIZE: f32 = 16.0;

/// Width of an on/off button. Fixed so `On` and `Off` share one box.
const TOGGLE_WIDTH: f32 = 64.0;

/// Diameter of the panel's circular icon buttons: a standard control height, so
/// they line up with the text buttons around them.
const PANEL_ICON_DIAMETER: f32 = CONTROL_HEIGHT;

/// Glyph size for an icon-only control. Matches the queue's clear button, which
/// is the reference the panel was asked to follow.
const ICON_ONLY_SIZE: f32 = 15.0;

/// Gap above the header's controls, below them, and between them and the rule.
/// One number for all three, so the strip is evenly inset instead of sitting
/// nearer the rule than the card's top edge. It is the header row's own spacing
/// too, which is why it takes the app's standard small space.
const HEADER_GAP: f32 = theme::SPACE_SM;

/// Height of the header's identity block when it carries a meta line: the 13 px
/// title, its 1 px gap and the 11 px meta, at iced's 1.3 line height.
const HEADER_LABEL_HEIGHT: f32 = 13.0 * 1.3 + 1.0 + 11.0 * 1.3;

/// Gap the header's slot leaves under the header's rule.
const HEADER_SLOT_GAP: f32 = theme::SPACE_SM;

/// Where the transcript header menus' top edge sits inside the card: the
/// header's gap above and below its controls, the identity block, the gap to the
/// rule, the rule itself and the slot's gap. Their width is not fixed: each menu
/// is as wide as its own widest row.
const VIEW_MENU_TOP: f32 =
    HEADER_GAP * 3.0 + HEADER_LABEL_HEIGHT + 1.0 + HEADER_SLOT_GAP;

/// Width of a header menu row's leading check slot. Fixed, so every label in a
/// menu starts on the same left edge whether its row is checked or not.
const HEADER_MENU_CHECK: f32 = 16.0;

/// The app header's logo, which is the tallest thing in that row and therefore
/// sets its height. The Help menu hangs from the row's bottom edge, so its top
/// is measured from this.
const HEADER_LOGO_SIZE: f32 = 67.0;

/// Top edge of the Help menu: the window's outer padding, the header row, and
/// the same small gap the menus inside the transcript card hang by. Its right
/// edge is `OUTER_PADDING` from the window edge, which is the Help button's own
/// right edge because the button ends the row.
const HELP_MENU_TOP: f32 = OUTER_PADDING + HEADER_LOGO_SIZE + theme::SPACE_SM;

/// The header's horizontal padding. The menus hang from the same left edge, so
/// this is named rather than written into both.
const HEADER_PAD_X: f32 = 8.0;

/// The header's identity badge and the gap between it and the label. The
/// filename's budget is measured past both.
const HEADER_BADGE_WIDTH: f32 = 26.0;
const HEADER_BADGE_GAP: f32 = 8.0;

/// Fewest characters the filename is cut down to. Below this a middle ellipsis
/// leaves two characters either side of it, which no longer names the file.
const MIN_NAME_CHARS: usize = 8;

/// Fixed widths of the header's menu buttons. A menu is anchored by adding the
/// widths of the buttons to its left, so these must be the widths the buttons
/// are actually given.
const FILE_BUTTON_WIDTH: f32 = 78.0;
const EDIT_BUTTON_WIDTH: f32 = 78.0;
const VIEW_BUTTON_WIDTH: f32 = 84.0;
const HELP_BUTTON_WIDTH: f32 = 78.0;

/// Left offsets of the four menus. The header starts `[File][Edit][View][Help]`,
/// so File's menu hangs from the card's own padding and each later one sits a
/// button and a gap further right. The menu layer aligns its child to the start,
/// so these are distances from the card's left edge.
fn file_menu_offset() -> f32 {
    HEADER_PAD_X
}

fn edit_menu_offset() -> f32 {
    file_menu_offset() + FILE_BUTTON_WIDTH + theme::SPACE_SM
}

fn view_menu_offset() -> f32 {
    edit_menu_offset() + EDIT_BUTTON_WIDTH + theme::SPACE_SM
}


/// iced's default text line height, as a multiple of the font size.
const LINE_HEIGHT_FACTOR: f32 = 1.3;

/// Padding inside every modal card (see `modal_card`).
const MODAL_PADDING: f32 = 16.0;

/// Mean token log-probability below which an utterance is treated as unsure.
const LOW_CONFIDENCE: f32 = -0.55;

/// Speaker-filter entry that shows only low-confidence utterances.
const LOW_CONFIDENCE_FILTER: &str = "Low confidence";

/// Number of RMS buckets requested when probing an audio file for the timeline.
const TIMELINE_BUCKETS: usize = 2000;

/// Copy for the empty-state feature deck. Slides are manual; no autoplay.
struct FeatureSlide {
    eyebrow: &'static str,
    title: &'static str,
    body: &'static str,
}

const FEATURE_DECK: [FeatureSlide; 5] = [
    FeatureSlide {
        eyebrow: "FREE",
        title: "Full-length transcription. Forever.",
        body: "Balanced model \u{2014} the best free pipeline. One speaker, unlimited files, \
             no minutes, no subscription.",
    },
    FeatureSlide {
        eyebrow: "PRO",
        title: "Separate every speaker.",
        body: "Shows who said what, labels each voice, and lets you rename speakers in the transcript.",
    },
    FeatureSlide {
        eyebrow: "PRO",
        title: "Queue the whole folder.",
        body: "Queue a whole folder and run it as one batch with shared language and speaker settings.",
    },
    FeatureSlide {
        eyebrow: "PRO",
        title: "Review, edit and choose.",
        body: "Playback, line editing, and Speed or Accuracy presets give you control over every transcript.",
    },
    FeatureSlide {
        eyebrow: "WHY HIPPO",
        title: "Hippo Transcribe is HIPAA inspired.",
        body: "Open source. Built for doctors and medical professionals. \
               Your recordings stay on this machine.",
    },
];

#[derive(Debug, Clone, PartialEq)]
enum ItemStatus {
    Pending,
    /// Started, then the process died before the file finished. Kept apart
    /// from Pending so the row can say what happened and offer a resume.
    Interrupted,
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
struct QueueItem {
    /// Identity used to match backend events and to de-duplicate additions:
    /// the audio path when one is known, otherwise the transcript path.
    path: PathBuf,
    /// Audio file backing this item.
    audio: Option<PathBuf>,
    status: ItemStatus,
    duration: Option<f64>,
    /// Output transcript (None while pending, or for a missing file).
    transcript: Option<PathBuf>,
    /// Run statistics for transcripts produced by the pipeline.
    stats: Option<backend::Stats>,
    /// Diarization turns, persisted with the queue for timeline colours.
    turns: Vec<backend::Turn>,
    peaks: Arc<Vec<f32>>,
}

/// A transcript found beside one of the audio files just added.
#[derive(Debug, Clone)]
pub(crate) struct TranscriptHit {
    /// The queue row the audio was added as, and the audio itself. Adoption is
    /// dropped if the row has moved or started running by the time it lands.
    index: usize,
    audio: PathBuf,
    transcript: PathBuf,
}

/// A found transcript, read and measured off the UI thread.
#[derive(Debug, Clone)]
pub(crate) struct AdoptedTranscript {
    index: usize,
    audio: PathBuf,
    transcript: PathBuf,
    text: String,
    duration: Option<f64>,
    peaks: Vec<f32>,
    turns: Vec<backend::Turn>,
}

/// Persisted queue entry. Speaker turns are stored so the timeline keeps its
/// speaker colours after a restart even if the diarization cache is gone.
#[derive(Serialize, Deserialize)]
struct QueueState {
    #[serde(default)]
    items: Vec<QueueRecord>,
    /// Options of the batch that was last started.
    #[serde(default)]
    last_run: Option<RunRecord>,
    /// Paths included in the most recent transcription batch.
    #[serde(default)]
    last_batch: Vec<String>,
}

/// The options a batch was started with, kept so an interrupted run can say
/// what it was doing after a restart changed the settings.
#[derive(Serialize, Deserialize, Clone, Default)]
struct RunRecord {
    #[serde(default)]
    preset: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    num_speakers: Option<i32>,
    #[serde(default)]
    output_dir: Option<String>,
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
    /// Diarization turns, so speaker bands survive a restart.
    #[serde(default)]
    turns: Vec<backend::Turn>,
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

/// Undo/redo for transcript edits made through the line editor.
///
/// A snapshot is the edited document's lines: [`DocLine`] carries each line's
/// speaker label, so restoring the lines restores the speaker map with it.
#[derive(Default)]
struct LineHistory {
    /// The document the stacks belong to: the selected view's queue index, or
    /// `None` for the live transcript.
    document: Option<usize>,
    undo: Vec<Vec<DocLine>>,
    redo: Vec<Vec<DocLine>>,
}

/// States kept for undo; the oldest falls off once the stack is full.
const LINE_UNDO_LIMIT: usize = 50;

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

/// The upgrade prompt after a Free run opens on roughly one run in five.
const UPSELL_SHOW_MOD: u64 = 5;

/// Roll the per-completion nudge, so Free users see the paywall occasionally
/// rather than after every run. FNV-1a spreads the clock seed evenly.
fn upsell_roll() -> bool {
    let seed = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.to_le_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % UPSELL_SHOW_MOD == 0
}

/// Calibration result and selection for one preset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GpuProfile {
    /// The preset has been offered calibration and either tested or skipped.
    #[serde(default)]
    tested: bool,
    /// Measured CPU-time / GPU-time ratio.
    #[serde(default)]
    multiplier: Option<f32>,
    /// Why the GPU path failed, if it did.
    #[serde(default)]
    gpu_error: Option<String>,
    /// Whether this preset should start on the GPU.
    #[serde(default)]
    selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalibrationPhase {
    Offered,
    TestingCpu,
    TestingGpu,
    Result,
}

struct Calibration {
    preset: Preset,
    phase: CalibrationPhase,
    progress: f32,
    cpu_seconds: Option<f64>,
    gpu_seconds: Option<f64>,
    multiplier: Option<f32>,
    error: Option<String>,
    /// True when the failure came from the GPU backend itself, not from a
    /// missing model or calibration setup.
    backend_failed: bool,
    /// True when calibration was opened by pressing Transcribe, so a choice
    /// continues into the run instead of only updating the preference.
    start_run: bool,
    /// The result has already been persisted once.
    recorded: bool,
}

impl Calibration {
    fn new(preset: Preset, start_run: bool) -> Self {
        Self {
            preset,
            phase: CalibrationPhase::Offered,
            progress: 0.0,
            cpu_seconds: None,
            gpu_seconds: None,
            multiplier: None,
            error: None,
            backend_failed: false,
            start_run,
            recorded: false,
        }
    }

    fn testing(&self) -> bool {
        matches!(
            self.phase,
            CalibrationPhase::TestingCpu | CalibrationPhase::TestingGpu
        )
    }
}

/// The briefing dialog's state.
enum BriefingState {
    /// The model has not been downloaded yet.
    NeedsModel,
    /// The helper is running.
    Running(BriefingRun),
    Ready {
        briefing: Briefing,
        lines: Vec<BriefingLine>,
        source: Option<PathBuf>,
    },
    Failed(String),
}

struct BriefingRun {
    events: Arc<Mutex<VecDeque<backend::BriefingEvent>>>,
    pid: Arc<AtomicU32>,
    response: PathBuf,
    lines: Vec<BriefingLine>,
    source: Option<PathBuf>,
    stage: String,
    fraction: f32,
    started: Instant,
}

#[derive(Serialize, Deserialize)]
struct Settings {
    preset: Option<String>,
    sms_mode: Option<bool>,
    num_speakers: Option<i32>,
    language: Option<String>,
    /// Microphone chosen in Settings. `None` means the system default.
    #[serde(default)]
    input_device: Option<String>,
    #[serde(default = "timestamps_default")]
    show_timestamps: bool,
    #[serde(default = "notifications_default")]
    notifications: bool,
    /// Show the Editing Tips card at the end of a run, until the reader ticks
    /// "Don't show these tips again".
    #[serde(default = "editing_tips_default")]
    editing_tips: bool,
    /// Lift quiet recordings before ASR and diarization.
    #[serde(default = "normalize_default")]
    normalize: bool,
    /// Highlight words as playback reaches them.
    #[serde(default = "karaoke_default")]
    karaoke: bool,
    #[serde(default)]
    gpu_profiles: HashMap<String, GpuProfile>,
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
    #[serde(default)]
    eta_throughput: Option<f32>,
    /// Wall seconds of diarization per audio second, blended across runs.
    #[serde(default)]
    eta_diarize: Option<f32>,
    /// Wall seconds of post-ASR work per audio second, blended across runs.
    #[serde(default)]
    eta_prepare: Option<f32>,
}

fn notifications_default() -> bool {
    true
}

fn timestamps_default() -> bool {
    true
}

fn editing_tips_default() -> bool {
    true
}

fn normalize_default() -> bool {
    true
}

fn karaoke_default() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: None,
            sms_mode: None,
            num_speakers: None,
            language: None,
            input_device: None,
            show_timestamps: true,
            notifications: true,
            editing_tips: true,
            normalize: true,
            karaoke: true,
            gpu_profiles: HashMap::new(),
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
            eta_throughput: None,
            eta_diarize: None,
            eta_prepare: None,
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

/// The phase a run is in, as the readout names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EtaPhase {
    Preparing,
    Transcribing,
    Diarizing,
    Writing,
    Finishing,
}

impl EtaPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Transcribing => "transcribing",
            Self::Diarizing => "diarizing",
            Self::Writing => "writing",
            Self::Finishing => "finishing",
        }
    }
}

/// What the readout says: which phase the run is in, and seconds left when that
/// phase can honestly be estimated.
///
/// `remaining: None` is a real answer, not a failure: a phase that reports no
/// rate gets its name and no digits, rather than a number made up from another
/// phase's rate.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EtaView {
    phase: EtaPhase,
    remaining: Option<f32>,
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
    /// A fixed-column table: header cells, then one row per file and a total.
    Table(Vec<String>, Vec<Vec<String>>),
}

/// A View-menu toggle reachable by a single key. The letters are shown in the
/// menu, so the shortcut and the hint cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewShortcut {
    Follow,
    Timestamps,
    Sms,
    Join,
    Karaoke,
    Confidence,
}

impl ViewShortcut {
    fn key(self) -> &'static str {
        match self {
            Self::Follow => "f",
            Self::Timestamps => "t",
            Self::Sms => "s",
            Self::Join => "j",
            Self::Karaoke => "k",
            Self::Confidence => "c",
        }
    }
}

/// Which menu in the transcript header is open. One at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderMenu {
    File,
    Edit,
    View,
    Help,
}

/// The dialog that owns the screen, if any. One at a time.
///
/// The variants are listed in paint order: when more than one of the backing
/// flags happens to be set, the first is the one drawn and the one Escape
/// closes. `view` matches on [`App::open_modal`], so the order here is the only
/// place the precedence is written down.
///
/// A header menu is not a modal: it takes the pointer but leaves the keyboard
/// and tooltips alone, so it is tracked separately (`header_menu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modal {
    Reset,
    Summary,
    DeleteLine,
    Calibration,
    Paywall,
    Settings,
    Help,
    RunSetup,
    Rename,
    Vocabulary,
    Briefing,
}

#[derive(Debug, Clone)]
pub enum Message {
    PickOutput,
    OutputPicked(Option<PathBuf>),
    PresetChanged(Preset),
    Start,
    Cancel,
    Tick,
    /// One display frame. `window::frames()` is subscribed only while a follow
    /// glide runs or a caret or focus is waiting for a first layout: those are
    /// the only things that need a frame clock.
    Frame(Instant),
    RenameName(usize, String),
    RenameSave,
    RenameCancel,
    RenameMove(bool),
    EnterSubmit,
    ShiftEnterSubmit,
    SelectLine(usize),
    LineClicked(usize),
    /// A press on a line's words, at a point in the text's own coordinates.
    /// Carries the caret position a double-click should open at.
    LineTextPressed(usize, Point),
    /// Cycle the edited line through the transcript's speakers.
    CycleLineSpeaker,
    /// Give the edited line the next free numbered speaker, for a voice
    /// diarization missed.
    AddLineSpeaker,
    LineHover(Option<usize>),
    LineUnhover(usize),
    SelectRelative(i32),
    SelectRelativeChecked(i32, bool),
    EditActiveLine,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFollowPlayhead,
    ToggleConfidenceView,
    SetTheme(AppTheme),
    OpenSettings,
    CloseSettings,
    OpenPaywall,
    ClosePaywall,
    LockedPreset,
    LockedSpeakers,
    LockedGpu,
    LockedPlayer,
    LockedRename,
    FeatureAdvance,
    PurchasePro,
    RestorePro,
    StartTrial,
    ToggleTier,
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
    TogglePlayerCollapsed,
    NotificationsToggled(bool),
    SetAcceleration(bool),
    TestGpu,
    UseCpuForRun,
    UseGpuForRun,
    RetestGpu,
    CloseCalibration,
    /// Open the folder holding the log files in Explorer.
    OpenLogFolder,
    TimestampsToggled(bool),
    /// The pointer left the transcript card. An open View menu closes with it.
    TranscriptLeft,
    ToggleHeaderMenu(HeaderMenu),
    CloseHeaderMenu,
    /// Show the Reset warning; nothing is reverted until it is confirmed.
    RequestResetTranscript,
    ConfirmResetTranscript,
    CloseResetConfirm,
    /// Show the run options (save location, preset, speakers, acceleration)
    /// before starting.
    OpenRunSetup,
    ConfirmRunSetup,
    CloseRunSetup,
    ToggleJoinLines,
    SpeakerCountChanged(SpeakerOpt),
    LanguageChanged(LanguageOpt),
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
    ShortcutPlayChecked(bool),
    /// A plain View-menu key, outside a text field.
    ViewShortcut(ViewShortcut),
    ViewShortcutChecked(ViewShortcut, bool),
    FindHover(bool),
    /// The Find row is open: is its field still focused?
    PollFindFocus,
    FindFocusPolled(bool),
    SeekBy(f32),
    SeekByChecked(f32, bool),
    CopyTranscript,
    SaveTranscriptAs,
    SaveTranscriptAsPicked(Option<PathBuf>),
    RevealOutput,
    OpenRename,
    /// Open the custom-vocabulary editor, from Edit or from Help.
    OpenVocabulary,
    CloseVocabulary,
    VocabTermsAction(text_editor::Action),
    VocabReplacementsAction(text_editor::Action),
    SaveVocabulary,
    /// Leave an interrupted file out of the next run without deleting its row.
    SkipItem(usize),
    StartRecording,
    StopRecording,
    InputDeviceChanged(String),
    NormalizeToggled(bool),
    KaraokeToggled(bool),
    OpenBriefing,
    CloseBriefing,
    RetryBriefing,
    SaveBriefing,
    DownloadBriefingModel,
    LockedBriefing,
    EditLine(usize),
    FlipLineSpeaker(usize),
    /// The line editor's speaker picker opened or closed its menu.
    LineEditAction(usize, text_editor::Action),
    /// Commit the inline editor and close it, resuming playback if it was
    /// playing when the edit started.
    DoneLineEdit,
    /// Close the inline editor and drop what was typed.
    CancelLineEdit,
    /// Ask before removing the line being edited.
    DeleteLineEdit,
    ConfirmDeleteLine,
    CancelDeleteLine,
    MergeNextLine,
    MergePrevLine,
    SplitLineAtCursor,
    UndoLineEdit,
    RedoLineEdit,
    CloseSummary,
    /// Turn the end-of-run Editing Tips card on or off. Saved at once.
    SetEditingTips(bool),
    AddFiles,
    FilesPicked(Option<Vec<PathBuf>>),
    AddFolder,
    FolderPicked(Option<PathBuf>),
    /// Transcripts found for the files in one add. The flag says the add
    /// produced a single file, which is the case that opens itself.
    TranscriptsFound(bool, Vec<TranscriptHit>),
    /// One found transcript, read and measured off the UI thread.
    TranscriptAdopted(bool, AdoptedTranscript),
    RemoveItem(usize),
    ClearQueue,
    ItemDurationProbed(PathBuf, Option<f64>),
    ViewDetailsLoaded(
        usize,
        PathBuf,
        Option<Vec<backend::Turn>>,
        Option<(f64, Vec<f32>)>,
    ),
    QueueWaveformLoaded(PathBuf, Option<(f64, Vec<f32>)>),
    RetryItem(usize),
    QueueDragStart(usize),
    QueueDrop(usize),
    QueueHover(Option<usize>),
    QueuePointerMoved(Point),
    QueuePointerLeft,
    ResetOutputDir,
    OpenModelsFolder,
}

/// One programmatic follow scroll: where it was asked to go, the offset the
/// scrollable reported at that moment, and the inputs it was computed from.
///
/// Follow is edge-triggered off this record. Remembering the inputs is what
/// keeps it quiet: with a static playhead and static content the same target
/// must never be re-requested, however far the scrollable settled from it.
#[derive(Debug, Clone, Copy)]
struct FollowIssue {
    at: Instant,
    target: f32,
    offset: f32,
    active: Option<usize>,
    playhead: f32,
    content: f32,
    viewport: f32,
    /// The same situation already got its one retry and still did not move.
    retried: bool,
}

/// A follow scroll that is easing towards its target instead of snapping to
/// it. Only the playhead-tracking path ever glides: the bottom pin follows a
/// target that grows as text streams in, and smoothing that would leave the
/// pane permanently behind the words.
#[derive(Debug, Clone, Copy)]
struct ScrollAnim {
    /// Where the pane is heading.
    target: f32,
    /// Offset the last step asked the scrollable to take.
    expected: f32,
    /// When that step was issued.
    last: Instant,
    /// Size of that step, so a report that arrives one issue late still counts
    /// as our own scroll rather than a drag.
    step: f32,
    /// How long this move takes: quick for a routine advance, longer for a
    /// deliberate jump.
    tau: Duration,
    /// True while this is a jump the user asked for. A jump still in flight is
    /// retargeted with the jump's shape, so the routine distance cap cannot cut
    /// it short when the playhead moves on mid-glide.
    jump: bool,
}

/// How a follow scroll travels.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Glide {
    /// Land now: no easing.
    Snap,
    /// Ease towards the target with this time constant.
    Ease { tau: Duration },
}

/// A scroll the follow logic wants, and whether it comes from tracking the
/// playhead line (the only path allowed to glide).
#[derive(Debug, Clone, Copy, PartialEq)]
struct FollowTarget {
    y: f32,
    centred: bool,
}

impl FollowIssue {
    /// True when nothing that decides the target has changed since `self`.
    fn same_situation(
        &self,
        active: Option<usize>,
        playhead: f32,
        content: f32,
        viewport: f32,
    ) -> bool {
        self.active == active
            && (self.playhead - playhead).abs() <= 0.01
            && (self.content - content).abs() <= FOLLOW_INPUT_STEP
            && (self.viewport - viewport).abs() <= FOLLOW_INPUT_STEP
    }
}

/// Fraction of the remaining distance one glide step covers.
///
/// Exponential rather than a fixed-duration curve: a tween restarts its clock
/// on every retarget, which stutters as soon as lines arrive faster than it
/// finishes, and it needs a duration that is wrong at every speed but one.
fn glide_fraction(dt: Duration, tau: Duration) -> f32 {
    if dt >= FOLLOW_GLIDE_MAX_GAP {
        // The window was occluded (frames stop) or the app was stalled: land
        // the move in this step instead of sweeping across the gap.
        return 1.0;
    }
    1.0 - (-dt.as_secs_f32() / tau.as_secs_f32().max(f32::EPSILON)).exp()
}

/// One step of the glide: the offset to ask for, and whether that offset is
/// the target itself (the glide is over).
fn glide_advance(expected: f32, target: f32, dt: Duration, tau: Duration) -> (f32, bool) {
    let next = expected + (target - expected) * glide_fraction(dt, tau);
    if (target - next).abs() <= FOLLOW_GLIDE_SNAP {
        (target, true)
    } else {
        (next, false)
    }
}

/// Time constant for a deliberate jump, ramped by distance.
fn jump_tau(distance: f32, row_height: f32) -> Duration {
    let rows = distance.abs() / row_height.max(1.0);
    if rows <= FOLLOW_GLIDE_ROWS {
        return FOLLOW_GLIDE_TAU;
    }
    if rows >= FOLLOW_JUMP_ROWS {
        return FOLLOW_JUMP_TAU;
    }
    let ramp = (rows - FOLLOW_GLIDE_ROWS) / (FOLLOW_JUMP_ROWS - FOLLOW_GLIDE_ROWS);
    let tau = FOLLOW_GLIDE_TAU.as_secs_f32()
        + (FOLLOW_JUMP_TAU.as_secs_f32() - FOLLOW_GLIDE_TAU.as_secs_f32()) * ramp;
    Duration::from_secs_f32(tau)
}

/// How a follow move should travel.
///
/// Only the centred-playhead path ever eases: the bottom pin tracks text that is
/// still arriving, so smoothing it would leave the pane permanently behind the
/// words. Below the deadband nothing moves at all, and the first move after
/// Follow is switched on always lands, because it can be a screenful.
///
/// A `jump` is a move the user asked for — a timeline click or a seek — and
/// eases at any distance with [`jump_tau`]. A routine advance eases only within
/// [`FOLLOW_GLIDE_ROWS`] rows; a longer one nobody asked for lands. A jump still
/// in flight keeps the jump's shape when the playhead retargets it.
fn glide_shape(
    centred: bool,
    jump: bool,
    distance: f32,
    row_height: f32,
    snap_next: bool,
    jump_in_flight: bool,
) -> Glide {
    let distance = distance.abs();
    if !centred || snap_next || distance <= FOLLOW_DEADBAND {
        return Glide::Snap;
    }
    if jump || jump_in_flight {
        return Glide::Ease {
            tau: jump_tau(distance, row_height),
        };
    }
    if distance <= row_height * FOLLOW_GLIDE_ROWS {
        Glide::Ease {
            tau: FOLLOW_GLIDE_TAU,
        }
    } else {
        Glide::Snap
    }
}

pub struct App {
    output_dir: Option<PathBuf>,
    preset: Preset,
    theme_mode: AppTheme,
    license: license::State,
    provider: Box<dyn license::Provider>,
    paywall_open: bool,
    stage: String,
    /// The pipeline's stage id for the same event `stage` labels: `decoding`,
    /// `transcription`, `diarization`, `loading`, `done`. The label is what the
    /// status line prints; this is what the readout reasons about.
    stage_id: String,
    progress: f32,
    progress_shown: f32,
    overall_shown: f32,
    run_start: Option<Instant>,
    elapsed_final: Option<f64>,
    /// The readout's phase, and how much of it is left when it can be priced.
    eta_shown: Option<EtaView>,
    /// Audio seconds transcribed in the current file, and its duration.
    work_done: f64,
    work_total: f64,
    /// Wall clock plus audio seconds done, for measuring throughput.
    eta_samples: VecDeque<(Instant, f64)>,
    /// Median audio-seconds transcribed per wall-second.
    eta_throughput: Option<f32>,
    /// Throughput persisted across runs.
    eta_prior: Option<f32>,
    /// Fraction of the current file's diarization completed, 0..1.
    diarize_fraction: f32,
    /// Diarization wall seconds per audio second, persisted across runs.
    diarize_prior: Option<f32>,
    /// Post-ASR wall seconds per audio second, persisted across runs.
    prepare_prior: Option<f32>,
    /// Pending-file count at the previous tick, to notice a growing queue.
    eta_pending: usize,
    status: Status,
    /// The live transcript: what the current run wrote, and what the live file
    /// holds. With an item selected this is not what the screen shows — that is
    /// `selected_view`. Read through `document_transcript` / `shown_transcript`
    /// unless you mean this buffer specifically (writing its file, rebuilding
    /// its text after an edit).
    transcript: String,
    /// The live transcript's lines, on the same terms as `transcript`: read
    /// through `document_lines` / `shown_lines` unless the live buffer itself is
    /// meant.
    lines: Vec<DocLine>,
    pending_lines: VecDeque<DocLine>,
    reveal_accum: f32,
    /// The live transcript's speaker turns, on the same terms as `lines`.
    turns: Vec<backend::Turn>,
    peaks: Arc<Vec<f32>>,
    peaks_for: Option<PathBuf>,
    playhead: f32,
    timeline_duration: Option<f32>,
    player: Option<Player>,
    output_txt: Option<PathBuf>,
    /// Microphone chosen in Settings; `None` uses the system default.
    input_device: Option<String>,
    /// Capture in progress, if any.
    recording: Option<crate::recording::Recorder>,
    /// Lift quiet recordings before transcription.
    normalize: bool,
    /// Highlight words as playback reaches them.
    karaoke: bool,
    /// The briefing dialog: absent when closed.
    briefing: Option<BriefingState>,
    /// Word times for the line under the playhead, for karaoke highlighting.
    karaoke_line: Option<usize>,
    karaoke_source: Option<PathBuf>,
    karaoke_all: Vec<Vec<Word>>,
    karaoke_words: Vec<Word>,
    summary_open: bool,
    pending_rename: bool,
    completion_pending: bool,
    rename_open: bool,
    rename_fields: Vec<(String, String)>,
    /// Options of the most recent batch, so a resumed run can report what it
    /// was doing even if the settings were changed since.
    last_run: Option<RunRecord>,
    /// The custom-vocabulary editor: the two fields as typed, and whether the
    /// dialog is up.
    vocabulary_open: bool,
    vocab_terms: text_editor::Content,
    vocab_replacements: text_editor::Content,
    rename_focus: usize,
    sms_mode: bool,
    /// The player's waveform, speaker ribbon and legend are hidden. The
    /// transport row stays: the card can lose its scrubber, not its playback.
    player_collapsed: bool,
    notifications: bool,
    /// Calibration results and selected backend, keyed by preset arg.
    gpu_profiles: HashMap<String, GpuProfile>,
    /// Backend reported by the running pipeline.
    gpu_actual: Option<String>,
    /// Why the pipeline fell back to CPU.
    gpu_fallback: Option<String>,
    /// A GPU-backed run failed; retry the failed items once on CPU.
    gpu_retry_pending: bool,
    calibration: Option<Calibration>,
    calibration_events: Arc<Mutex<VecDeque<BenchmarkEvent>>>,
    show_timestamps: bool,
    num_speakers: Option<i32>,
    language: Option<String>,
    /// Derived orders, invalidated whenever the shown lines change.
    display_order_cache: RefCell<Option<Vec<usize>>>,
    speaker_order_cache: RefCell<Option<Vec<String>>>,
    transcript_viewport: f32,
    transcript_content: f32,
    transcript_offset: f32,
    transcript_at_bottom: bool,
    /// The transcript header's open menu, if any. One at a time.
    header_menu: Option<HeaderMenu>,
    /// The Reset warning dialog is showing.
    reset_confirm: bool,
    /// The Transcribe/Retranscribe setup dialog is showing.
    run_setup_open: bool,
    /// Show the Editing Tips card at the end of a run.
    editing_tips: bool,
    /// This completion still owes the tips card, before any pending rename.
    pending_tips: bool,
    transcript_dirty: bool,
    follow_offset: Option<f32>,
    /// Horizontal target of the last programmatic transcript scroll, used to
    /// distinguish it from a user scroll.
    scroll_target: Option<f32>,
    /// Last follow scroll we asked for and the offset reported then. Without
    /// it, a target the scrollable cannot reach (clamped, or computed from a
    /// content height that arrived a frame late) was re-issued every tick and
    /// the pane shimmered.
    follow_issue: Option<FollowIssue>,
    /// The follow scroll currently easing towards its target. It owns the
    /// in-flight marker (`scroll_target`) while it runs, and it is the only
    /// thing that issues `scroll_to` between its start and its end.
    scroll_anim: Option<ScrollAnim>,
    /// The next follow move must land: switching Follow on can put the pane a
    /// screen away from the playhead, which must not be eased across.
    snap_next_follow: bool,
    /// Height of the editor strip the scroll offset has already been shifted for.
    /// Kept here rather than derived from `line_edit` because one message can close
    /// and reopen the editor (moving it) without the strip ever leaving the screen.
    strip_height: f32,
    /// When the pane was last moved by a seek. Seeks closer together than
    /// [`FOLLOW_SEEK_QUIET`] are one scrub and must not each start a glide.
    last_seek: Option<Instant>,
    /// Row height used to centre the followed line. Re-derived only when the
    /// row count changes or the content height jumps materially, so streamed
    /// growth does not nudge the estimate on every frame.
    follow_row_height: RefCell<Option<(usize, f32)>>,
    /// Spacers the virtualised transcript window asked for last frame:
    /// (first line, end line, top spacer, bottom spacer). Used to measure how
    /// tall the rendered window really is.
    pane_window: Cell<(usize, usize, f32, f32)>,
    /// Latched row height for the transcript spacers.
    pane_row_height: Cell<Option<f32>>,
    /// First line of the virtualised window, kept while the view stays inside
    /// it so rows do not shift on every scroll event.
    pane_start: Cell<Option<usize>>,
    /// True after the user scroll-seeks the live transcript: the highlight then
    /// follows the playhead instead of the newest streamed line.
    scroll_seek: bool,
    font_scale: f32,
    follow_playhead: bool,
    /// Mark low-confidence utterances and expose the confidence filter.
    confidence_view: bool,
    console_open: bool,
    console_lines: Vec<String>,
    toast: Option<(String, Instant)>,
    help_open: Option<HelpKind>,
    settings_open: bool,
    /// Zero-based index of the visible empty-state feature slide.
    feature_slide: usize,
    /// When the current slide was shown; drives auto-advance.
    feature_slide_at: Instant,
    search_query: String,
    /// The Find row is showing. It is hidden until Ctrl+F, so the transcript
    /// card can reach the bottom of the pane.
    find_open: bool,
    /// Ctrl+F asked for the Find field before it was mounted; focus it on the
    /// next frame, once it has a widget and a layout.
    find_focus_pending: bool,
    /// The pointer is over the Find row. Its buttons are clicks outside the
    /// field and drop its focus, so the row must not vanish mid-click.
    find_hover: bool,
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
    /// Queue index currently being dragged, if any.
    queue_drag: Option<usize>,
    /// Queue index under the pointer, used for hover styling.
    queue_hover: Option<usize>,
    /// Pointer position inside the queue list while dragging.
    queue_pointer: Option<Point>,
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
    /// Display adjacent same-speaker lines as one block.
    join_lines: bool,
    joined_lines: Vec<DocLine>,
    joined_transcript: String,
    /// The current view differs from the default transcript file.
    modified: bool,
    /// Last line click, used to detect a double click.
    last_line_click: Option<(usize, Instant)>,
    /// Transcript line currently under the pointer.
    transcript_line_hover: Option<usize>,
    /// Transcript line open in the inline editor, if any.
    line_edit: Option<LineEdit>,
    /// Playback was running when the inline editor opened; Done resumes it.
    line_edit_resume: bool,
    /// A click point waiting for the editor to be laid out. The editor's text
    /// metrics are only known after its first frame, and `Action::Click` maps
    /// against them.
    line_edit_caret: Option<Point>,
    /// The speaker picker's own menu is open. Enter belongs to the menu then,
    /// not to the line being edited.
    /// A line delete is waiting on the confirmation dialog.
    delete_confirm: bool,
    /// Document the open editor writes to, so a view change cannot commit its
    /// text into a different transcript.
    line_edit_document: Option<usize>,
    /// Undo/redo for transcript edits made through the inline editor.
    line_history: LineHistory,
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

/// Default window: start at the minimum supported size (4:3).
fn default_window_size() -> Size {
    Size::new(MIN_W, MIN_H)
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
        let first_run = Self::is_first_run();
        let (queue, batch_paths) = if first_run {
            // Never restore a queue left behind by an older install or a
            // previous build. The first launch of the app starts clean.
            Self::clear_queue_files();
            (Vec::new(), Vec::new())
        } else {
            Self::load_queue_and_batch()
        };
        let now = SystemTime::now();
        let provider = license::provider();
        let pro_override = provider.override_tier();
        let mut license_state = license::State::load(
            &Self::license_path().unwrap_or_else(|| PathBuf::from("license.json")),
            now,
        );
        license_state.refresh(pro_override, now);
        // Free only offers the balanced model; a downgrade must never leave a
        // Pro-only preset selected.
        let mut preset = settings
            .preset
            .as_deref()
            .and_then(Preset::from_arg)
            .unwrap_or_default();
        if license_state.access.tier == license::Tier::Free {
            preset = Preset::Balanced;
        }
        let gpu_crashed = Self::gpu_guard_path().is_some_and(|path| path.exists());
        let mut app = Self {
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
            stage_id: "idle".to_string(),
            progress: 0.0,
            progress_shown: 0.0,
            overall_shown: 0.0,
            run_start: None,
            elapsed_final: None,
            eta_shown: None,
            work_done: 0.0,
            work_total: 0.0,
            eta_samples: VecDeque::new(),
            eta_throughput: None,
            eta_prior: settings.eta_throughput.and_then(App::usable_throughput),
            diarize_fraction: 0.0,
            diarize_prior: settings.eta_diarize.and_then(App::usable_diarize),
            prepare_prior: settings.eta_prepare.and_then(App::usable_prepare),
            eta_pending: 0,
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
            input_device: settings.input_device.clone(),
            normalize: settings.normalize,
            karaoke: settings.karaoke,
            recording: None,
            briefing: None,
            karaoke_line: None,
            karaoke_source: None,
            karaoke_all: Vec::new(),
            karaoke_words: Vec::new(),
            summary_open: false,
            pending_rename: false,
            completion_pending: false,
            rename_open: false,
            rename_fields: Vec::new(),
            last_run: Self::load_last_run(),
            vocabulary_open: false,
            vocab_terms: text_editor::Content::with_text(""),
            vocab_replacements: text_editor::Content::with_text(""),
            rename_focus: 0,
            sms_mode: settings.sms_mode.unwrap_or(false),
            player_collapsed: false,
            notifications: settings.notifications,
            gpu_profiles: settings.gpu_profiles.clone(),
            gpu_actual: None,
            gpu_fallback: None,
            gpu_retry_pending: false,
            calibration: None,
            calibration_events: Arc::new(Mutex::new(VecDeque::new())),
            show_timestamps: settings.show_timestamps,
            num_speakers: match settings.num_speakers {
                Some(1) => Some(1),
                // Everything else, including settings saved before the picker
                // became Single/Multiple, is a detect run.
                _ => None,
            },
            language: settings.language.clone(),
            display_order_cache: RefCell::new(None),
            speaker_order_cache: RefCell::new(None),
            transcript_viewport: 0.0,
            transcript_content: 0.0,
            transcript_offset: 0.0,
            transcript_at_bottom: true,
            header_menu: None,
            reset_confirm: false,
            run_setup_open: false,
            editing_tips: settings.editing_tips,
            pending_tips: false,
            transcript_dirty: false,
            follow_offset: None,
            scroll_target: None,
            follow_issue: None,
            scroll_anim: None,
            snap_next_follow: false,
            strip_height: 0.0,
            last_seek: None,
            follow_row_height: RefCell::new(None),
            pane_window: Cell::new((0, 0, 0.0, 0.0)),
            pane_row_height: Cell::new(None),
            pane_start: Cell::new(None),
            scroll_seek: false,
            font_scale: settings.font_scale.unwrap_or(DEFAULT_FONT_SCALE),
            follow_playhead: settings.follow_playhead.unwrap_or(true),
            // Low-confidence lines are marked from the moment a transcript with
            // confidence data is opened; the toggle is there to quiet them.
            confidence_view: false,
            console_open: false,
            console_lines: Vec::new(),
            toast: None,
            help_open: None,
            settings_open: false,
            feature_slide: 0,
            feature_slide_at: Instant::now(),
            waveform_zoom: settings.waveform_zoom.unwrap_or(1.0),
            search_query: String::new(),
            find_open: false,
            find_focus_pending: false,
            find_hover: false,
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
            queue_drag: None,
            queue_hover: None,
            queue_pointer: None,
            batch: false,
            batch_paths,
            current: None,
            current_index: None,
            selected: None,
            selected_view: None,
            view_error: None,
            join_lines: false,
            joined_lines: Vec::new(),
            joined_transcript: String::new(),
            modified: false,
            last_line_click: None,
            transcript_line_hover: None,
            line_edit: None,
            line_edit_resume: false,
            line_edit_caret: None,
            delete_confirm: false,
            line_edit_document: None,
            line_history: LineHistory::default(),
            backfill_active: false,
            waveform_tried: HashSet::new(),
            models: download::status(),
            model_download: None,
            model_error: None,
            model_events: Arc::new(Mutex::new(VecDeque::new())),
            model_cancel: Arc::new(AtomicBool::new(false)),
            last_model_check: Some(Instant::now()),
            drag_hover: false,
        };
        if first_run {
            // Establish the settings marker so the next launch restores the
            // queue instead of treating every launch as a first run.
            app.save_settings();
        }
        // Older builds stored "whisper model not found" as a GPU failure.
        // Once the models resolve, that is stale setup noise, not a backend
        // failure, and must not disable GPU for a new user.
        if app.models_ready() {
            let mut cleared = false;
            for profile in app.gpu_profiles.values_mut() {
                if profile
                    .gpu_error
                    .as_deref()
                    .is_some_and(|error| error.to_ascii_lowercase().contains("model not found"))
                {
                    profile.gpu_error = None;
                    profile.tested = false;
                    cleared = true;
                }
            }
            if cleared {
                app.save_settings();
            }
        }
        if gpu_crashed {
            for profile in app.gpu_profiles.values_mut() {
                profile.selected = false;
            }
            app.save_settings();
            Self::clear_gpu_guard();
            app.console_lines.push(
                "GPU acceleration was disabled after the previous run did not finish cleanly."
                    .to_string(),
            );
            app.toast = Some((
                "GPU acceleration disabled after an unclean shutdown".to_string(),
                Instant::now(),
            ));
        }
        app
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

    /// Marker for an in-flight GPU run. It is removed on a clean finish or an
    /// explicit cancel, so its presence on the next launch means the previous
    /// process crashed before it could fall back.
    fn gpu_guard_path() -> Option<PathBuf> {
        Self::settings_path().map(|path| path.with_file_name("gpu-run.lock"))
    }

    fn clear_gpu_guard() {
        if let Some(path) = Self::gpu_guard_path() {
            let _ = std::fs::remove_file(path);
        }
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
            num_speakers: self.num_speakers,
            language: self.language.clone(),
            input_device: self.input_device.clone(),
            notifications: self.notifications,
            editing_tips: self.editing_tips,
            normalize: self.normalize,
            karaoke: self.karaoke,
            gpu_profiles: self.gpu_profiles.clone(),
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
            eta_throughput: self.eta_prior,
            eta_diarize: self.diarize_prior,
            eta_prepare: self.prepare_prior,
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

    /// First launch means no settings file exists yet. The settings file is
    /// written immediately after construction so later launches restore the
    /// queue normally.
    fn is_first_run() -> bool {
        let current = Self::settings_path().is_some_and(|path| path.exists());
        let legacy = Self::legacy_settings_path().is_some_and(|path| path.exists());
        !current && !legacy
    }

    fn clear_queue_files() {
        for path in [Self::queue_path(), Self::legacy_queue_path()]
            .into_iter()
            .flatten()
        {
            let _ = std::fs::remove_file(path);
        }
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
                    // A run interrupted by a restart is resumable, and says so.
                    "running" | "interrupted" => ItemStatus::Interrupted,
                    "failed" => {
                        ItemStatus::Failed(record.message.unwrap_or_else(|| "failed".to_string()))
                    }
                    "done" => ItemStatus::Done,
                    _ => ItemStatus::Pending,
                };
                if matches!(status, ItemStatus::Pending | ItemStatus::Interrupted)
                    && !path.is_file()
                {
                    status = ItemStatus::Failed("file not found".to_string());
                }
                Some(QueueItem {
                    path,
                    audio,
                    status,
                    duration: record.duration,
                    transcript,
                    stats: record.stats,
                    turns: record.turns,
                    peaks: Arc::new(record.peaks),
                })
            })
            .collect();
        (items, batch_paths)
    }

    /// The options recorded for the last batch, read back independently of the
    /// queue rows so a restored queue keeps its resume context.
    fn load_last_run() -> Option<RunRecord> {
        let path = Self::queue_path()?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<QueueState>(&text).ok()?.last_run
    }

    fn save_queue(&self) {
        let Some(path) = Self::queue_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let state = QueueState {
            last_run: self.last_run.clone(),
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
                        ItemStatus::Interrupted => "interrupted",
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
                    turns: item.turns.clone(),
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
        if self.join_lines {
            self.rebuild_joined_preview();
        }
    }

    /// The document the screen is about, before the Join preview rewrites it:
    /// the selected queue item, or the live buffer when nothing is selected.
    ///
    /// Everything that means "the transcript on screen" starts here. The
    /// `shown_*` accessors add the Join preview on top; `document_*` is what the
    /// editor writes to.
    fn document_lines(&self) -> &[DocLine] {
        match &self.selected_view {
            Some(view) => &view.lines,
            None => &self.lines,
        }
    }

    fn document_turns(&self) -> &[backend::Turn] {
        match &self.selected_view {
            Some(view) => &view.turns,
            None => &self.turns,
        }
    }

    fn document_transcript(&self) -> &str {
        match &self.selected_view {
            Some(view) => &view.transcript,
            None => &self.transcript,
        }
    }

    fn shown_lines(&self) -> &[DocLine] {
        if self.join_lines {
            return &self.joined_lines;
        }
        self.document_lines()
    }

    fn shown_turns(&self) -> &[backend::Turn] {
        self.document_turns()
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
        if self.join_lines {
            return &self.joined_transcript;
        }
        self.document_transcript()
    }

    fn rebuild_joined_preview(&mut self) {
        self.joined_lines = join_adjacent(self.document_lines());
        self.joined_transcript = self
            .joined_lines
            .iter()
            .map(|line| line.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !self.joined_transcript.is_empty() {
            self.joined_transcript.push('\n');
        }
        self.display_order_cache.replace(None);
        self.speaker_order_cache.replace(None);
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
        self.can(license::Feature::Playback)
            && self.shown_audio().is_some()
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

    /// True while a completed transcript is on screen. Pro builds keep that view
    /// instead of snapping to the file that is being transcribed.
    fn viewing_completed_item(&self) -> bool {
        self.selected_view.is_some()
            && self
                .selected
                .and_then(|index| self.queue.get(index))
                .is_some_and(|item| matches!(item.status, ItemStatus::Done))
    }

    /// Pro builds keep a finished transcript on screen during a new run.
    fn keeps_finished_view(&self) -> bool {
        !self.shows(license::Audience::FreeOnly) && self.viewing_completed_item()
    }

    fn completed_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|item| matches!(item.status, ItemStatus::Done))
            .count()
    }

    /// A press on the line itself: the first click seeks, the second opens the
    /// editor. `point` is where inside the words the press landed, if it did;
    /// the editor then puts its caret there.
    fn line_pressed(&mut self, index: usize, point: Option<Point>) -> Task<Message> {
        self.close_find();
        // A press on the transcript dismisses an open header menu rather than
        // acting on the row underneath it. The header buttons are above the row
        // and still take their own presses, so one menu can switch to another.
        if self.header_menu.is_some() {
            self.header_menu = None;
            return Task::none();
        }
        let double = self.last_line_click.is_some_and(|(last, at)| {
            last == index && at.elapsed() <= Duration::from_millis(400)
        });
        if double {
            self.last_line_click = None;
            // Double-click always opens the inline editor; opening it turns
            // Follow off, so the pane stops moving under the pointer before the
            // first keystroke. Dispatch directly so the field is mounted before
            // the task loop resumes.
            return self.begin_inline_edit_at(index, point);
        }
        self.last_line_click = Some((index, Instant::now()));
        // A click on another utterance commits the one being edited and moves
        // the editor to the clicked line. Clicking the edited row itself never
        // reaches here: the field owns those clicks.
        if self.line_edit.as_ref().is_some_and(|edit| edit.index != index) {
            self.commit_inline_edit(false);
            return self.begin_inline_edit_at(index, point);
        }
        self.select_line_with(index, false)
    }

    fn select_line(&mut self, index: usize) -> Task<Message> {
        self.select_line_with(index, true)
    }

    /// Seek to `index`. `scroll` is false for a click on the line itself: it is
    /// already on screen, and scrolling it would move the row out from under a
    /// second click that is trying to open the editor.
    fn select_line_with(&mut self, index: usize, scroll: bool) -> Task<Message> {
        if !self.can(license::Feature::Playback) {
            return Task::none();
        }
        if let Some(line) = self.shown_lines().get(index) {
            if let Some(start) = line.start {
                let max = self.shown_duration().unwrap_or(start);
                self.playhead = start.clamp(0.0, max);
                self.follow_offset = None;
                self.follow_issue = None;
                self.scroll_anim = None;
                self.scroll_seek = scroll;
                if let Some(player) = &self.player {
                    player.seek(self.playhead);
                }
            }
        }
        Task::none()
    }

    /// Right-click a split line to cycle it through the transcript speakers.
    fn flip_line_speaker(&mut self, index: usize) -> Task<Message> {
        if self.join_lines {
            self.toast = Some(("Split the view to edit lines".to_string(), Instant::now()));
            return Task::none();
        }
        if !self.can(license::Feature::Editing) {
            self.toast = Some(("Editing is a Pro feature".to_string(), Instant::now()));
            return Task::none();
        }
        if self.shown_output().is_none() {
            self.toast = Some(("This transcript is read-only".to_string(), Instant::now()));
            return Task::none();
        }
        let speakers = self.speaker_order();
        if speakers.len() < 2 {
            self.toast = Some((
                "Only one speaker in this transcript".to_string(),
                Instant::now(),
            ));
            return Task::none();
        }
        // A swap lands on the same undo stack as a text edit, so an open editor
        // is committed first: its snapshot and the swap must not interleave.
        if self.line_edit.is_some() {
            self.commit_inline_edit(false);
        }
        let mut lines = self.edit_lines().to_vec();
        let Some(line) = lines.get(index) else {
            return Task::none();
        };
        let current = line.speaker.clone();
        let text = line_text(line);
        let position = speakers
            .iter()
            .position(|name| *name == current)
            .unwrap_or(0);
        let next = speakers[(position + 1) % speakers.len()].clone();

        self.modified = true;
        self.push_line_undo();
        if let Some(line) = lines.get_mut(index) {
            rewrite_edited_line(line, &next, &text);
        }
        self.replace_edit_lines(lines);
        self.toast = Some((format!("Speaker: {next}"), Instant::now()));
        Task::none()
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

    fn install_transcript_view(
        &mut self,
        index: usize,
        source: PathBuf,
        text: String,
        lines: Vec<DocLine>,
        output: Option<PathBuf>,
    ) -> Task<Message> {
        let Some(item) = self.queue.get(index) else {
            return Task::none();
        };
        let audio = item.audio.clone();
        let duration = item.duration.map(|seconds| seconds as f32);
        let mut turns = item.turns.clone();
        let peaks = item.peaks.clone();

        if turns.is_empty() {
            turns = lines
                .iter()
                .enumerate()
                .filter_map(|(line_index, line)| {
                    let start = line.start?;
                    if line.speaker.is_empty() {
                        return None;
                    }
                    let end = lines
                        .iter()
                        .skip(line_index + 1)
                        .find_map(|next| next.start)
                        .or(duration)
                        .unwrap_or(start + 2.0);
                    Some(backend::Turn {
                        start,
                        end: end.max(start + 0.2),
                        speaker: line.speaker.clone(),
                    })
                })
                .collect();
        }

        self.header_menu = None;
        self.close_find();
        self.view_error = None;
        self.join_lines = false;
        self.joined_lines.clear();
        self.joined_transcript.clear();
        self.modified = false;
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

        self.transcript_offset = 0.0;
        self.scroll_target = Some(0.0);
        // A freshly installed view has no line-editor history yet.
        self.line_history = LineHistory::default();
        let mut tasks = vec![scrollable::snap_to(
            transcript_id(),
            scrollable::RelativeOffset { x: 0.0, y: 0.0 },
        )];

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
                    {
                        let normalize = self.normalize;
                        async move {
                            backend::load_completed(audio, num_speakers, TIMELINE_BUCKETS, normalize)
                        }
                    },
                    move |(turns, probe)| {
                        Message::ViewDetailsLoaded(index, source.clone(), turns, probe)
                    },
                ));
            } else if needs_turns {
                tasks.push(Task::perform(
                    {
                        let normalize = self.normalize;
                        async move { backend::load_turns(&audio, num_speakers, normalize) }
                    },
                    move |turns| Message::ViewDetailsLoaded(index, source.clone(), turns, None),
                ));
            }
        }
        Task::batch(tasks)
    }

    fn open_item_view(
        &mut self,
        index: usize,
        preloaded: Option<(String, Option<PathBuf>)>,
    ) -> Task<Message> {
        // Changing what is on screen commits the open inline edit first: its
        // row belongs to the document being left.
        self.header_menu = None;
        self.close_find();
        self.commit_inline_edit(false);
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
            self.join_lines = false;
            self.joined_lines.clear();
            self.joined_transcript.clear();
            self.modified = false;
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
        self.scroll_anim = None;
        self.follow_issue = None;
        *self.follow_row_height.borrow_mut() = None;
        self.reset_pane_geometry();
        self.scroll_seek = false;
        self.transcript_viewport = 0.0;
        self.transcript_content = 0.0;
        self.transcript_offset = 0.0;
        self.transcript_at_bottom = true;

        let output = source
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("txt"))
            .unwrap_or(false)
            .then(|| source.clone());

        if let Some((text, output)) = preloaded {
            let confidences = backend::read_confidences(&source);
            return self.install_transcript_view(
                index,
                source,
                text.clone(),
                attach_confidences(parse_lines(&text), &confidences),
                output,
            );
        }
        let Some(text) = backend::read_transcript(&source) else {
            self.selected = Some(index);
            self.selected_view = Some(ItemView {
                index,
                source: source.clone(),
                transcript: String::new(),
                lines: Vec::new(),
                turns,
                peaks,
                duration,
                audio,
                output,
            });
            self.view_error = Some(format!("Could not read {}", source.display()));
            return Task::none();
        };
        let confidences = backend::read_confidences(&source);
        self.install_transcript_view(
            index,
            source,
            text.clone(),
            attach_confidences(parse_lines(&text), &confidences),
            output,
        )
    }

    /// Drop a pinned completed-item view and show the live run again.
    fn return_to_live(&mut self) {
        // The edited row belongs to the view being dropped.
        self.header_menu = None;
        self.close_find();
        self.commit_inline_edit(false);
        self.invalidate_derived();
        self.selected = None;
        self.selected_view = None;
        self.line_history = LineHistory::default();
        self.view_error = None;
        self.join_lines = false;
        self.joined_lines.clear();
        self.joined_transcript.clear();
        self.modified = false;
        self.player = None;
        self.playhead = 0.0;
        self.last_tick = None;
        self.follow_offset = None;
        self.scroll_target = None;
        self.scroll_anim = None;
        self.follow_issue = None;
        *self.follow_row_height.borrow_mut() = None;
        self.reset_pane_geometry();
        self.scroll_seek = false;
        self.transcript_offset = 0.0;
        self.search_query.clear();
        self.search_index = 0;
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

    /// Title-bar / taskbar icon decoded from the bundled PNG. It is the same
    /// artwork that build.rs embeds into the executable, so the window, the
    /// taskbar and Explorer all agree.
    fn window_icon() -> Option<iced::window::Icon> {
        #[cfg(windows)]
        const ICON: &[u8] = include_bytes!("../../icons/pencil-icon.png");
        #[cfg(not(windows))]
        const ICON: &[u8] = include_bytes!("../../icons/simple-hippo-square-icon.png");
        let image = image::load_from_memory(ICON).ok()?.into_rgba8();
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
            _ => default_window_size(),
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

    fn gpu_profile(&self) -> Option<&GpuProfile> {
        self.gpu_profiles.get(self.preset.arg())
    }

    fn gpu_selected(&self) -> bool {
        self.can(license::Feature::GpuAcceleration)
            && asr::gpu_available()
            && self.gpu_profile().is_some_and(|profile| profile.selected)
    }

    fn gpu_multiplier(&self) -> Option<f32> {
        self.gpu_profile().and_then(|profile| profile.multiplier)
    }

    fn gpu_unavailable_reason(&self) -> Option<String> {
        if !self.can(license::Feature::GpuAcceleration) {
            return Some("GPU transcription is a Pro feature.".to_string());
        }
        if !asr::gpu_available() {
            return Some("GPU unavailable in this build.".to_string());
        }
        if !self.models_ready() {
            return Some("Download the transcription models before testing GPU.".to_string());
        }
        self.gpu_profile()
            .and_then(|profile| profile.gpu_error.as_deref())
            .filter(|error| !stale_gpu_error(error, self.models_ready()))
            .map(str::to_string)
    }

    /// First-run prompt for a preset that has never been offered calibration.
    fn calibration_needed(&self) -> bool {
        self.can(license::Feature::GpuAcceleration)
            && asr::gpu_available()
            && self.models_ready()
            && self.gpu_unavailable_reason().is_none()
            && !self.gpu_profile().is_some_and(|profile| profile.tested)
    }

    /// True until a preset has a successful CPU-vs-GPU measurement. Selecting
    /// GPU after a skipped test must prompt again; only a real multiplier
    /// proves the backend works.
    fn gpu_test_needed(&self) -> bool {
        self.can(license::Feature::GpuAcceleration)
            && asr::gpu_available()
            && self.models_ready()
            && self.gpu_unavailable_reason().is_none()
            && !self
                .gpu_profile()
                .is_some_and(|profile| profile.multiplier.is_some())
    }

    fn open_calibration(&mut self, start_run: bool) {
        self.calibration = Some(Calibration::new(self.preset, start_run));
        self.calibration_events.lock().unwrap().clear();
    }

    fn start_calibration_test(&mut self) {
        let Some(calibration) = &mut self.calibration else {
            return;
        };
        if calibration.testing() {
            return;
        }
        calibration.phase = CalibrationPhase::TestingCpu;
        calibration.progress = 0.02;
        calibration.cpu_seconds = None;
        calibration.gpu_seconds = None;
        calibration.multiplier = None;
        calibration.error = None;
        calibration.backend_failed = false;
        let preset = calibration.preset;
        let events = self.calibration_events.clone();
        let Some(sample) = benchmark::sample_path() else {
            self.calibration_events
                .lock()
                .unwrap()
                .push_back(BenchmarkEvent::Failed {
                    message: "The bundled calibration sample is missing.".to_string(),
                });
            return;
        };
        std::thread::spawn(move || {
            let mut push = |event| {
                events.lock().unwrap().push_back(event);
            };
            if let Err(message) = benchmark::run(preset.arg(), &sample, &mut push) {
                push(BenchmarkEvent::Failed { message });
            }
        });
    }

    fn remember_skip(&mut self, preset: Preset) {
        let entry = self
            .gpu_profiles
            .entry(preset.arg().to_string())
            .or_default();
        entry.tested = true;
        entry.selected = false;
        self.save_settings();
    }

    fn choose_gpu(&mut self, selected: bool) {
        let Some(calibration) = &self.calibration else {
            return;
        };
        let preset = calibration.preset;
        let multiplier = calibration.multiplier;
        let error = calibration.error.clone();
        let entry = self
            .gpu_profiles
            .entry(preset.arg().to_string())
            .or_default();
        entry.tested = true;
        entry.selected = selected && error.is_none();
        if multiplier.is_some() {
            entry.multiplier = multiplier;
        }
        if selected && error.is_none() {
            entry.gpu_error = None;
        } else if error.is_some() {
            entry.gpu_error = error;
        }
        let run = calibration.start_run;
        self.calibration = None;
        self.save_settings();
        if run {
            self.begin_batch(false);
        }
    }

    fn drain_calibration(&mut self) {
        let events: Vec<BenchmarkEvent> = {
            let mut queue = self.calibration_events.lock().unwrap();
            queue.drain(..).collect()
        };
        for event in events {
            let Some(calibration) = &mut self.calibration else {
                continue;
            };
            match event {
                BenchmarkEvent::CpuStarted => {
                    calibration.phase = CalibrationPhase::TestingCpu;
                    calibration.progress = calibration.progress.max(0.02);
                }
                BenchmarkEvent::CpuFinished { seconds } => {
                    calibration.cpu_seconds = Some(seconds);
                    calibration.progress = 0.5;
                    calibration.phase = CalibrationPhase::TestingGpu;
                }
                BenchmarkEvent::GpuStarted => {
                    calibration.phase = CalibrationPhase::TestingGpu;
                    calibration.progress = calibration.progress.max(0.52);
                }
                BenchmarkEvent::GpuFinished { seconds } => {
                    calibration.gpu_seconds = Some(seconds);
                    calibration.progress = 1.0;
                    calibration.phase = CalibrationPhase::Result;
                    if let Some(cpu) = calibration.cpu_seconds {
                        if seconds > 0.0 {
                            calibration.multiplier = Some((cpu / seconds) as f32);
                        }
                    }
                }
                BenchmarkEvent::GpuFailed { message } => {
                    calibration.error = Some(message);
                    calibration.backend_failed = true;
                    calibration.phase = CalibrationPhase::Result;
                    calibration.progress = 1.0;
                }
                BenchmarkEvent::Failed { message } => {
                    calibration.error = Some(message);
                    calibration.backend_failed = false;
                    calibration.phase = CalibrationPhase::Result;
                    calibration.progress = 1.0;
                }
            }
        }
        if let Some(calibration) = &mut self.calibration {
            if calibration.phase == CalibrationPhase::Result && !calibration.recorded {
                calibration.recorded = true;
                let preset = calibration.preset;
                let multiplier = calibration.multiplier;
                let error = calibration.error.clone();
                let backend_failed = calibration.backend_failed;
                let entry = self
                    .gpu_profiles
                    .entry(preset.arg().to_string())
                    .or_default();
                if backend_failed {
                    // A real GPU failure is worth remembering until retest.
                    entry.tested = true;
                    entry.selected = false;
                    entry.gpu_error = error;
                } else if error.is_none() {
                    entry.tested = true;
                    entry.selected = false;
                    entry.gpu_error = None;
                    if multiplier.is_some() {
                        entry.multiplier = multiplier;
                    }
                } else {
                    // Missing models or a bad sample are setup failures, not
                    // evidence that this GPU is broken. Leave the profile
                    // unmeasured so the next selection can retest.
                    entry.selected = false;
                }
                self.save_settings();
            }
        }
    }

    /// True when a UI element tagged for `audience` belongs on screen.
    fn shows(&self, audience: license::Audience) -> bool {
        audience.visible_for(self.license.access.tier)
    }

    /// Tag a widget with its tier: rendered for the matching audience,
    /// otherwise collapsed to nothing.
    fn tagged<'a>(
        &'a self,
        audience: license::Audience,
        content: impl Into<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        if self.shows(audience) {
            content.into()
        } else {
            container(horizontal_space())
                .width(Length::Fixed(0.0))
                .height(Length::Fixed(0.0))
                .into()
        }
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
        if self.preset != Preset::Balanced && !self.can(license::Feature::SpeedPreset) {
            self.preset = Preset::Balanced;
            self.save_settings();
        }
        if !self.can(license::Feature::Playback) {
            self.player = None;
        }
    }

    /// Header control showing the current tier and opening the paywall.
    fn license_button<'a>(&'a self, scheme: &Scheme) -> Element<'a, Message> {
        let label = match self.license.access.tier {
            license::Tier::Pro if self.license.access.source == license::Source::Trial => {
                "Pro trial"
            }
            license::Tier::Pro => "Pro",
            license::Tier::Free => "Get Pro",
        };
        let button: Element<Message> = if self.shows(license::Audience::ProOnly) {
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
            "Speed and Accuracy presets, speaker labels, batch, editing and playback",
            scheme,
        )
    }

    /// Development-only Free/Pro switch shown in the header.
    fn tier_toggle_button<'a>(&'a self, scheme: &Scheme) -> Element<'a, Message> {
        let label = if self.shows(license::Audience::ProOnly) {
            "Use Free"
        } else {
            "Use Pro"
        };
        self.tip(
            action_text(label)
                .on_press(Message::ToggleTier)
                .style(pill_button_secondary),
            "Development: switch between the Free and Pro versions",
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
                    icon_colored(ICON_CHECK, 14.0, scheme.green),
                    text(feature.label()).size(theme::TEXT_SM),
                ]
                .spacing(6),
            );
        }

        body = body.push(
            text("Free version includes balanced model, unlimited, full length transcriptions")
                .size(theme::TEXT_XS)
                .color(scheme.muted),
        );

        let mut actions = row![].spacing(8).align_y(iced::Alignment::Center);
        let mut secondary = row![].spacing(8).align_y(iced::Alignment::Center);
        let mut has_secondary = false;
        if self.shows(license::Audience::ProOnly) {
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
                // Store purchasing arrives in 1.1. Say what Pro is and let the
                // trial do the selling, instead of apologising for a dead button.
                actions = actions.push(
                    text("Pro arrives in 1.1. The 7-day trial shows what it adds.")
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
            // Keep restore on its own line so the label never wraps.
            secondary = secondary.push(
                action_text("Restore purchase")
                    .on_press(Message::RestorePro)
                    .style(pill_button_secondary),
            );
            has_secondary = true;
        }

        body = body.push(
            container(actions)
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
        );
        if has_secondary {
            body = body.push(
                container(secondary)
                    .width(Length::Fill)
                    .align_x(iced::Alignment::End),
            );
        }
        body = body.push(
            container(
                row![
                    horizontal_space(),
                    action_text("Close")
                        .on_press(Message::ClosePaywall)
                        .style(pill_button_secondary)
                ]
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
        let ids = download::required_for(self.preset.arg(), self.pipelines_diarize());
        self.start_model_download_for(ids);
    }

    /// Download an explicit set of models; the briefing model is fetched on
    /// first use rather than with every preset.
    fn start_model_download_for(&mut self, ids: Vec<&'static str>) {
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

    /// Idle views do not need a 10 Hz rebuild. Only subscribe to the tick while
    /// something is actually animating or draining a background queue.
    fn needs_ticks(&self) -> bool {
        matches!(self.status, Status::Running)
            || self.recording.is_some()
            || self.player.as_ref().is_some_and(Player::is_playing)
            || self.toast.is_some()
            || self.model_download.is_some()
            || self.calibration.is_some()
            || self.window_dirty
            || self.feature_deck_visible()
            || self.completion_pending
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // Karaoke word boundaries are finer than a 100 ms step; only tighten
        // the clock while it is actually moving.
        let tick_ms = if self.karaoke && self.player.as_ref().is_some_and(Player::is_playing) {
            50
        } else {
            100
        };
        let tick = if self.needs_ticks() {
            iced::time::every(Duration::from_millis(tick_ms)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // The glide is the only frame-driven animation in the app, and placing
        // a caret or focusing the Find field needs exactly one frame of layout.
        // Subscribing unconditionally would keep a 60 Hz redraw loop (and,
        // through each step's scroll echo, two view builds per frame) running
        // while nothing moves.
        let frames = if self.scroll_anim.is_some()
            || self.line_edit_caret.is_some()
            || self.find_focus_pending
        {
            iced::window::frames().map(Message::Frame)
        } else {
            Subscription::none()
        };
        // The Find row closes when its field loses focus, and iced has no blur
        // message: poll while it is open. One probe every 120 ms while the row
        // is showing is cheap, and the row is short-lived.
        let find_focus = if self.find_open && !self.find_focus_pending {
            iced::time::every(Duration::from_millis(120)).map(|_| Message::PollFindFocus)
        } else {
            Subscription::none()
        };
        Subscription::batch([
            tick,
            frames,
            find_focus,
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
            // Join the edited line with its neighbour. Without an open editor
            // the actions below are no-ops, so they need no extra guard here.
            Key::Named(Named::ArrowUp) if modifiers.alt() => Some(Message::MergePrevLine),
            Key::Named(Named::ArrowDown) if modifiers.alt() => Some(Message::MergeNextLine),
            Key::Named(Named::ArrowUp) => Some(Message::SelectRelative(-1)),
            Key::Named(Named::ArrowDown) => Some(Message::SelectRelative(1)),
            Key::Named(Named::ArrowRight) => Some(Message::SeekBy(5.0)),
            Key::Named(Named::ArrowLeft) => Some(Message::SeekBy(-5.0)),
            // Single-key View toggles. Never fires while something is being
            // typed into: the focus probe in the handler decides that.
            Key::Character(character)
                if !modifiers.command() && !modifiers.alt() && !modifiers.shift() =>
            {
                match character.to_ascii_lowercase().as_str() {
                    "f" => Some(Message::ViewShortcut(ViewShortcut::Follow)),
                    "t" => Some(Message::ViewShortcut(ViewShortcut::Timestamps)),
                    "s" => Some(Message::ViewShortcut(ViewShortcut::Sms)),
                    "j" => Some(Message::ViewShortcut(ViewShortcut::Join)),
                    "k" => Some(Message::ViewShortcut(ViewShortcut::Karaoke)),
                    "c" => Some(Message::ViewShortcut(ViewShortcut::Confidence)),
                    _ => None,
                }
            }
            Key::Character(character) if modifiers.command() => match character.as_str() {
                "," => Some(Message::OpenSettings),
                "o" => Some(Message::AddFiles),
                "s" => Some(Message::SaveTranscriptAs),
                "f" => Some(Message::FocusFind),
                "k" | "K" if modifiers.shift() => Some(Message::SplitLineAtCursor),
                "=" | "+" => Some(Message::ZoomIn),
                "-" | "_" => Some(Message::ZoomOut),
                "0" => Some(Message::ZoomReset),
                _ => None,
            },
            _ => None,
        }
    }

    /// A dialog or an open picker owns the keyboard, so a line-edit shortcut
    /// that arrives from the global key subscription must not fire behind it.
    fn line_edit_shortcut_blocked(&self) -> bool {
        self.menu_open() || self.popup_open()
    }

    /// A single-key View toggle is off while a dialog owns the keyboard or the
    /// line editor is open: those fields would otherwise swallow the letter's
    /// meaning.
    fn view_shortcut_blocked(&self) -> bool {
        self.line_edit_shortcut_blocked() || self.line_edit.is_some()
    }

    /// Ask the widget tree whether a focusable input currently owns focus.
    fn focus_probe() -> Task<bool> {
        iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::count())
            .map(|count| count.focused.is_some())
    }

    /// Give the open line editor the keyboard: focus the first focusable
    /// widget, which is the editor whenever it is mounted, and clear every
    /// other focus (the Find field, in practice). One pass, so the two cannot
    /// race.
    fn focus_line_editor() -> Task<Message> {
        iced::advanced::widget::operate(focus_first::<Message>())
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.dispatch(message);
        // Karaoke must follow a seek, a pause or a line change, not only the
        // tick, so it is refreshed at the message boundary.
        self.refresh_karaoke();
        // A menu takes the pointer, but the remembered row is left alone:
        // `pointer_blocked` already hides its affordances while the menu is up,
        // and clearing it meant that after the menu closed nothing re-entered
        // the row -- the row's `on_enter` does not fire again while the pointer
        // stays inside it -- so the swap button stayed gone until the pointer
        // left and came back.
        // A dialog takes the keyboard: the Find field has lost focus even when a
        // dialog input has taken it next.
        if self.popup_open() && self.find_open {
            self.close_find();
        }
        // The editor strip changes the height of the list, and that has to move the
        // scroll offset in the same update as the layout, before the frame is drawn.
        match self.strip_scroll() {
            Some(scroll) => Task::batch([scroll, task]),
            None => task,
        }
    }

    /// Handles one message. [`Self::update`] wraps this with the scroll the editor
    /// strip's appearance or disappearance needs.
    fn dispatch(&mut self, message: Message) -> Task<Message> {
        // A dialog owns the pointer, so nothing behind its scrim may keep a
        // pointer-shaped state alive. Clearing it here rather than at each of
        // the nine open paths is what makes that true for all of them, and it
        // stops a picker that was unmounted mid-open from blocking hover after
        // the dialog closes.
        if self.open_modal().is_some() {
            self.transcript_line_hover = None;
            self.queue_hover = None;
        }
        match message {
            Message::TogglePlayerCollapsed => {
                self.player_collapsed = !self.player_collapsed;
                Task::none()
            }
            Message::SmsToggled(value) => {
                // Bubbles and lines are laid out differently: keep the edited
                // row as a line and commit it before the view changes shape.
                self.commit_inline_edit(false);
                self.sms_mode = value;
                // Bubble rows are a different shape: re-measure the spacers.
                self.reset_pane_geometry();
                self.save_settings();
                Task::none()
            }
            Message::NotificationsToggled(value) => {
                self.notifications = value;
                self.save_settings();
                Task::none()
            }
            Message::NormalizeToggled(value) => {
                self.normalize = value;
                self.save_settings();
                Task::none()
            }
            Message::KaraokeToggled(value) => {
                self.karaoke = value;
                self.karaoke_source = None;
                self.save_settings();
                Task::none()
            }
            Message::SetAcceleration(value) => {
                if matches!(self.status, Status::Running) {
                    return Task::none();
                }
                if !value {
                    let preset = self.preset.arg().to_string();
                    let entry = self.gpu_profiles.entry(preset).or_default();
                    entry.selected = false;
                    Self::clear_gpu_guard();
                    self.save_settings();
                    return Task::none();
                }
                if !self.can(license::Feature::GpuAcceleration) {
                    self.paywall_open = true;
                    return Task::none();
                }
                if !asr::gpu_available() {
                    self.toast =
                        Some(("GPU unavailable in this build".to_string(), Instant::now()));
                    return Task::none();
                }
                if let Some(reason) = self.gpu_unavailable_reason() {
                    self.toast = Some((reason, Instant::now()));
                    return Task::none();
                }
                let needs_test = self.gpu_test_needed();
                let preset = self.preset.arg().to_string();
                let entry = self.gpu_profiles.entry(preset).or_default();
                entry.selected = true;
                self.save_settings();
                if needs_test {
                    self.open_calibration(false);
                }
                Task::none()
            }
            Message::TestGpu => {
                self.start_calibration_test();
                Task::none()
            }
            Message::UseCpuForRun => {
                if let Some(calibration) = &self.calibration {
                    let preset = calibration.preset;
                    let run = calibration.start_run;
                    self.calibration = None;
                    self.remember_skip(preset);
                    if run {
                        self.begin_batch(false);
                    }
                }
                Task::none()
            }
            Message::UseGpuForRun => {
                self.choose_gpu(true);
                Task::none()
            }
            Message::CloseCalibration => {
                let Some(calibration) = &self.calibration else {
                    return Task::none();
                };
                let testing = calibration.testing();
                let preset = calibration.preset;
                let run = calibration.start_run;
                self.calibration = None;
                self.remember_skip(preset);
                // Closing during a test cancels that attempt. Starting a run
                // while the benchmark thread is still winding down would
                // compete for the same CPU/GPU.
                if run && !testing {
                    self.begin_batch(false);
                }
                Task::none()
            }
            Message::RetestGpu => {
                if matches!(self.status, Status::Running)
                    || !self.can(license::Feature::GpuAcceleration)
                {
                    return Task::none();
                }
                if !asr::gpu_available() {
                    self.toast =
                        Some(("GPU unavailable in this build".to_string(), Instant::now()));
                    return Task::none();
                }
                self.gpu_profiles.remove(self.preset.arg());
                self.open_calibration(false);
                self.start_calibration_test();
                Task::none()
            }
            Message::LockedGpu => {
                self.paywall_open = true;
                Task::none()
            }
            Message::OpenLogFolder => {
                let dir = hippo_transcribe_core::logging::log_dir();
                let _ = std::fs::create_dir_all(&dir);
                // Failures here are cosmetic: the path is still shown in help.
                #[cfg(windows)]
                let _ = std::process::Command::new("explorer.exe").arg(&dir).spawn();
                #[cfg(not(windows))]
                let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                Task::none()
            }
            Message::TimestampsToggled(value) => {
                self.show_timestamps = value;
                self.save_settings();
                Task::none()
            }
            Message::TranscriptLeft => {
                // Only the card's own menus close when the pointer leaves the
                // card; Help's menu hangs from the app header and has nothing to
                // do with where the pointer is. A dialog owns the pointer: an
                // open menu is already hidden behind its scrim, so leave it.
                if !self.popup_open()
                    && matches!(
                        self.header_menu,
                        Some(HeaderMenu::File | HeaderMenu::Edit | HeaderMenu::View)
                    )
                {
                    self.header_menu = None;
                }
                Task::none()
            }
            Message::ToggleHeaderMenu(menu) => {
                if self.popup_open() {
                    return Task::none();
                }
                self.header_menu = if self.header_menu == Some(menu) {
                    None
                } else {
                    Some(menu)
                };
                Task::none()
            }
            Message::CloseHeaderMenu => {
                self.header_menu = None;
                Task::none()
            }
            Message::OpenRunSetup => {
                if !matches!(self.status, Status::Running) && !self.queue.is_empty() {
                    self.header_menu = None;
                    self.run_setup_open = true;
                }
                Task::none()
            }
            Message::CloseRunSetup => {
                self.run_setup_open = false;
                Task::none()
            }
            Message::ConfirmRunSetup => {
                self.run_setup_open = false;
                self.start();
                Task::none()
            }
            Message::RequestResetTranscript => {
                self.header_menu = None;
                if self.reset_available() {
                    self.reset_confirm = true;
                }
                Task::none()
            }
            Message::ConfirmResetTranscript => {
                self.reset_confirm = false;
                self.reset_transcript()
            }
            Message::CloseResetConfirm => {
                self.reset_confirm = false;
                Task::none()
            }
            Message::ToggleJoinLines => {
                if self.shown_lines().is_empty() {
                    return Task::none();
                }
                // Joined lines cannot be edited, so an open editor is committed
                // before the transcript is replaced by the joined preview.
                self.commit_inline_edit(false);
                self.join_lines = !self.join_lines;
                self.modified = true;
                // Joined rows are different rows: the target a glide was aiming
                // at no longer exists.
                self.scroll_anim = None;
                self.reset_pane_geometry();
                if self.join_lines {
                    self.rebuild_joined_preview();
                } else {
                    self.invalidate_derived();
                }
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
                let previous_content = self.transcript_content;
                self.transcript_viewport = viewport.bounds().height;
                self.transcript_content = viewport.content_bounds().height;
                let offset = viewport.absolute_offset().y;
                self.transcript_offset = offset;
                let viewport_h = viewport.bounds().height;
                let content_h = viewport.content_bounds().height;

                // A glide owns the in-flight marker while it runs, so a report
                // that lands between its start and its target must not consume
                // it; the marker is compared below and released when the glide
                // lands.
                let ours = if self.scroll_anim.is_some() {
                    self.glide_covers(offset)
                } else {
                    self.scroll_target
                        .take()
                        .is_some_and(|target| (offset - target).abs() <= 6.0)
                };

                // "At the bottom" is where a running glide is heading, not
                // where it happens to be: on the way down it passes through the
                // middle, which would read as having been scrolled away.
                self.transcript_at_bottom = match self.scroll_anim {
                    Some(anim) => anim.target + viewport_h >= content_h - 8.0,
                    None => offset + viewport_h >= content_h - 8.0,
                };

                if !ours {
                    // Rows move under a stationary pointer while scrolling, and
                    // a virtualized row that mounts under the cursor never fires
                    // enter. Drop the remembered row rather than lighting up
                    // whatever now sits where it used to. A glide moves rows
                    // every frame, so it must not clear the hover: the row's
                    // affordances would strobe for the length of the move.
                    self.transcript_line_hover = None;
                    if let Some(anim) = self.scroll_anim.take() {
                        // The user took the pane. Keep the expectation so the
                        // ordinary 12 px test below can see it was them, which
                        // turns Follow off rather than snapping back.
                        self.follow_offset = Some(anim.expected);
                        self.scroll_target = None;
                    }
                }

                // Our own scroll landing (or close to it) needs no follow-up.
                if ours {
                    return Task::none();
                }
                match self.correct_follow_offset(previous_content, offset, viewport_h, content_h) {
                    Some(y) => scrollable::scroll_to(
                        transcript_id(),
                        scrollable::AbsoluteOffset { x: 0.0, y },
                    ),
                    None => Task::none(),
                }
            }
            Message::WindowEvent(event) => {
                match event {
                    iced::window::Event::Resized(size) => {
                        // A resize re-wraps the transcript, so the target the
                        // glide was aiming at no longer exists.
                        self.scroll_anim = None;
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
                        return self.added_tasks(added);
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
            Message::RetryItem(index) => {
                if matches!(self.status, Status::Running) {
                    return Task::none();
                }
                let retried = self.queue.get_mut(index).is_some_and(|item| {
                    if matches!(item.status, ItemStatus::Failed(_) | ItemStatus::Interrupted) {
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
            Message::QueueDragStart(index) => {
                if self.pointer_blocked() || index >= self.queue.len() {
                    return Task::none();
                }
                if !matches!(self.status, Status::Running) {
                    self.queue_drag = Some(index);
                    return Task::none();
                }
                // Reordering stays locked during a run, but a click on a
                // finished row opens it again in Pro.
                let Some(item) = self.queue.get(index) else {
                    return Task::none();
                };
                let done = matches!(item.status, ItemStatus::Done);
                let has_transcript = item.transcript.is_some();
                let running_row = matches!(item.status, ItemStatus::Running);
                let locked_previous =
                    self.shows(license::Audience::FreeOnly) && self.current_index != Some(index);
                if running_row {
                    // From a pinned transcript, clicking the row that is being
                    // transcribed returns to the live view.
                    return self.view_item(index);
                }
                if done && locked_previous {
                    self.toast = Some((
                        "Reviewing during active transcription is a Pro feature".to_string(),
                        Instant::now(),
                    ));
                } else if done && has_transcript && !self.shows(license::Audience::FreeOnly) {
                    return self.view_item(index);
                }
                Task::none()
            }
            Message::QueueDrop(index) => {
                let Some(from) = self.queue_drag.take() else {
                    return Task::none();
                };
                self.queue_hover = None;
                self.queue_pointer = None;
                if index >= self.queue.len() {
                    return Task::none();
                }
                if from != index {
                    self.move_queue_item(from, index);
                    // Dropping moves focus to the item in its new position.
                    return self.view_item(index);
                }
                let Some(item) = self.queue.get(index) else {
                    return Task::none();
                };
                let running = matches!(self.status, Status::Running);
                let locked_previous = self.shows(license::Audience::FreeOnly)
                    && running
                    && matches!(item.status, ItemStatus::Done)
                    && self.current_index != Some(index);
                let selectable = !locked_previous
                    && (matches!(item.status, ItemStatus::Running)
                        || (!running && matches!(item.status, ItemStatus::Pending))
                        || (matches!(item.status, ItemStatus::Done) && item.transcript.is_some()));
                if locked_previous {
                    self.toast = Some((
                        "Reviewing during active transcription is a Pro feature".to_string(),
                        Instant::now(),
                    ));
                } else if selectable {
                    return self.view_item(index);
                }
                Task::none()
            }
            Message::QueueHover(index) => {
                // A dialog owns the pointer: a row under the scrim must not
                // light up as the pointer crosses it. Leaving is always allowed
                // through, or a hover set before the dialog opened could never
                // be cleared.
                if index.is_some() && self.pointer_blocked() {
                    return Task::none();
                }
                self.queue_hover = index;
                Task::none()
            }
            Message::QueuePointerMoved(point) => {
                if self.queue_drag.is_some() {
                    self.queue_pointer = Some(point);
                }
                Task::none()
            }
            Message::QueuePointerLeft => {
                self.queue_pointer = None;
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
                if !self.can(license::Feature::Playback) {
                    return Task::none();
                }
                let max = self.shown_duration().unwrap_or(0.0).max(0.0);
                self.playhead = seconds.clamp(0.0, max);
                self.last_tick = Some(Instant::now());
                if let Some(player) = &self.player {
                    player.seek(self.playhead);
                }
                // Scrubbing the timeline jumps the transcript to that moment.
                // A click eases there; a drag (which arrives as a stream of
                // seeks) lands at once.
                self.scroll_seek = true;
                match self.line_for_playhead(self.playhead) {
                    Some(line) => self.seek_to_line(line),
                    None => Task::none(),
                }
            }
            Message::PlayerToggle => self.toggle_playback(),
            Message::FocusFind => {
                if self.popup_open() {
                    return Task::none();
                }
                self.header_menu = None;
                self.find_open = true;
                self.find_focus_pending = true;
                self.find_hover = false;
                Task::none()
            }
            Message::SearchChanged(query) => {
                self.search_query = query;
                self.search_index = 0;
                Task::none()
            }
            Message::FindNext => {
                let matches = self.matching_lines();
                if !matches.is_empty() {
                    self.search_index = (self.search_index + 1) % matches.len();
                    return Task::batch([
                        self.scroll_to_line(matches[self.search_index]),
                        text_input::focus(find_id()),
                    ]);
                }
                Task::none()
            }
            Message::FindPrev => {
                let matches = self.matching_lines();
                if !matches.is_empty() {
                    self.search_index = (self.search_index + matches.len() - 1) % matches.len();
                    return Task::batch([
                        self.scroll_to_line(matches[self.search_index]),
                        text_input::focus(find_id()),
                    ]);
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
            Message::SelectLine(index) => self.select_line(index),
            Message::LineClicked(index) => self.line_pressed(index, None),
            Message::LineTextPressed(index, point) => self.line_pressed(index, Some(point)),
            Message::CycleLineSpeaker => {
                let order = self.speaker_order();
                if order.len() < 2 {
                    return Task::none();
                }
                if let Some(edit) = self.line_edit.as_mut() {
                    let position = edit.current_position();
                    if let Some(row) = edit.rows.get_mut(position) {
                        let at = order.iter().position(|name| *name == row.speaker).unwrap_or(0);
                        let next = order[(at + 1) % order.len()].clone();
                        row.speaker = next.clone();
                        self.toast = Some((format!("Speaker: {next}"), Instant::now()));
                    }
                    // The press on the chip landed outside the editor, which
                    // made the editor drop focus; take it back so typing can
                    // continue.
                    return Self::focus_line_editor();
                }
                Task::none()
            }
            Message::AddLineSpeaker => {
                if self.line_edit.is_none() {
                    return Task::none();
                }
                let name = next_speaker_name(&self.speaker_order());
                if let Some(edit) = self.line_edit.as_mut() {
                    let position = edit.current_position();
                    if let Some(row) = edit.rows.get_mut(position) {
                        row.speaker = name.clone();
                        self.toast = Some((format!("Speaker: {name}"), Instant::now()));
                    }
                }
                // The new name lives in the row, not the document, so the order
                // the chip draws from has to learn it before the next frame or
                // the colour and the cycle would both miss it.
                self.invalidate_derived();
                // Like the chip: the press landed outside the field, so hand
                // the keyboard back and typing can continue.
                Self::focus_line_editor()
            }
            Message::LineHover(index) => {
                // As with the transcript chrome: no per-line affordances while
                // a dialog or a menu owns the pointer.
                if self.pointer_blocked() {
                    return Task::none();
                }
                self.transcript_line_hover = index;
                Task::none()
            }
            // An exit names the row it left, so it cannot clear a hover that a
            // newer enter already set. Column children are visited in index
            // order and both messages land in the same update when the pointer
            // moves up a row: the entering row's `Some(i)` was being overwritten
            // by the leaving row's `None`, and the row's own `on_enter` never
            // fires again while the pointer stays inside it.
            Message::LineUnhover(index) => {
                if self.transcript_line_hover == Some(index) {
                    self.transcript_line_hover = None;
                }
                Task::none()
            }
            Message::SelectRelative(delta) => {
                if self.rename_open
                    || self.vocabulary_open
                    || self.briefing.is_some()
                    || self.line_edit.is_some()
                    || self.settings_open
                    || self.calibration.is_some()
                {
                    return Task::none();
                }
                Self::focus_probe()
                    .map(move |focused| Message::SelectRelativeChecked(delta, focused))
            }
            Message::SelectRelativeChecked(delta, focused) => {
                if focused
                    || self.rename_open
                    || self.vocabulary_open
                    || self.briefing.is_some()
                    || self.line_edit.is_some()
                    || self.settings_open
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
                if self.rename_open
                    || self.vocabulary_open
                    || self.briefing.is_some()
                    || self.settings_open
                    || self.calibration.is_some()
                {
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
                // Tab in the vocabulary editor walks the dialog's focusables;
                // text_editor has no id in iced 0.13, so the field order is the
                // widget order.
                if self.vocabulary_open {
                    return if forward {
                        iced::widget::focus_next::<Message>()
                    } else {
                        iced::widget::focus_previous::<Message>()
                    };
                }
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
                if self.settings_open || self.vocabulary_open {
                    return Task::none();
                }
                // Enter over an open list or a dialog belongs to that widget,
                // not to the line being edited underneath it.
                if self.line_edit_shortcut_blocked() {
                    return Task::none();
                }
                if self.line_edit.is_some() {
                    // Enter commits the inline editor, as Done does.
                    self.commit_inline_edit(true);
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
                if self.settings_open || self.vocabulary_open {
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
                self.scroll_anim = None;
                self.reset_pane_geometry();
                self.save_settings();
                Task::none()
            }
            Message::ZoomOut => {
                self.font_scale = (self.font_scale - 0.1).max(0.6);
                self.scroll_anim = None;
                self.reset_pane_geometry();
                self.save_settings();
                Task::none()
            }
            Message::ZoomReset => {
                self.font_scale = DEFAULT_FONT_SCALE;
                self.scroll_anim = None;
                self.reset_pane_geometry();
                self.save_settings();
                Task::none()
            }
            Message::ToggleConfidenceView => {
                self.confidence_view = !self.confidence_view;
                Task::none()
            }
            Message::ToggleFollowPlayhead => {
                let turning_off = self.follow_playhead;
                let turning_on = !turning_off;
                self.follow_playhead = turning_on;
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
                // A glide belongs to the mode it started in. Switching Follow
                // off finishes its move so the line does not freeze off centre;
                // switching it on must not inherit one.
                let stop = self.stop_glide(turning_off);
                if turning_off {
                    self.snap_next_follow = false;
                }
                self.follow_offset = None;
                self.scroll_target = None;
                self.follow_issue = None;
                self.scroll_seek = false;
                self.save_settings();
                if turning_on {
                    // Same rules as the tick path: follow the playhead line,
                    // never the first-row fallback, and never to the top.
                    let active = self.follow_line_index();
                    match self.next_follow_target(active) {
                        Some(target) => {
                            // Enabling Follow is a jump, not a glide: the pane
                            // can be a screen away.
                            self.scroll_anim = None;
                            self.scroll_target = Some(target.y);
                            self.note_follow_issue(target.y, active);
                            return scrollable::scroll_to(
                                transcript_id(),
                                scrollable::AbsoluteOffset { x: 0.0, y: target.y },
                            );
                        }
                        None => {
                            // Nothing to scroll to yet, so the next move that
                            // does happen is the engage move and must land.
                            self.snap_next_follow = true;
                        }
                    }
                }
                stop
            }
            Message::SetTheme(mode) => {
                self.theme_mode = mode;
                self.save_settings();
                Task::none()
            }
            Message::OpenSettings => {
                self.header_menu = None;
                self.settings_open = true;
                self.help_open = None;
                self.toast = None;
                Task::none()
            }
            Message::OpenPaywall => {
                self.paywall_open = true;
                Task::none()
            }
            Message::LockedPreset => {
                self.toast = Some((
                    "Speed and Accuracy presets are Pro features".to_string(),
                    Instant::now(),
                ));
                Task::none()
            }
            Message::FeatureAdvance => {
                if self.feature_deck_visible() {
                    self.feature_slide = (self.feature_slide + 1) % FEATURE_DECK.len();
                    self.feature_slide_at = Instant::now();
                }
                Task::none()
            }
            Message::LockedPlayer => {
                self.toast = Some(("Player is a Pro feature".to_string(), Instant::now()));
                Task::none()
            }
            Message::LockedRename => {
                self.toast = Some((
                    "Rename speakers and editing is a Pro feature".to_string(),
                    Instant::now(),
                ));
                Task::none()
            }
            Message::LockedSpeakers => {
                self.toast = Some((
                    "Speaker labels are a Pro feature.".to_string(),
                    Instant::now(),
                ));
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
                            "The Store purchase arrives in 1.1; the trial is open now".to_string(),
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
                        self.toast = Some((state.label().to_string(), Instant::now()));
                    }
                }
                Task::none()
            }
            Message::ToggleTier => {
                let now = SystemTime::now();
                if self.license.access.tier == license::Tier::Pro {
                    self.license.revoke_pro(now);
                    self.save_license();
                    self.clamp_preset();
                    self.toast = Some(("Switched to Free".to_string(), Instant::now()));
                } else {
                    self.license.grant_pro(now);
                    self.save_license();
                    self.toast = Some(("Switched to Pro".to_string(), Instant::now()));
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
            Message::OpenBriefing => {
                self.header_menu = None;
                if !self.can(license::Feature::Briefing) {
                    self.toast = Some(("Briefing is a Pro feature".to_string(), Instant::now()));
                    return Task::none();
                }
                self.open_briefing();
                Task::none()
            }
            Message::CloseBriefing => {
                if let Some(modal) = self.open_modal() {
                    return self.close_modal(modal);
                }
                self.briefing = None;
                Task::none()
            }
            Message::RetryBriefing => {
                self.open_briefing();
                Task::none()
            }
            Message::SaveBriefing => {
                self.save_briefing();
                Task::none()
            }
            Message::DownloadBriefingModel => {
                self.start_model_download_for(vec!["briefing"]);
                Task::none()
            }
            Message::LockedBriefing => {
                self.toast = Some(("Briefing is a Pro feature".to_string(), Instant::now()));
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
                } else if matches!(kind, HelpKind::Settings) {
                    return self.update(Message::OpenSettings);
                } else {
                    self.help_open = Some(kind);
                    self.toast = None;
                }
                Task::none()
            }
            // One close, whether from the card's button or from Escape: the
            // tips are a link in the completion chain and must hand off.
            Message::CloseHelp => self.close_modal(Modal::Help),
            Message::Noop => Task::none(),
            Message::Escape => {
                // A dialog in front takes the key, whatever it is. This now
                // covers Help and the paywall too: they were in `popup_open`
                // but missing from the escape chain, so Escape fell through to
                // the run/view state behind them.
                if let Some(modal) = self.open_modal() {
                    return self.close_modal(modal);
                }
                if self.header_menu.is_some() {
                    self.header_menu = None;
                    return Task::none();
                }
                if self.line_edit.is_some() {
                    self.cancel_inline_edit();
                } else if self.find_open {
                    self.close_find();
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
                if self.rename_open
                    || self.vocabulary_open
                    || self.briefing.is_some()
                    || self.line_edit.is_some()
                    || self.settings_open
                    || self.calibration.is_some()
                {
                    return Task::none();
                }
                Self::focus_probe().map(Message::ShortcutPlayChecked)
            }
            Message::ShortcutPlayChecked(focused) => {
                if !focused
                    && !self.rename_open
                    && !self.vocabulary_open
                    && self.briefing.is_none()
                    && self.line_edit.is_none()
                    && !self.settings_open
                    && self.calibration.is_none()
                {
                    self.toggle_playback()
                } else {
                    Task::none()
                }
            }
            Message::FindHover(hovered) => {
                self.find_hover = hovered;
                Task::none()
            }
            Message::PollFindFocus => Self::focus_probe().map(Message::FindFocusPolled),
            Message::FindFocusPolled(focused) => {
                // The row closes when its field loses focus. The pointer being
                // over the row keeps it alive: clicking next/prev is a click
                // outside the field, and its press must not take the row away.
                if self.find_open && !self.find_focus_pending && !self.find_hover && !focused {
                    self.close_find();
                }
                Task::none()
            }
            Message::ViewShortcut(shortcut) => {
                if self.view_shortcut_blocked() {
                    return Task::none();
                }
                Self::focus_probe().map(move |focused| Message::ViewShortcutChecked(shortcut, focused))
            }
            Message::ViewShortcutChecked(shortcut, focused) => {
                if focused || self.view_shortcut_blocked() {
                    return Task::none();
                }
                match shortcut {
                    ViewShortcut::Follow => self.update(Message::ToggleFollowPlayhead),
                    ViewShortcut::Timestamps => {
                        self.update(Message::TimestampsToggled(!self.show_timestamps))
                    }
                    ViewShortcut::Sms => self.update(Message::SmsToggled(!self.sms_mode)),
                    ViewShortcut::Join => self.update(Message::ToggleJoinLines),
                    ViewShortcut::Karaoke => {
                        self.update(Message::KaraokeToggled(!self.karaoke))
                    }
                    ViewShortcut::Confidence if self.has_confidence() => {
                        self.update(Message::ToggleConfidenceView)
                    }
                    ViewShortcut::Confidence => Task::none(),
                }
            }
            Message::SeekBy(delta) => {
                if self.rename_open
                    || self.vocabulary_open
                    || self.briefing.is_some()
                    || self.line_edit.is_some()
                    || self.settings_open
                    || self.calibration.is_some()
                {
                    return Task::none();
                }
                Self::focus_probe().map(move |focused| Message::SeekByChecked(delta, focused))
            }
            Message::SeekByChecked(delta, focused) => {
                if focused
                    || self.rename_open
                    || self.line_edit.is_some()
                    || self.settings_open
                    || self.calibration.is_some()
                {
                    return Task::none();
                }
                if self.shown_duration().is_some() {
                    // A seek moves the target; a glide towards the old one is
                    // no longer where the pane should be.
                    self.scroll_anim = None;
                    let max = self.shown_duration().unwrap_or(0.0).max(0.0);
                    self.playhead = (self.playhead + delta).clamp(0.0, max);
                    let playhead = self.playhead;
                    self.last_tick = Some(Instant::now());
                    if let Some(player) = &self.player {
                        player.seek(playhead);
                    }
                    // A seek is a jump the user asked for, so the pane goes with
                    // it: a single tap eases there, a held key (repeat faster
                    // than `FOLLOW_SEEK_QUIET`) lands each step. The live
                    // transcript pins to its newest line rather than the
                    // playhead, and Follow off means the reader chose where to
                    // be, so neither is moved here.
                    if self.follow_playhead && !self.live_streaming() {
                        if let Some(line) = self.line_for_playhead(self.playhead) {
                            return self.seek_to_line(line);
                        }
                    }
                }
                Task::none()
            }
            Message::StartRecording => {
                if self.recording.is_some() {
                    return Task::none();
                }
                let Some(dir) = self
                    .output_dir
                    .clone()
                    .or_else(|| Self::config_dir().map(|dir| dir.join("recordings")))
                else {
                    self.toast =
                        Some(("No folder to record into".to_string(), Instant::now()));
                    return Task::none();
                };
                if let Err(error) = std::fs::create_dir_all(&dir) {
                    self.toast = Some((
                        format!("Could not create {}: {error}", dir.display()),
                        Instant::now(),
                    ));
                    return Task::none();
                }
                let path = dir.join(crate::recording::default_name());
                match crate::recording::Recorder::start(self.input_device.clone(), path) {
                    Ok(recorder) => {
                        self.recording = Some(recorder);
                        self.toast =
                            Some((format!("Recording to {}", dir.display()), Instant::now()));
                    }
                    Err(message) => {
                        self.toast = Some((
                            format!(
                                "Could not record: {message}. Check the microphone in Settings."
                            ),
                            Instant::now(),
                        ));
                    }
                }
                Task::none()
            }
            Message::StopRecording => {
                let Some(recorder) = self.recording.take() else {
                    return Task::none();
                };
                match recorder.stop() {
                    Ok((path, seconds)) => {
                        let added = self.add_paths(vec![path]);
                        let count = added.len();
                        let task = self.added_tasks(added);
                        if count == 0 {
                            self.toast = Some((
                                "The recording was already in the queue".to_string(),
                                Instant::now(),
                            ));
                        } else {
                            self.toast = Some((
                                format!(
                                    "Recorded {} \u{2014} added to the queue",
                                    format_secs(seconds as f64)
                                ),
                                Instant::now(),
                            ));
                            // The chosen behaviour: a fresh recording starts
                            // transcribing at once. A run already in flight
                            // picks it up in the follow-up pass.
                            if !matches!(self.status, Status::Running) {
                                self.start();
                            }
                        }
                        return task;
                    }
                    Err(message) => {
                        self.toast =
                            Some((format!("Recording failed: {message}"), Instant::now()));
                    }
                }
                Task::none()
            }
            Message::InputDeviceChanged(name) => {
                self.input_device = if name == SYSTEM_DEFAULT_DEVICE {
                    None
                } else {
                    Some(name)
                };
                self.save_settings();
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
                    return self.added_tasks(added);
                }
                Task::none()
            }
            Message::AddFolder => {
                if !self.can(license::Feature::Batch) {
                    self.toast =
                        Some(("Folder import is a Pro feature".to_string(), Instant::now()));
                    return Task::none();
                }
                Task::perform(
                    async { rfd::FileDialog::new().pick_folder() },
                    Message::FolderPicked,
                )
            }
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
                    return self.added_tasks(added);
                }
                Task::none()
            }
            Message::TranscriptsFound(single, hits) => {
                // Reading the transcript and re-deriving the turns/waveform is
                // per file, so one slow probe cannot hold up the others.
                let normalize = self.normalize;
                Task::batch(hits.into_iter().map(move |hit| {
                    Task::perform(
                        async move {
                            let text = backend::read_transcript(&hit.transcript)?;
                            if text.trim().is_empty() {
                                return None;
                            }
                            let (turns, probe) =
                                backend::load_completed(
                                    hit.audio.clone(),
                                    None,
                                    TIMELINE_BUCKETS,
                                    normalize,
                                );
                            let (duration, peaks) = match probe {
                                Some((duration, peaks)) => (Some(duration), peaks),
                                None => (None, Vec::new()),
                            };
                            Some(AdoptedTranscript {
                                index: hit.index,
                                audio: hit.audio,
                                transcript: hit.transcript,
                                text,
                                duration,
                                peaks,
                                turns: turns.unwrap_or_default(),
                            })
                        },
                        move |adopted| match adopted {
                            Some(adopted) => Message::TranscriptAdopted(single, adopted),
                            None => Message::Noop,
                        },
                    )
                }))
            }
            Message::TranscriptAdopted(single, adopted) => {
                // Only adopt onto the row this audio was added as, and only
                // while it is still waiting: a run may have started on it while
                // the lookup and probe were running.
                let still_waiting = self.queue.get(adopted.index).is_some_and(|item| {
                    matches!(item.status, ItemStatus::Pending)
                        && item.audio.as_deref() == Some(adopted.audio.as_path())
                });
                if !still_waiting {
                    return Task::none();
                }
                let editable = adopted
                    .transcript
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"));
                let output = editable.then(|| adopted.transcript.clone());
                {
                    let item = &mut self.queue[adopted.index];
                    item.status = ItemStatus::Done;
                    item.transcript = Some(adopted.transcript.clone());
                    item.duration = adopted.duration;
                    item.peaks = Arc::new(adopted.peaks);
                    item.turns = adopted.turns;
                }
                self.invalidate_derived();
                self.save_queue();
                // A single file is the case where the user is waiting to see
                // the text, so it opens; a folder add leaves the queue on
                // screen and the adopted rows simply read as Done.
                if !single {
                    return Task::none();
                }
                let name = adopted
                    .audio
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| adopted.audio.display().to_string());
                self.toast = Some((
                    format!("Added {name} with its transcript"),
                    Instant::now(),
                ));
                self.open_item_view(adopted.index, Some((adopted.text, output)))
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
            Message::SkipItem(index) => {
                if !matches!(self.status, Status::Running) {
                    if let Some(item) = self.queue.get_mut(index) {
                        if matches!(item.status, ItemStatus::Interrupted) {
                            item.status = ItemStatus::Failed("skipped".to_string());
                            if let Some(audio) = &item.audio {
                                let _ = std::fs::remove_file(
                                    hippo_transcribe_core::pipeline::checkpoint_path(audio),
                                );
                            }
                            self.save_queue();
                        }
                    }
                }
                Task::none()
            }
            Message::RemoveItem(index) => {
                if !matches!(self.status, Status::Running) && index < self.queue.len() {
                    // The edited document may be the row about to be removed.
                    self.commit_inline_edit(false);
                    let was_viewing = self.selected_view.is_some();
                    if let Some(audio) = self.queue[index].audio.clone() {
                        let _ = std::fs::remove_file(
                            hippo_transcribe_core::pipeline::checkpoint_path(&audio),
                        );
                    }
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
                    // "Clear" is a full reset: queue, transcript and run state,
                    // including the document an open editor was writing to.
                    self.commit_inline_edit(false);
                    self.queue.clear();
                    self.batch_paths.clear();
                    self.gpu_retry_pending = false;
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
                    self.scroll_anim = None;
                    self.follow_issue = None;
                    *self.follow_row_height.borrow_mut() = None;
                    self.reset_pane_geometry();
                    self.scroll_seek = false;
                    self.output_txt = None;
                    self.summary_open = false;
                    self.pending_rename = false;
                    self.pending_tips = false;
                    self.rename_open = false;
                    self.rename_fields.clear();
                    self.rename_focus = 0;
                    self.search_query.clear();
                    self.search_index = 0;
                    self.follow_offset = None;
                    self.scroll_anim = None;
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
                    self.stage_id = "idle".to_string();
                    self.status = Status::Idle;
                    self.invalidate_derived();
                    self.save_queue();
                }
                Task::none()
            }
            Message::OpenSummary => {
                self.header_menu = None;
                if self.has_results() {
                    self.summary_open = true;
                    // Opened by hand, so it carries none of the chain's follow-ups.
                    self.pending_rename = false;
                    self.pending_tips = false;
                    self.toast = None;
                    return scrollable::snap_to(
                        summary_id(),
                        scrollable::RelativeOffset { x: 0.0, y: 0.0 },
                    );
                }
                Task::none()
            }
            Message::CloseSummary => self.close_modal(Modal::Summary),
            Message::SetEditingTips(show) => {
                self.editing_tips = show;
                self.save_settings();
                Task::none()
            }
            Message::OpenRename => {
                self.header_menu = None;
                // A rename rewrites the line the editor may be showing.
                self.commit_inline_edit(false);
                self.open_rename()
            }
            Message::OpenVocabulary => {
                self.header_menu = None;
                self.commit_inline_edit(false);
                self.open_vocabulary()
            }
            Message::CloseVocabulary => {
                self.vocabulary_open = false;
                Task::none()
            }
            Message::VocabTermsAction(action) => {
                self.vocab_terms.perform(action);
                Task::none()
            }
            Message::VocabReplacementsAction(action) => {
                self.vocab_replacements.perform(action);
                Task::none()
            }
            Message::SaveVocabulary => {
                let (vocabulary, bad) = Vocabulary::parse_fields(
                    &self.vocab_terms.text(),
                    &self.vocab_replacements.text(),
                );
                if !bad.is_empty() {
                    // The dialog shows the same complaint inline; keep it open
                    // rather than silently dropping what could not be parsed.
                    self.toast = Some((
                        format!("Not saved: {} needs heard = shown", bad.join(", ")),
                        Instant::now(),
                    ));
                    return Task::none();
                }
                self.vocabulary_open = false;
                match vocabulary.save() {
                    Ok(()) => {
                        self.toast = Some((
                            "Vocabulary saved for the next run".to_string(),
                            Instant::now(),
                        ));
                    }
                    Err(error) => {
                        self.toast = Some((
                            format!("Could not save vocabulary: {error}"),
                            Instant::now(),
                        ));
                    }
                }
                Task::none()
            }
            Message::FlipLineSpeaker(index) => self.flip_line_speaker(index),
            Message::EditLine(index) => self.begin_inline_edit(index),
            Message::LineEditAction(position, action) => {
                if let Some(row) = self
                    .line_edit
                    .as_mut()
                    .and_then(|edit| edit.rows.get_mut(position))
                {
                    row.content.perform(action);
                }
                Task::none()
            }
            Message::DoneLineEdit => {
                self.commit_inline_edit(true);
                Task::none()
            }
            Message::CancelLineEdit => {
                self.cancel_inline_edit();
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
            Message::MergeNextLine => {
                // Reachable from Alt+Down, which the keyboard subscription
                // delivers even while a dialog owns the screen.
                if !self.line_edit_shortcut_blocked() && self.can(license::Feature::Editing) {
                    self.merge_next_line();
                }
                Task::none()
            }
            Message::MergePrevLine => {
                if !self.line_edit_shortcut_blocked() && self.can(license::Feature::Editing) {
                    self.merge_prev_line();
                }
                Task::none()
            }
            Message::SplitLineAtCursor => {
                if !self.line_edit_shortcut_blocked() && self.can(license::Feature::Editing) {
                    self.split_line_at_cursor();
                }
                Task::none()
            }
            Message::UndoLineEdit => {
                if self.can(license::Feature::Editing) {
                    self.undo_line_edit();
                }
                Task::none()
            }
            Message::RedoLineEdit => {
                if self.can(license::Feature::Editing) {
                    self.redo_line_edit();
                }
                Task::none()
            }
            Message::CopyTranscript => {
                self.header_menu = None;
                if self.shown_transcript().is_empty() {
                    return Task::none();
                }
                self.toast = Some(("Copied transcript".to_string(), Instant::now()));
                iced::clipboard::write(self.shown_transcript().to_string())
            }
            Message::RevealOutput => {
                self.header_menu = None;
                self.reveal_output();
                Task::none()
            }
            Message::SaveTranscriptAs => {
                self.header_menu = None;
                if self.shown_transcript().trim().is_empty() {
                    self.toast = Some(("No transcript to save".to_string(), Instant::now()));
                    return Task::none();
                }
                // Prefer the filename of the transcript currently on screen.
                // It already carries the stem, preset and speaker count in the
                // same order as the generated output.
                let mut default_name = self
                    .selected_view
                    .as_ref()
                    .map(|view| view.source.as_path())
                    .or_else(|| self.shown_output())
                    .or_else(|| self.shown_audio())
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(|name| Path::new(name).with_extension("txt").display().to_string())
                    .unwrap_or_else(|| "transcript.txt".to_string());
                if self.join_lines {
                    let stem = Path::new(&default_name)
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("transcript");
                    default_name = format!("{stem}-joined.txt");
                }
                Task::perform(
                    async move {
                        let mut dialog = rfd::FileDialog::new();
                        for format in ExportFormat::ALL {
                            dialog = dialog.add_filter(format.label(), &[format.extension()]);
                        }
                        dialog.set_file_name(default_name).save_file()
                    },
                    Message::SaveTranscriptAsPicked,
                )
            }
            Message::SaveTranscriptAsPicked(path) => {
                if let Some(mut path) = path {
                    let format = path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .and_then(ExportFormat::from_extension)
                        .unwrap_or(ExportFormat::Txt);
                    if path.extension().is_none() {
                        path.set_extension(format.extension());
                    }

                    let text = self.shown_transcript().to_string();
                    let source = self.shown_output().map(Path::to_path_buf);
                    let cues = backend::export_cues(source.as_deref(), self.shown_lines());
                    let turns = self.shown_turns().to_vec();
                    let duration = self.shown_duration();

                    // The JSON sidecar is the transcript's metadata, not an
                    // export target: writing over it would erase speaker ids
                    // and line bounds.
                    if source
                        .as_deref()
                        .and_then(hippo_transcribe_core::sidecar::locate)
                        .as_deref()
                        == Some(path.as_path())
                    {
                        self.toast = Some((
                            "That file holds the transcript's metadata; choose another name"
                                .to_string(),
                            Instant::now(),
                        ));
                    } else if text.trim().is_empty() {
                        self.toast = Some(("No transcript to save".to_string(), Instant::now()));
                    } else {
                        // Saving over the transcript itself keeps the backup
                        // the editor has always taken before rewriting it.
                        let saving_over_source = source.as_deref() == Some(path.as_path());
                        let result = if saving_over_source && format == ExportFormat::Txt {
                            write_transcript_file(&path, &text)
                        } else {
                            hippo_transcribe_core::export::write(
                                &path, format, &text, &cues, &turns, duration,
                            )
                        };
                        match result {
                            Ok(()) => {
                                self.modified = false;
                                self.toast =
                                    Some((format!("Saved {}", path.display()), Instant::now()));
                            }
                            Err(error) => self.toast = Some((
                                format!("Could not save {}: {error}", path.display()),
                                Instant::now(),
                            )),
                        }
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
                if !self.shows(license::Audience::ProOnly) && preset != Preset::Balanced {
                    self.toast = Some((
                        "Speed and Accuracy presets are Pro features".to_string(),
                        Instant::now(),
                    ));
                    return Task::none();
                }
                let feature = match preset {
                    Preset::Balanced => None,
                    Preset::Speed => Some(license::Feature::SpeedPreset),
                    Preset::Accuracy => Some(license::Feature::AccuracyPreset),
                };
                if let Some(feature) = feature {
                    if !self.can(feature) {
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
                // Cmd/Ctrl+Enter is Save while the vocabulary editor owns the
                // screen; starting a run behind the scrim would be a surprise.
                if self.vocabulary_open {
                    return self.update(Message::SaveVocabulary);
                }
                if !self.popup_open() && self.calibration.is_none() {
                    self.run_setup_open = false;
                    self.start();
                }
                Task::none()
            }
            Message::Cancel => {
                self.cancel();
                Task::none()
            }
            Message::Frame(now) => {
                // A click point can only be mapped once the editor has been
                // laid out, which is what a frame proves. Focus it and place
                // the caret before the glide, so a double-click lands ready to
                // type. The Find field is the same: Ctrl+F mounts it, and this
                // frame is the first one where focusing it can work.
                let caret = self
                    .line_edit_caret
                    .take()
                    .filter(|_| self.line_edit.is_some())
                    .map(|point| {
                        Task::batch(vec![
                            Self::focus_line_editor(),
                            Task::done(Message::LineEditAction(
                                0,
                                text_editor::Action::Click(point),
                            )),
                        ])
                    });
                let find_focus = if std::mem::take(&mut self.find_focus_pending) && self.find_open {
                    Some(text_input::focus(find_id()))
                } else {
                    None
                };
                // A dialog owns the screen: land the move instead of easing
                // behind it.
                if self.popup_open() {
                    let mut tasks = vec![self.stop_glide(true)];
                    if let Some(caret) = caret {
                        tasks.push(caret);
                    }
                    if let Some(focus) = find_focus {
                        tasks.push(focus);
                    }
                    return Task::batch(tasks);
                }
                let glide = match self.advance_glide(now) {
                    Some(y) => scrollable::scroll_to(
                        transcript_id(),
                        scrollable::AbsoluteOffset { x: 0.0, y },
                    ),
                    None => Task::none(),
                };
                let mut tasks = vec![glide];
                if let Some(caret) = caret {
                    tasks.push(caret);
                }
                if let Some(focus) = find_focus {
                    tasks.push(focus);
                }
                Task::batch(tasks)
            }
            Message::Tick => {
                self.poll_briefing();
                if self.feature_deck_auto_advance() {
                    if self.feature_slide_at.elapsed() >= Duration::from_secs(7) {
                        self.feature_slide = (self.feature_slide + 1) % FEATURE_DECK.len();
                        self.feature_slide_at = Instant::now();
                    }
                } else {
                    self.feature_slide_at = Instant::now();
                }
                let mut tasks = vec![self.drain_events()];
                self.drain_model_download();
                self.drain_calibration();
                if let Some(calibration) = &mut self.calibration {
                    if calibration.testing() {
                        let target = if calibration.phase == CalibrationPhase::TestingCpu {
                            0.47
                        } else {
                            0.97
                        };
                        calibration.progress += (target - calibration.progress) * 0.02;
                    }
                }
                if self.model_download.is_none()
                    && self
                        .last_model_check
                        .is_none_or(|last| last.elapsed() > Duration::from_secs(5))
                {
                    self.refresh_models();
                }
                // The live buffer, deliberately: this rebuilds the text that is
                // written to the file, and the Join preview must not reach it.
                // The `selected_view.is_none()` guard is what makes the raw
                // buffer the document on screen here.
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

                // Estimate remaining wall-clock time from measured throughput:
                // audio seconds actually transcribed per wall second.
                if matches!(self.status, Status::Running) {
                    let pending_now = self
                        .queue
                        .iter()
                        .filter(|item| matches!(item.status, ItemStatus::Pending))
                        .count();
                    let queue_grew = pending_now > self.eta_pending;
                    self.eta_pending = pending_now;
                    match self.estimate_remaining() {
                        Some(raw) => {
                            // A new phase is a new measurement: seed it rather
                            // than blending it into the last phase's number, or
                            // the smoothing and the cap would hold a stale
                            // estimate across the boundary.
                            let phase_changed = self
                                .eta_shown
                                .is_some_and(|shown| shown.phase != raw.phase);
                            let remaining = match (phase_changed, raw.remaining) {
                                (true, _) | (false, None) => raw.remaining,
                                (false, Some(raw_left)) => {
                                    let smoothed = match self
                                        .eta_shown
                                        .and_then(|shown| shown.remaining)
                                    {
                                        Some(previous) => (previous + raw_left) * 0.5,
                                        None => raw_left,
                                    };
                                    // A shrinking workload can drop the estimate
                                    // fast; letting it climb more than 10% per
                                    // tick would spike on a stalled sample,
                                    // unless new files were queued.
                                    let ceiling = self
                                        .eta_shown
                                        .and_then(|shown| shown.remaining)
                                        .map(|previous| previous * 1.1)
                                        .unwrap_or(f32::INFINITY);
                                    Some(if smoothed > ceiling && !queue_grew {
                                        ceiling
                                    } else {
                                        smoothed
                                    }
                                    .max(0.0))
                                }
                            };
                            self.eta_shown = Some(EtaView {
                                phase: raw.phase,
                                remaining,
                            });
                        }
                        None => self.eta_shown = None,
                    }
                } else {
                    self.eta_shown = None;
                    self.work_done = 0.0;
                    self.work_total = 0.0;
                    self.diarize_fraction = 0.0;
                    self.eta_samples.clear();
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
                // Follow tracks the active line on whichever transcript is
                // shown (streamed text while live, the audio playhead
                // otherwise). Manual scrolling disengages it. The target is
                // only re-issued when it actually moved, so the pane sits
                // still while nothing changes.
                if self.follow_playhead {
                    let active = self.follow_line_index();
                    if let Some(y) = self.follow_scroll(active) {
                        self.follow_offset = Some(y);
                        tasks.push(scrollable::scroll_to(
                            transcript_id(),
                            scrollable::AbsoluteOffset { x: 0.0, y },
                        ));
                    }
                } else {
                    self.follow_offset = None;
                    // A jump the user asked for is not a follow move: let it
                    // finish even though Follow is off, or a timeline click
                    // would freeze the pane half way.
                    if !self.scroll_anim.is_some_and(|anim| anim.jump) {
                        self.scroll_anim = None;
                    }
                    // Keep the live view pinned to the bottom as text is added,
                    // as long as the user hasn't scrolled away from the bottom.
                    if let Some(y) = self.follow_scroll(None) {
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
                    .is_some_and(|(message, at)| at.elapsed() > toast_ttl(message))
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
        if !self.can(license::Feature::Editing) {
            self.toast = Some((
                format!("{} is a Pro feature", license::Feature::Editing.label()),
                Instant::now(),
            ));
            return Task::none();
        }
        let mut seen: Vec<String> = Vec::new();
        for line in self.shown_lines() {
            if !line.speaker.is_empty() && !seen.contains(&line.speaker) {
                seen.push(line.speaker.clone());
            }
        }
        // The dialog lists speakers in their natural order (SPEAKER 1, 2, 3 …)
        // rather than the order they happen to speak.
        sort_speaker_names(&mut seen);
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

    /// Open the briefing for the document on screen.
    ///
    /// The transcript is the input; no audio is re-read. A missing model opens
    /// the download prompt instead of failing.
    fn open_briefing(&mut self) {
        self.commit_inline_edit(false);
        let source = self.shown_output().map(Path::to_path_buf);
        let cues = backend::export_cues(source.as_deref(), self.shown_lines());
        if cues.is_empty() {
            self.toast = Some((
                "Transcribe something before asking for a briefing".to_string(),
                Instant::now(),
            ));
            return;
        }
        let lines: Vec<BriefingLine> = cues
            .into_iter()
            .map(|cue| BriefingLine {
                start: cue.start.unwrap_or(0.0),
                end: cue.end,
                speaker: cue.speaker,
                text: cue.text,
            })
            .collect();
        if !download::briefing_ready() {
            self.briefing = Some(BriefingState::NeedsModel);
            return;
        }
        self.spawn_briefing(lines, source);
    }

    fn spawn_briefing(&mut self, lines: Vec<BriefingLine>, source: Option<PathBuf>) {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let pid = Arc::new(AtomicU32::new(0));
        match backend::spawn_briefing(
            lines.clone(),
            models::briefing_model(),
            events.clone(),
            pid.clone(),
        ) {
            Ok(response) => {
                self.briefing = Some(BriefingState::Running(BriefingRun {
                    events,
                    pid,
                    response,
                    lines,
                    source,
                    stage: "Starting".to_string(),
                    fraction: 0.0,
                    started: Instant::now(),
                }));
            }
            Err(message) => self.briefing = Some(BriefingState::Failed(message)),
        }
    }

    /// Move helper events into the dialog, and settle when it finishes.
    fn poll_briefing(&mut self) {
        let events: Vec<backend::BriefingEvent> = match self.briefing.as_ref() {
            Some(BriefingState::Running(run)) => run
                .events
                .lock()
                .map(|mut queue| queue.drain(..).collect())
                .unwrap_or_default(),
            _ => return,
        };
        let mut terminal: Option<Result<(), String>> = None;
        if let Some(BriefingState::Running(run)) = self.briefing.as_mut() {
            for event in events {
                match event {
                    backend::BriefingEvent::Stage { label, fraction } => {
                        run.stage = label;
                        run.fraction = fraction.clamp(0.0, 1.0);
                    }
                    backend::BriefingEvent::Done => terminal = Some(Ok(())),
                    backend::BriefingEvent::Failed(message) => terminal = Some(Err(message)),
                }
            }
        }
        let Some(result) = terminal else {
            return;
        };
        let Some(BriefingState::Running(run)) = self.briefing.take() else {
            return;
        };
        let response = std::fs::read_to_string(&run.response);
        let _ = std::fs::remove_file(&run.response);
        self.briefing = Some(match result {
            Ok(()) => match response
                .map_err(|error| format!("could not read the briefing: {error}"))
                .and_then(|text| {
                    serde_json::from_str::<Briefing>(&text)
                        .map_err(|error| format!("invalid briefing: {error}"))
                }) {
                Ok(briefing) => BriefingState::Ready {
                    briefing,
                    lines: run.lines,
                    source: run.source,
                },
                Err(message) => BriefingState::Failed(message),
            },
            Err(message) => BriefingState::Failed(message),
        });
    }

    fn save_briefing(&mut self) {
        let Some(BriefingState::Ready {
            briefing,
            lines,
            source,
        }) = self.briefing.as_ref()
        else {
            return;
        };
        let target = source.clone().or_else(|| self.shown_output().map(Path::to_path_buf));
        let Some(target) = target else {
            self.toast = Some((
                "Nowhere to save the briefing".to_string(),
                Instant::now(),
            ));
            return;
        };
        let stem = target
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("transcript");
        let path = target.with_file_name(format!("{stem}-briefing.md"));
        let text = hippo_briefing::markdown(briefing, lines);
        match write_atomic(&path, &text) {
            Ok(()) => {
                self.toast = Some((format!("Saved {}", path.display()), Instant::now()));
            }
            Err(error) => {
                self.toast = Some((
                    format!("Could not save {}: {error}", path.display()),
                    Instant::now(),
                ));
            }
        }
    }

    /// Open the custom-vocabulary editor with what is stored.
    ///
    /// Offered from Edit (it changes transcript text) and from Help (it is a
    /// reference control users look for there).
    fn open_vocabulary(&mut self) -> Task<Message> {
        let (terms, replacements) = Vocabulary::load().to_fields();
        self.vocab_terms = text_editor::Content::with_text(&terms);
        self.vocab_replacements = text_editor::Content::with_text(&replacements);
        self.vocabulary_open = true;
        self.toast = None;
        // The terms field is the first focusable widget once the transcript
        // editor has been committed, so this lands on it.
        iced::advanced::widget::operate(focus_first::<Message>())
    }

    /// Rewrite the live transcript text and its file from the live lines.
    ///
    /// Deliberately the live buffer: an item view has its own file, written by
    /// its own path, and must not be overwritten from here.
    fn refresh_transcript(&mut self) {
        self.transcript = lines_text(&self.lines);
        if let Some(path) = &self.output_txt {
            let _ = write_transcript_file(path, &self.transcript);
        }
    }

    /// Whether Reset has somewhere to go: the sibling copy `write_transcript_file`
    /// keeps before the first edit. Without an edit there is no earlier state.
    fn reset_available(&self) -> bool {
        if !self.can(license::Feature::Editing) {
            return false;
        }
        if self.selected_view.is_none() && matches!(self.status, Status::Running) {
            return false;
        }
        self.shown_output()
            .map(unedited_path)
            .is_some_and(|path| path.is_file())
    }

    /// Revert the edited document to `<stem>-unedited.txt`, the copy kept when
    /// the transcript was first rewritten. The revert is one undo step, so a
    /// Reset made by mistake can be taken back like any other edit.
    fn reset_transcript(&mut self) -> Task<Message> {
        self.commit_inline_edit(false);
        let Some(output) = self.shown_output().map(Path::to_path_buf) else {
            return Task::none();
        };
        let unedited = unedited_path(&output);
        let Some(text) = backend::read_transcript(&unedited) else {
            self.toast = Some((
                "Could not read the initial transcript".to_string(),
                Instant::now(),
            ));
            return Task::none();
        };
        let confidences = backend::read_confidences(&unedited);
        self.push_line_undo();
        self.replace_edit_lines(attach_confidences(parse_lines(&text), &confidences));
        self.modified = false;
        self.scroll_anim = None;
        self.reset_pane_geometry();
        self.toast = Some(("Reverted to the initial transcript".to_string(), Instant::now()));
        Task::none()
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
        let was_empty = self.queue.is_empty();
        // Free adds one file per action; multi-select and folder drops are Pro.
        let mut paths = paths;
        if !self.can(license::Feature::Batch) && paths.len() > 1 {
            paths.truncate(1);
            self.toast = Some((
                "The free version adds one file at a time".to_string(),
                Instant::now(),
            ));
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
            if was_empty {
                self.feature_slide = 0;
                self.feature_slide_at = Instant::now();
            }
            self.save_queue();
        }
        added
    }

    /// What an add starts: the duration probe for every file, plus the lookup
    /// for a transcript each one may already have.
    ///
    /// A file with a transcript is adopted as a completed item instead of being
    /// queued for a run; see `Message::TranscriptAdopted`. The scan runs off the
    /// UI thread with the probe, and reads each directory once for the whole
    /// add.
    fn added_tasks(&self, added: Vec<PathBuf>) -> Task<Message> {
        let rows: Vec<(usize, PathBuf)> = added
            .iter()
            .filter_map(|path| {
                self.queue
                    .iter()
                    .position(|item| item.audio.as_deref() == Some(path.as_path()))
                    .map(|index| (index, path.clone()))
            })
            .collect();
        // A single file is opened when its transcript is found; a batch is not,
        // because the view would jump away from the queue the user just filled.
        let single = added.len() == 1;
        let output_dir = self.output_dir.clone();
        Task::batch([
            Self::probe_items(added),
            Task::perform(
                async move { crate::files::find_transcripts(&rows, output_dir.as_deref()) },
                move |hits| {
                    if hits.is_empty() {
                        return Message::Noop;
                    }
                    Message::TranscriptsFound(
                        single,
                        hits.into_iter()
                            .map(|(index, audio, transcript)| TranscriptHit {
                                index,
                                audio,
                                transcript,
                            })
                            .collect(),
                    )
                },
            ),
        ])
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
        // Free finishes on the summary; roughly one run in five also opens the
        // upgrade prompt, so it stays a nudge instead of a nag.
        if self.shows(license::Audience::FreeOnly) && upsell_roll() {
            self.summary_open = false;
            self.pending_rename = false;
            self.pending_tips = false;
            self.paywall_open = true;
            self.toast = None;
            if self.notifications {
                notify("Hippo Transcribe", "Finished \u{00b7} saved locally");
            }
            return true;
        }
        self.summary_open = true;
        // The tips explain editing, so they follow the same gate as the rename
        // that shares this chain: no editing, no card.
        let editing = self.can(license::Feature::Editing);
        self.pending_tips = editing && self.editing_tips;
        self.pending_rename = editing && generated == 1 && self.completed_count() == 1;
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
    fn toggle_playback(&mut self) -> Task<Message> {
        if !self.player_ready() {
            return Task::none();
        }
        if self.player.is_none() {
            let Some(path) = self.shown_audio().map(Path::to_path_buf) else {
                return Task::none();
            };
            self.player = Player::new(&path, self.playback_speed);
            if self.player.is_none() {
                self.toast = Some(("Could not open the audio file".to_string(), Instant::now()));
                return Task::none();
            }
            if let Some(player) = &self.player {
                player.seek(self.playhead);
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
        // Starting or stopping playback is a user decision: finish the move in
        // flight instead of leaving the followed line half a row off centre.
        self.stop_glide(true)
    }

    /// "Retranscribe" when nothing is waiting and the selected queue row is a
    /// finished transcript, because that is the row Start will run. Any pending
    /// item keeps the button "Transcribe": Start works through the queue. The
    /// same rule drives the setup dialog's primary button.
    fn transcribe_label(&self) -> &'static str {
        let pending = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending));
        let interrupted = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Interrupted));
        let finished = self
            .selected
            .and_then(|index| self.queue.get(index))
            .is_some_and(|item| matches!(item.status, ItemStatus::Done));
        if !pending && interrupted {
            "Resume"
        } else if !pending && finished {
            "Retranscribe"
        } else {
            "Transcribe"
        }
    }

    fn start(&mut self) {
        if self.queue.is_empty() {
            self.status = Status::Error("add at least one file".to_string());
            return;
        }
        let has_pending = self
            .queue
            .iter()
            .any(|item| matches!(item.status, ItemStatus::Pending | ItemStatus::Interrupted));
        if !has_pending {
            // Nothing is waiting: transcribe again only the finished file the
            // user has selected. The queue itself is never re-run wholesale.
            let target = self.selected.filter(|index| {
                self.queue.get(*index).is_some_and(|item| {
                    matches!(item.status, ItemStatus::Done | ItemStatus::Failed(_))
                })
            });
            let Some(index) = target else {
                self.toast = Some((
                    "Select a finished file to transcribe it again".to_string(),
                    Instant::now(),
                ));
                return;
            };
            let item = &mut self.queue[index];
            item.status = ItemStatus::Pending;
            item.transcript = None;
            item.stats = None;
            item.turns.clear();
            item.peaks = Arc::new(Vec::new());
            self.invalidate_derived();
            self.save_queue();
        }
        if self.calibration_needed() || (self.gpu_selected() && self.gpu_test_needed()) {
            self.open_calibration(true);
            return;
        }
        self.begin_batch(false);
    }

    /// Turn off GPU acceleration after a failed run so a broken driver cannot
    /// trap the next attempt in the same backend.
    fn disable_gpu_after_failure(&mut self, reason: &str) {
        Self::clear_gpu_guard();
        if !self.gpu_profile().is_some_and(|profile| profile.selected) {
            return;
        }
        let preset = self.preset.arg().to_string();
        let entry = self.gpu_profiles.entry(preset).or_default();
        entry.selected = false;
        let setup_failure = reason.to_ascii_lowercase().contains("model not found");
        if !setup_failure {
            entry.gpu_error = Some(reason.to_string());
        }
        self.save_settings();
        self.console_lines.push(format!(
            "GPU acceleration disabled after a transcription failure: {reason}"
        ));
        self.toast = Some((
            if setup_failure {
                "Transcription failed: model files are missing".to_string()
            } else {
                "GPU acceleration disabled after an error; retry on CPU".to_string()
            },
            Instant::now(),
        ));
    }

    /// Spawn a single pipeline process for all pending queue items, so the
    /// models load once and are reused across the whole batch. `preserve_view`
    /// keeps a completed transcript (and its playback) visible across an
    /// automatic follow-up pass; explicit starts always return to live.
    fn begin_batch(&mut self, preserve_view: bool) {
        self.gpu_retry_pending = false;
        if !self.models_ready() {
            self.status = Status::Error("transcription models are missing".to_string());
            self.toast = Some((
                "Download the transcription models before starting".to_string(),
                Instant::now(),
            ));
            return;
        }
        let preserve = preserve_view && self.keep_pinned_view();
        let pending: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item.status, ItemStatus::Pending | ItemStatus::Interrupted))
            .map(|(index, _)| index)
            .collect();
        // Free runs exactly one file per press: the selected pending item, or
        // the first pending item when nothing is selected. The rest stay queued.
        let inputs: Vec<PathBuf> = if self.can(license::Feature::Batch) {
            pending
                .iter()
                .map(|index| self.queue[*index].path.clone())
                .collect()
        } else {
            let chosen = self
                .selected
                .filter(|index| pending.contains(index))
                .or_else(|| pending.first().copied());
            chosen
                .map(|index| vec![self.queue[index].path.clone()])
                .unwrap_or_default()
        };
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
        self.last_run = Some(RunRecord {
            preset: self.preset.arg().to_string(),
            language: self.language.clone(),
            num_speakers: self.num_speakers,
            output_dir: self
                .output_dir
                .as_ref()
                .map(|dir| dir.display().to_string()),
        });
        self.save_queue();

        self.batch = true;
        self.current = None;
        self.current_index = None;
        self.status = Status::Running;
        self.stage = "starting".to_string();
        self.stage_id = "starting".to_string();
        self.progress = 0.0;
        self.progress_shown = 0.0;
        self.overall_shown = 0.0;
        self.run_start = Some(Instant::now());
        self.elapsed_final = None;
        self.eta_shown = None;
        self.work_done = 0.0;
        self.work_total = 0.0;
        self.diarize_fraction = 0.0;
        self.eta_samples.clear();
        // A sample measured on another run's workload must not set the pace for
        // this one; the persisted prior still does.
        self.eta_throughput = None;
        self.eta_pending = 0;
        // Whatever was being typed into the outgoing transcript is saved
        // before its lines are replaced by the new run.
        self.commit_inline_edit(false);
        self.transcript.clear();
        self.lines.clear();
        self.pending_lines.clear();
        // A new run is a new document; no earlier edits can be undone onto it.
        self.line_history = LineHistory::default();
        self.reveal_accum = 0.0;
        self.completion_pending = false;
        self.current_index = None;
        self.transcript_dirty = false;
        self.follow_offset = None;
        self.scroll_target = None;
        self.scroll_anim = None;
        self.follow_issue = None;
        *self.follow_row_height.borrow_mut() = None;
        self.reset_pane_geometry();
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
            // Pro keeps a finished transcript on screen while a new run starts.
            if !self.keeps_finished_view() {
                self.selected = None;
                self.selected_view = None;
                self.view_error = None;
            }
        }
        self.cancelled.store(false, Ordering::SeqCst);
        let use_gpu = self.gpu_selected();
        self.gpu_actual = None;
        self.gpu_fallback = None;
        if use_gpu {
            if let Some(path) = Self::gpu_guard_path() {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&path, std::process::id().to_string());
            }
        } else {
            Self::clear_gpu_guard();
        }

        backend::spawn(
            inputs,
            self.output_dir.clone(),
            self.preset.arg(),
            self.num_speakers,
            self.language.clone(),
            use_gpu,
            true, // diarization cache is always on
            self.pipelines_diarize(),
            self.normalize,
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
        self.gpu_retry_pending = false;
        let pid = self.pid.load(Ordering::SeqCst);
        if pid != 0 {
            backend::kill(pid);
        }
        self.cancelled.store(true, Ordering::SeqCst);
        Self::clear_gpu_guard();
        self.scroll_anim = None;
        self.status = Status::Cancelled;
        self.stage = "cancelled".to_string();
        self.stage_id = "cancelled".to_string();
        self.progress = 0.0;
        self.progress_shown = 0.0;
        self.overall_shown = 0.0;
        self.run_start = None;
        self.elapsed_final = None;
        self.eta_shown = None;
        self.work_done = 0.0;
        self.work_total = 0.0;
        self.diarize_fraction = 0.0;
        self.eta_samples.clear();
        self.eta_pending = 0;
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
        self.modified = true;
        // A speaker rename is not part of the line editor's undo scope.
        self.line_history = LineHistory::default();
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
                backend::write_sidecar_renamed(path, &view.lines, &mapping);
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
            if let Some(path) = &self.output_txt {
                backend::write_sidecar_renamed(path, &self.lines, &mapping);
            }
        }
        self.invalidate_derived();
        self.rename_open = false;
    }

    // ---- inline line editor lifecycle ----

    /// Open the inline editor on `index`: the transcript row becomes a field,
    /// playback pauses, and Follow turns off so the pane stops moving under the
    /// caret. The same gates as before: Pro editing, a writable output, no live
    /// run, and no joined view.
    fn begin_inline_edit(&mut self, index: usize) -> Task<Message> {
        self.begin_inline_edit_at(index, None)
    }

    /// As [`Self::begin_inline_edit`], with the caret placed at `caret` - a
    /// point in the text's own coordinates - once the editor exists and has
    /// been laid out.
    fn begin_inline_edit_at(&mut self, index: usize, caret: Option<Point>) -> Task<Message> {
        // The editor takes the keyboard; Find is no longer focused.
        self.close_find();
        if self.join_lines {
            self.toast = Some(("Split the view to edit lines".to_string(), Instant::now()));
            return Task::none();
        }
        if !self.can(license::Feature::Editing) {
            self.toast = Some(("Editing is a Pro feature".to_string(), Instant::now()));
            return Task::none();
        }
        // Only transcripts we can write back to are editable, and the live
        // transcript is left alone while it is still being built.
        if self.shown_output().is_none()
            || (self.selected_view.is_none() && matches!(self.status, Status::Running))
        {
            self.toast = Some(("This transcript is read-only".to_string(), Instant::now()));
            return Task::none();
        }
        if self.line_edit.as_ref().is_some_and(|edit| edit.index == index) {
            return Task::none();
        }
        // Moving the editor from one line to another commits the first.
        let strip_was_open = self.line_edit.is_some();
        if strip_was_open {
            self.commit_inline_edit(false);
        }
        // Rows come from the document the editor writes to, not from the joined
        // preview, so their indices stay valid on commit.
        let Some(edit) = LineEdit::inline(index, self.edit_lines()) else {
            return Task::none();
        };
        self.line_edit = Some(edit);
        // The row's speaker joins the order the chips are drawn from.
        self.invalidate_derived();
        self.line_edit_caret = caret;
        self.line_edit_document = self.edit_document();
        // Editing while audio plays makes the caret fight the playhead; pause,
        // remember that it was playing, and keep the position.
        self.line_edit_resume = false;
        if let Some(player) = &self.player {
            if player.is_playing() {
                player.toggle();
                self.last_tick = Some(Instant::now());
                self.line_edit_resume = true;
            }
        }
        // Follow would scroll the edited row away while it is being typed into.
        if self.follow_playhead {
            self.follow_playhead = false;
            self.follow_offset = None;
            self.scroll_target = None;
            self.scroll_anim = None;
            self.follow_issue = None;
            self.scroll_seek = false;
        }
        // The edited row is a field, not a line: re-measure the window, but keep
        // the measured row height so moving the editor to a neighbouring row
        // does not flash a different layout for a frame.
        self.reflow_pane_geometry();
        self.summary_open = false;
        self.rename_open = false;
        self.last_line_click = None;
        self.toast = None;
        // With the strip appearing, `strip_scroll` shifts the offset and keeps the
        // edited row below it. Nothing else moves the pane when the strip was
        // already there, so moving the editor onto a row that is off the pane (the
        // keyboard path can pick one) has to come into view here.
        if strip_was_open {
            self.reveal_edited_line(index)
        } else {
            Task::none()
        }
    }

    /// Bring the row being edited into view, when nothing else moves the pane.
    ///
    /// The row's place is the same average-row-height estimate the follow logic
    /// uses. When the strip is appearing this never fires: `strip_scroll` holds the
    /// clicked row where it was and stops before it can go under the strip.
    fn reveal_edited_line(&mut self, index: usize) -> Task<Message> {
        let Some((top, bottom)) = self.edited_row_place(index) else {
            return Task::none();
        };
        if top >= 0.0 && bottom <= self.transcript_viewport {
            Task::none()
        } else {
            self.scroll_to_line(index)
        }
    }

    /// The edited row's place in the pane, estimated the way the follow logic
    /// estimates every row: its top and bottom in the list's own coordinates.
    fn edited_row_place(&self, index: usize) -> Option<(f32, f32)> {
        let viewport = self.transcript_viewport;
        if viewport <= 0.0 {
            return None;
        }
        let target = self.follow_target(index)?;
        let row = self.follow_row_estimate().max(1.0);
        let centre = target + viewport / 2.0 - self.transcript_offset;
        Some((centre - row / 2.0, centre + row / 2.0))
    }

    /// The scroll the editor strip's appearance or disappearance needs, if any.
    ///
    /// The strip is inserted between the header and the list, so its height moves
    /// the list's top edge and every row with it. Shifting the offset the other way
    /// by the same amount holds the content still on screen; see
    /// [`compensate_offset`] for the bounds that keeps it honest at the top of the
    /// document and under the strip.
    fn strip_scroll(&mut self) -> Option<Task<Message>> {
        let height = editor_strip_height(self.line_edit.is_some());
        let applied = self.strip_height;
        if (height - applied).abs() < 0.5 {
            return None;
        }
        self.strip_height = height;
        if self.transcript_viewport <= 0.0 {
            return None;
        }
        let delta = height - applied;
        let viewport = self.transcript_viewport;
        let place = self
            .line_edit
            .as_ref()
            .and_then(|edit| self.edited_row_place(edit.index));
        let target = match place {
            // A row that was outside the pane altogether (only the keyboard can open
            // the editor on one) has to be brought in; holding it still would hold it
            // out of sight.
            Some((top, bottom)) if top < 0.0 || bottom > viewport => self
                .line_edit
                .as_ref()
                .and_then(|edit| self.follow_target(edit.index))
                .unwrap_or_else(|| compensate_offset(self.transcript_offset, delta, f32::MAX)),
            // Otherwise hold the content still, but never scroll so far that the row's
            // top goes above the list's top edge.
            Some((top, _)) => {
                compensate_offset(self.transcript_offset, delta, self.transcript_offset + top)
            }
            None => compensate_offset(self.transcript_offset, delta, f32::MAX),
        };
        // A glide towards an older target would keep writing offsets over this one.
        self.scroll_anim = None;
        self.scroll_target = Some(target);
        self.follow_offset = Some(target);
        Some(scrollable::scroll_to(
            transcript_id(),
            scrollable::AbsoluteOffset { x: 0.0, y: target },
        ))
    }

    /// Commit the inline editor into the transcript and close it. One changed
    /// row is one undo step. `resume` restores playback when this commit is the
    /// user pressing Done or Enter, not a side effect of moving elsewhere.
    fn commit_inline_edit(&mut self, resume: bool) {
        let Some(edit) = self.line_edit.take() else {
            return;
        };
        // The edited row is gone, so any speaker it alone carried must leave
        // the order too.
        self.invalidate_derived();
        self.line_edit_caret = None;
        // Whether the user asked to save this line, as opposed to the editor
        // being committed as a side effect of moving elsewhere.
        let asked = resume;
        let was_playing = std::mem::take(&mut self.line_edit_resume);
        let resume = resume && was_playing;
        // A view change that dropped the document without committing first
        // must not write this row into whatever is on screen now.
        if self.line_edit_document != self.edit_document() {
            self.line_edit_document = None;
            self.reset_pane_geometry();
            return;
        }
        let Some(row) = edit.current_row() else {
            self.line_edit_document = None;
            self.reset_pane_geometry();
            return;
        };
        let (speaker, text) = line_edit_row_fields(&row.speaker, &row.content);
        if text.is_empty() {
            // A blank line has no rendered row of its own: writing it would
            // leave a line nobody can click. Asked to save, say so and keep the
            // field open; committed as a side effect, drop the change instead of
            // writing a hole into the document.
            if asked {
                self.line_edit = Some(edit);
                self.line_edit_resume = was_playing;
                self.toast = Some((
                    "A line needs text. Use the bin to remove it.".to_string(),
                    Instant::now(),
                ));
            } else {
                self.line_edit_document = None;
                self.reset_pane_geometry();
            }
            return;
        }
        self.line_edit_document = None;
        let changed = !self.edit_lines().get(edit.index).is_some_and(|line| {
            line.speaker == speaker && line_text(line) == text
        });
        if changed {
            let mut lines = self.edit_lines().to_vec();
            self.modified = true;
            self.push_line_undo();
            if let Some(line) = lines.get_mut(edit.index) {
                rewrite_edited_line(line, &speaker, &text);
            }
            self.replace_edit_lines(lines);
        }
        // The edited row stops being a field; re-measure without resetting the
        // anchor or the row height.
        self.reflow_pane_geometry();
        if resume {
            // Same path as the play button, so a finished track restarts from
            // the top instead of being left stopped. Opening the editor already
            // cancelled any glide, so there is nothing for it to land.
            let _ = self.toggle_playback();
        }
    }

    /// Remove the line open in the editor and close it.
    ///
    /// Destructive, so it goes through the confirmation dialog; the removal
    /// still lands on the undo stack like any other line edit, because a
    /// misclick on a neighbouring row is one keystroke away.
    fn delete_edit_line(&mut self) {
        // Typed text goes first: the removal shifts every index below it.
        let flushed = self.flush_line_edit_rows();
        let Some(edit) = self.line_edit.as_ref() else {
            return;
        };
        let index = edit.index;
        let mut lines = self.edit_lines().to_vec();
        if index >= lines.len() {
            self.cancel_inline_edit();
            return;
        }
        if !flushed {
            self.push_line_undo();
        }
        self.modified = true;
        lines.remove(index);
        self.replace_edit_lines(lines);
        self.line_edit = None;
        self.line_edit_resume = false;
        self.line_edit_document = None;
        self.reset_pane_geometry();
        self.toast = Some(("Deleted line".to_string(), Instant::now()));
    }

    /// Close the inline editor and drop what was typed. Used by Escape and by
    /// anything that replaces the document under it.
    fn cancel_inline_edit(&mut self) {
        self.line_edit_caret = None;
        if self.line_edit.take().is_some() {
            // A speaker added to the discarded row must not linger in the
            // legend or the chip colours.
            self.invalidate_derived();
            self.line_edit_resume = false;
            self.line_edit_document = None;
            // The delete confirmation belongs to the line being edited. Without
            // this it could outlive its editor and hold the pointer with no
            // dialog left to draw.
            self.delete_confirm = false;
            self.reset_pane_geometry();
        }
    }

    /// Write the edited row back to the transcript without closing the editor,
    /// ahead of an operation that moves line indices around. Returns whether
    /// anything was written; when it was, the undo snapshot is taken before the
    /// write so a single undo steps back over the write and the operation
    /// together.
    fn flush_line_edit_rows(&mut self) -> bool {
        let Some(edit) = self.line_edit.as_ref() else {
            return false;
        };
        let Some(row) = edit.current_row() else {
            return false;
        };
        let index = edit.index;
        let (speaker, text) = line_edit_row_fields(&row.speaker, &row.content);
        // A cleared field is not written: a blank line has no row of its own,
        // and a join reads the field directly anyway.
        if text.is_empty() {
            return false;
        }
        let unchanged = self.edit_lines().get(index).is_some_and(|line| {
            line.speaker == speaker && line_text(line) == text
        });
        if unchanged {
            return false;
        }
        let mut lines = self.edit_lines().to_vec();
        self.modified = true;
        self.push_line_undo();
        if let Some(line) = lines.get_mut(index) {
            rewrite_edited_line(line, &speaker, &text);
        }
        self.replace_edit_lines(lines);
        true
    }

    // ---- line editor history ----

    /// Key identifying the document the line editor writes to: the selected
    /// view's queue index, or `None` for the live transcript.
    fn edit_document(&self) -> Option<usize> {
        self.selected_view.as_ref().map(|view| view.index)
    }

    /// The lines of the document the line editor writes to.
    fn edit_lines(&self) -> &[DocLine] {
        self.document_lines()
    }

    fn edit_history_is_current(&self) -> bool {
        self.line_history.document == self.edit_document()
    }

    fn line_undo_available(&self) -> bool {
        self.edit_history_is_current() && !self.line_history.undo.is_empty()
    }

    fn line_redo_available(&self) -> bool {
        self.edit_history_is_current() && !self.line_history.redo.is_empty()
    }

    /// Record the current lines before mutating them. A new edit clears the
    /// redo stack, and a change of document starts a fresh history.
    fn push_line_undo(&mut self) {
        let document = self.edit_document();
        if !self.edit_history_is_current() {
            self.line_history = LineHistory {
                document,
                ..LineHistory::default()
            };
        }
        self.line_history.undo.push(self.edit_lines().to_vec());
        if self.line_history.undo.len() > LINE_UNDO_LIMIT {
            self.line_history.undo.remove(0);
        }
        self.line_history.redo.clear();
    }

    /// Replace the edited document's lines and rewrite its transcript file.
    fn replace_edit_lines(&mut self, lines: Vec<DocLine>) {
        if let Some(view) = self.selected_view.as_mut() {
            view.lines = lines;
            view.transcript = lines_text(&view.lines);
            if let Some(path) = &view.output {
                let _ = write_transcript_file(path, &view.transcript);
                backend::write_sidecar(path, &view.lines);
            }
        } else {
            self.lines = lines;
            self.refresh_transcript();
            if let Some(path) = &self.output_txt {
                backend::write_sidecar(path, &self.lines);
            }
        }
        self.invalidate_derived();
        // The sidecar's word times no longer match the document.
        self.karaoke_source = None;
    }

    /// Re-point the open editor at a line that still exists and rebuild its
    /// row from the transcript, so the field never shows stale text.
    fn sync_line_edit(&mut self) {
        let Some(index) = self.line_edit.as_ref().map(|edit| edit.index) else {
            return;
        };
        let len = self.edit_lines().len();
        if len == 0 {
            self.cancel_inline_edit();
            return;
        }
        self.point_line_edit(index.min(len - 1));
    }

    /// Keep the editor open on `index`, rebuilding its row from the transcript:
    /// a merge or split shifts every later index, so the row is rebuilt rather
    /// than patched field by field. The caret goes to the end of the text.
    fn point_line_edit(&mut self, index: usize) {
        self.point_line_edit_at(index, usize::MAX);
    }

    /// As [`Self::point_line_edit`], with the caret `caret` bytes into the
    /// rebuilt row, so a merge leaves it at the join and a split at the start of
    /// the line it just created.
    fn point_line_edit_at(&mut self, index: usize, caret: usize) {
        if self.line_edit.is_none() {
            return;
        }
        self.line_edit = LineEdit::inline_at(index, self.edit_lines(), caret);
        // The row was rebuilt from the document, so its speaker may have
        // changed with it (a merge takes the neighbour's name).
        self.invalidate_derived();
        // The window's line count changed: the spacers need fresh heights.
        self.reset_pane_geometry();
    }

    /// Restore the document as it was before the last line edit.
    fn undo_line_edit(&mut self) {
        if self.shown_output().is_none() || !self.line_undo_available() {
            return;
        }
        let Some(previous) = self.line_history.undo.pop() else {
            return;
        };
        self.line_history.redo.push(self.edit_lines().to_vec());
        self.modified = true;
        self.replace_edit_lines(previous);
        self.sync_line_edit();
        self.toast = Some(("Undid the last line edit".to_string(), Instant::now()));
    }

    /// Re-apply the line edit undone most recently.
    fn redo_line_edit(&mut self) {
        if self.shown_output().is_none() || !self.line_redo_available() {
            return;
        }
        let Some(next) = self.line_history.redo.pop() else {
            return;
        };
        self.line_history.undo.push(self.edit_lines().to_vec());
        self.modified = true;
        self.replace_edit_lines(next);
        self.sync_line_edit();
        self.toast = Some(("Redid the line edit".to_string(), Instant::now()));
    }

    /// Prepend the line open in the editor to the previous one. The editor
    /// stays open on the surviving line so its text can be seen to land.
    fn merge_prev_line(&mut self) {
        // Save what was typed first: the merge shifts the indices around, and
        // unflushed text would go with them.
        let flushed = self.flush_line_edit_rows();
        let Some(edit) = self.line_edit.as_ref() else {
            return;
        };
        let index = edit.index;
        let Some(row) = edit.current_row() else {
            return;
        };
        let (speaker, text) = line_edit_row_fields(&row.speaker, &row.content);
        let mut lines = self.edit_lines().to_vec();
        if index >= lines.len() {
            return;
        }
        if index == 0 {
            // Nothing above the first line; the flush already kept the edits.
            self.toast = Some(("No previous line to merge".to_string(), Instant::now()));
            return;
        }
        if !flushed {
            self.push_line_undo();
        }
        self.modified = true;
        let previous = line_text(&lines[index - 1]);
        // Where the two halves met, so the rebuilt field puts the caret there
        // instead of at the end of the joined text.
        let caret = previous.len() + usize::from(!previous.is_empty() && !text.is_empty());
        let joined = join_line_text(&previous, &text);
        rewrite_edited_line(&mut lines[index - 1], &speaker, &joined);
        lines.remove(index);
        self.replace_edit_lines(lines);
        // The merged line now sits where the previous one was, so the rows are
        // rebuilt around the new index rather than patched in place.
        self.point_line_edit_at(index - 1, caret);
        self.toast = Some(("Merged with previous line".to_string(), Instant::now()));
    }

    /// Append the next line to the one open in the editor.
    fn merge_next_line(&mut self) {
        let flushed = self.flush_line_edit_rows();
        let Some(edit) = self.line_edit.as_ref() else {
            return;
        };
        let index = edit.index;
        let Some(row) = edit.current_row() else {
            return;
        };
        let (speaker, text) = line_edit_row_fields(&row.speaker, &row.content);
        let mut lines = self.edit_lines().to_vec();
        if index >= lines.len() {
            return;
        }
        let merged = index + 1 < lines.len();
        let next_text = if merged {
            line_text(&lines[index + 1])
        } else {
            String::new()
        };
        let joined = if merged {
            join_line_text(&text, &next_text)
        } else {
            text.clone()
        };
        if !merged && lines[index].speaker == speaker && line_text(&lines[index]) == joined {
            // Nothing to merge; the flush above already kept any edit.
            self.toast = Some(("No next line to merge".to_string(), Instant::now()));
            return;
        }
        if !flushed {
            self.push_line_undo();
        }
        self.modified = true;
        // Where the two halves met, so the rebuilt field puts the caret there.
        let caret = text.len() + usize::from(!text.is_empty() && !next_text.is_empty());
        rewrite_edited_line(&mut lines[index], &speaker, &joined);
        if merged {
            lines.remove(index + 1);
        }
        self.replace_edit_lines(lines);
        // The next line's row is gone, so the window is rebuilt rather than
        // left pointing at an index that no longer exists.
        self.point_line_edit_at(index, caret);
        self.toast = Some((
            if merged {
                "Merged with next line".to_string()
            } else {
                "No next line to merge".to_string()
            },
            Instant::now(),
        ));
    }

    /// Split the edited line at the editor's caret.
    fn split_line_at_cursor(&mut self) {
        // Neighbouring rows are saved first: the split inserts a line and
        // shifts every later index.
        let flushed = self.flush_line_edit_rows();
        let Some(edit) = self.line_edit.as_ref() else {
            return;
        };
        let index = edit.index;
        let Some(row) = edit.current_row() else {
            return;
        };
        let speaker = row.speaker.trim().to_string();
        let Some((left, right)) = split_at_caret(row) else {
            self.toast = Some(("Cannot split here".to_string(), Instant::now()));
            return;
        };
        // The ratio below is measured against the whole line's text.
        let text = row.content.text().trim_end().replace(['\n', '\r'], " ");
        let (line_start, next_start) = {
            let lines = self.edit_lines();
            if index >= lines.len() {
                return;
            }
            (
                lines[index].start,
                lines.get(index + 1).and_then(|line| line.start),
            )
        };
        let start = line_start.unwrap_or(0.0);
        let end = next_start
            .or(self.shown_duration())
            .unwrap_or(start + 5.0)
            .max(start + 0.1);
        // The split lands between this line's start and the next line's start,
        // in proportion to how much of the text stays on the left.
        let total = text.chars().count().max(1) as f32;
        let ratio = (left.chars().count() as f32 / total).clamp(0.05, 0.95);
        let right_start = line_start.map(|_| start + (end - start) * ratio);

        if !flushed {
            self.push_line_undo();
        }
        self.modified = true;
        let mut lines = self.edit_lines().to_vec();
        rewrite_edited_line(&mut lines[index], &speaker, &left);
        let prefix = match right_start {
            Some(right_start) => format!("{} ", format_timestamp(right_start as f64)),
            None => String::new(),
        };
        let raw = if speaker.is_empty() {
            format!("{prefix}{right}")
        } else {
            format!("{prefix}{speaker}: {right}")
        };
        lines.insert(
            index + 1,
            DocLine {
                raw,
                speaker: speaker.clone(),
                start: right_start,
                confidence: Some(HUMAN_CONFIDENCE),
            },
        );
        self.replace_edit_lines(lines);
        // The editor follows the text that was cut off, with the caret at the
        // start of the new line: that is where typing continues.
        self.point_line_edit_at(index + 1, 0);
        self.toast = Some(("Split line at cursor".to_string(), Instant::now()));
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
                    self.stage = if label.is_empty() {
                        stage.clone()
                    } else {
                        label
                    };
                    self.stage_id = stage;
                    self.progress = progress.clamp(0.0, 1.0);
                }
                Event::Backend {
                    backend,
                    device,
                    fallback,
                } => {
                    self.gpu_actual = Some(match device {
                        Some(device) if !device.is_empty() => format!("{backend} · {device}"),
                        _ => backend,
                    });
                    let fallback = fallback;
                    self.gpu_fallback = fallback.clone();
                    if let Some(reason) = fallback {
                        self.disable_gpu_after_failure(&reason);
                    }
                }
                Event::Work { done, total } => {
                    self.work_done = done;
                    self.work_total = total;
                    let now = Instant::now();
                    let sample = match self.eta_samples.back() {
                        Some((last, audio)) => {
                            let wall = now.duration_since(*last).as_secs_f32();
                            (wall >= 0.5 && done - *audio > 0.5).then_some((now, done))
                        }
                        None => Some((now, done)),
                    };
                    if let Some(sample) = sample {
                        self.eta_samples.push_back(sample);
                        // Three deltas reject a stalled phase without lagging
                        // behind a real change in speed.
                        while self.eta_samples.len() > 4 {
                            self.eta_samples.pop_front();
                        }
                        let points: Vec<(Instant, f64)> =
                            self.eta_samples.iter().copied().collect();
                        let mut rates: Vec<f32> = points
                            .windows(2)
                            .filter_map(|pair| {
                                let wall = pair[1].0.duration_since(pair[0].0).as_secs_f32();
                                let audio = (pair[1].1 - pair[0].1) as f32;
                                (wall > 0.0 && audio > 0.0).then(|| audio / wall)
                            })
                            .collect();
                        if !rates.is_empty() {
                            rates.sort_by(f32::total_cmp);
                            self.eta_throughput = App::usable_throughput(median(&rates));
                        }
                    }
                }
                Event::Diarize { fraction } => {
                    self.diarize_fraction = fraction.clamp(0.0, 1.0);
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
                    let keep_view = self.keep_pinned_view() || self.keeps_finished_view();
                    self.current = self.find_queue_index(&path);
                    self.current_index = self.current;
                    if let Some(index) = self.current {
                        self.queue[index].status = ItemStatus::Running;
                        // Persist at once: a crash before the file finishes must
                        // restore as Interrupted, not as still-queued.
                        self.save_queue();
                    }
                    if !keep_view {
                        // The edited document is about to leave the screen.
                        self.commit_inline_edit(false);
                        self.selected = None;
                        self.selected_view = None;
                        self.view_error = None;
                    }
                    self.status = Status::Running;
                    self.stage = "loading".to_string();
                    self.stage_id = "loading".to_string();
                    self.progress = 0.0;
                    self.progress_shown = 0.0;
                    self.eta_shown = None;
                    self.work_done = 0.0;
                    self.work_total = 0.0;
                    self.diarize_fraction = 0.0;
                    self.eta_samples.clear();
                    // Per file: the pace is re-measured from this file's work.
                    self.eta_throughput = None;
                    self.eta_pending = 0;
                    self.elapsed_final = None;
                    self.transcript.clear();
                    self.lines.clear();
                    self.pending_lines.clear();
                    self.reveal_accum = 0.0;
                    self.completion_pending = false;
                    self.transcript_dirty = false;
                    self.follow_offset = None;
                    self.scroll_target = None;
                    self.scroll_anim = None;
                    self.follow_issue = None;
                    *self.follow_row_height.borrow_mut() = None;
                    self.reset_pane_geometry();
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
                    confidence,
                } => {
                    let stamp = format_secs(start as f64);
                    // A cleared label must not leave a leading colon behind.
                    let raw = if speaker.is_empty() {
                        format!("[{stamp}] {text}")
                    } else {
                        format!("[{stamp}] {speaker}: {text}")
                    };
                    // Queue it; the textbox reveals lines at a speech-like pace.
                    self.pending_lines.push_back(DocLine {
                        raw,
                        speaker,
                        start: Some(start),
                        confidence: Some(confidence),
                    });
                }
                Event::Done { txt, stats, .. } => {
                    if self.learn_from_stats(&stats) {
                        self.save_settings();
                    }

                    // Finish revealing any queued utterances immediately.
                    while let Some(line) = self.pending_lines.pop_front() {
                        self.reveal_line(line);
                    }
                    self.reveal_accum = 0.0;
                    self.stage = "done".to_string();
                    self.stage_id = "done".to_string();
                    // Target 100%; the bar eases there over the next ticks.
                    self.progress = 1.0;
                    // A finished run is always the live buffer: the item it
                    // belongs to is only opened later, when the user picks it.
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
                    let was_gpu = self.gpu_selected();
                    self.disable_gpu_after_failure(&message);
                    self.gpu_retry_pending = was_gpu;
                    self.stage = "error".to_string();
                    self.stage_id = "error".to_string();
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
                    Self::clear_gpu_guard();
                    let retry_on_cpu = std::mem::take(&mut self.gpu_retry_pending);
                    if retry_on_cpu
                        && self
                            .queue
                            .iter()
                            .any(|item| matches!(item.status, ItemStatus::Failed(_)))
                    {
                        for item in &mut self.queue {
                            if matches!(item.status, ItemStatus::Failed(_)) {
                                item.status = ItemStatus::Pending;
                                item.transcript = None;
                                item.stats = None;
                                item.turns.clear();
                                item.peaks = Arc::new(Vec::new());
                            }
                        }
                        self.invalidate_derived();
                        self.save_queue();
                        self.begin_batch(true);
                        return Task::batch(tasks);
                    }
                    self.batch = false;
                    self.current = None;
                    self.stage = "done".to_string();
                    self.stage_id = "done".to_string();
                    self.progress = 1.0;
                    self.elapsed_final = self.run_start.map(|s| s.elapsed().as_secs_f64());
                    self.eta_shown = None;
                    self.toast = None;
                    self.save_queue();
                    // Pro chains a follow-up pass for files queued during the
                    // run. The free tier stops after one item and waits for a
                    // manual Transcribe.
                    if self.can(license::Feature::Batch)
                        && self
                            .queue
                            .iter()
                            .any(|item| matches!(item.status, ItemStatus::Pending))
                    {
                        self.begin_batch(true);
                    } else if self.has_results() {
                        self.status = Status::Done;
                        // Ticks stop as soon as the run leaves `Status::Running`,
                        // so there is no later frame to ease the bars in or run
                        // the deferred completion. Snap them and present now.
                        self.progress = 1.0;
                        self.progress_shown = 1.0;
                        self.overall_shown = 1.0;
                        self.current_index = None;
                        self.completion_pending = false;
                        if self.present_completion() {
                            tasks.push(scrollable::snap_to(
                                summary_id(),
                                scrollable::RelativeOffset { x: 0.0, y: 0.0 },
                            ));
                        }
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

    /// Completed items from this run, in queue order, with their display name.
    fn batch_runs(&self) -> Vec<(String, &backend::Stats)> {
        self.queue
            .iter()
            .filter(|item| matches!(item.status, ItemStatus::Done))
            // Restrict the automatic summary to the files this run processed,
            // not every completed item left in the queue from earlier runs.
            .filter(|item| self.batch_paths.is_empty() || self.batch_paths.contains(&item.path))
            .filter_map(|item| {
                let stats = item.stats.as_ref()?;
                let name = item
                    .audio
                    .as_ref()
                    .or(item.transcript.as_ref())
                    .or(Some(&item.path))
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("file")
                    .to_string();
                Some((name, stats))
            })
            .collect()
    }

    /// Cross-run tabulation shown ahead of the per-file blocks when a queue
    /// processed more than one file. Speaker labels are per file, so speaker
    /// counts are listed per row rather than summed into a meaningless total.
    fn aggregate_table(runs: &[(String, &backend::Stats)]) -> (Vec<String>, Vec<Vec<String>>) {
        let headers = ["File", "Length", "Run", "Speed", "Spk", "Words"]
            .iter()
            .map(|cell| cell.to_string())
            .collect();
        let mut audio_total = 0.0;
        let mut run_total = 0.0;
        let mut word_total = 0usize;
        let mut rows = Vec::new();
        for (name, stats) in runs {
            let speed = if stats.run_seconds > 0.0 {
                stats.audio_seconds / stats.run_seconds
            } else {
                0.0
            };
            let speakers = match stats.num_speakers {
                Some(count) => count.to_string(),
                None => stats.speakers.to_string(),
            };
            let words: usize = stats.speaker_stats.values().map(|stat| stat.words).sum();
            audio_total += stats.audio_seconds;
            run_total += stats.run_seconds;
            word_total += words;
            rows.push(vec![
                ellipsize_middle(name, 18),
                format_secs(stats.audio_seconds),
                format_secs(stats.run_seconds),
                format!("{speed:.1}\u{00d7}"),
                speakers,
                words.to_string(),
            ]);
        }
        let speed = if run_total > 0.0 {
            audio_total / run_total
        } else {
            0.0
        };
        rows.push(vec![
            format!("{} files", runs.len()),
            format_secs(audio_total),
            format_secs(run_total),
            format!("{speed:.1}\u{00d7}"),
            String::new(),
            word_total.to_string(),
        ]);
        (headers, rows)
    }

    fn stats_lines(&self) -> Vec<SummaryLine> {
        let mut lines: Vec<SummaryLine> = Vec::new();
        let runs = self.batch_runs();
        if runs.len() > 1 {
            let (headers, rows) = Self::aggregate_table(&runs);
            lines.push(SummaryLine::Table(headers, rows));
        }
        for item in &self.queue {
            if !matches!(item.status, ItemStatus::Done) {
                continue;
            }
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
            if !lines.is_empty() && !matches!(lines.last(), Some(SummaryLine::Table(..))) {
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
            format!(
                "Speed: {:.1}\u{00d7} realtime \u{00b7} {} audio in {}",
                speed,
                human_duration(s.audio_seconds),
                human_duration(s.run_seconds)
            ),
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

    /// Fold a finished file's timings into the priors the readout prices with.
    /// Returns whether anything changed, so the caller can persist it.
    ///
    /// Only measurements that describe the phase they are stored for:
    ///
    /// * Throughput needs real ASR time and real audio. `usable_throughput`
    ///   rejects a rate no decode could have produced.
    /// * Diarization is learned only when a pass actually ran. A one-speaker
    ///   run, ASR-only mode and a cached pass all return immediately, and the
    ///   core still reports their leftover interval as `diarize_seconds`;
    ///   blending that is what dragged the stored rate to 0.0018. A real pass
    ///   over any audio takes more than half a second, so anything under that is
    ///   a skipped pass, not a measurement.
    /// * The post-ASR tail is never learned. `prepare_seconds` is the pre-ASR
    ///   clip-building gap in the classic path and the attribution window in the
    ///   overlap path, and the GUI gets no mode signal, so half of every sample
    ///   would describe the wrong phase. The estimate uses the stored value or
    ///   the built-in default instead.
    fn learn_from_stats(&mut self, stats: &backend::Stats) -> bool {
        if stats.audio_seconds <= 0.0 {
            return false;
        }
        let mut learned = false;
        if stats.asr_seconds > 0.0 {
            let sample = (stats.audio_seconds / stats.asr_seconds) as f32;
            if let Some(sample) = Self::usable_throughput(sample) {
                self.eta_prior = Some(match self.eta_prior {
                    Some(prior) => prior * 0.5 + sample * 0.5,
                    None => sample,
                });
                learned = true;
            }
        }
        const DIARIZE_FLOOR_SECONDS: f64 = 0.5;
        let diarized = stats.num_speakers != Some(1)
            && stats.diarize_seconds >= DIARIZE_FLOOR_SECONDS;
        if diarized {
            let sample = (stats.diarize_seconds / stats.audio_seconds) as f32;
            self.diarize_prior = Some(match self.diarize_prior {
                Some(prior) => prior * 0.5 + sample * 0.5,
                None => sample,
            });
            learned = true;
        }
        learned
    }

    /// What the run is doing and how long it has left.
    ///
    /// The tail is sequential: after ASR, whatever diarization is still behind
    /// has to finish and then the attribution/write pass runs, so their
    /// estimates add. Only inside ASR do decoding and diarization overlap, and
    /// there the longer of the two is the wait.
    ///
    /// A phase with no rate to price it with returns `remaining: None` rather
    /// than borrowing another phase's rate: that is what stopped a finished ASR
    /// pass from printing a five-second estimate over a three-minute tail.
    fn estimate_remaining(&self) -> Option<EtaView> {
        if !matches!(self.status, Status::Running) {
            return None;
        }
        let phase = self.eta_phase();
        let duration = self.eta_duration();
        // Attribution and writing, which no event reports: the migrated prior
        // or the built-in default. Unknown audio length means unknown tail.
        let tail = (duration > 0.0)
            .then(|| duration * self.prepare_prior.unwrap_or(PREPARE_DEFAULT).max(0.0));

        let current = match phase {
            // Nothing measures decode or the final hand-off, so neither gets a
            // number.
            EtaPhase::Preparing | EtaPhase::Finishing => None,
            EtaPhase::Transcribing => self
                .eta_throughput
                .or(self.eta_prior)
                .and_then(Self::usable_throughput)
                .map(|throughput| {
                    let audio_left = (self.work_total - self.work_done).max(0.0) as f32;
                    // Diarization runs beside ASR in the overlap schedule and
                    // can finish after it; only count it while it is mid-flight.
                    let diarizing =
                        self.diarize_fraction > 0.0 && self.diarize_fraction < 1.0;
                    let diarize_left = if diarizing {
                        self.diarize_remaining(duration).unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    (audio_left / throughput).max(diarize_left) + tail.unwrap_or(0.0)
                }),
            EtaPhase::Diarizing => self
                .diarize_remaining(duration)
                .map(|left| left + tail.unwrap_or(0.0)),
            EtaPhase::Writing => tail,
        };

        // Files queued after the current one. Unknown durations use the same
        // average as `queue_weight`, and the same diarize rate as above.
        let start = self.current_index.map_or(0, |index| index + 1);
        let pending: Vec<&QueueItem> = self
            .queue
            .iter()
            .skip(start)
            .filter(|item| matches!(item.status, ItemStatus::Pending))
            .collect();
        let known: Vec<f32> = pending
            .iter()
            .filter_map(|item| item.duration.map(|duration| duration as f32))
            .filter(|duration| *duration > 0.0)
            .collect();
        let fallback = if known.is_empty() {
            1.0
        } else {
            known.iter().sum::<f32>() / known.len() as f32
        };
        let throughput = self
            .eta_throughput
            .or(self.eta_prior)
            .and_then(Self::usable_throughput);
        // A one-speaker setting never diarizes, so a queued file costs only
        // its decode and ASR.
        let diarize_rate = if self.num_speakers == Some(1) {
            0.0
        } else {
            self.diarize_prior.unwrap_or(DIARIZE_DEFAULT).max(0.0)
        };
        let queued: f32 = match throughput {
            Some(rate) => pending
                .iter()
                .map(|item| queue_weight(item, fallback) * (1.0 / rate + diarize_rate))
                .sum(),
            None => 0.0,
        };

        let remaining = current.map(|current| current + queued);
        Some(EtaView {
            phase,
            remaining: remaining.filter(|left| left.is_finite()).map(|left| left.max(0.0)),
        })
    }

    /// Which phase the pipeline's last stage belongs to.
    ///
    /// The core writes the transcript under the `transcription` stage with its
    /// own label, so the write is told apart by the label rather than the id.
    fn eta_phase(&self) -> EtaPhase {
        match self.stage_id.as_str() {
            "loading" | "decoding" | "starting" | "idle" => EtaPhase::Preparing,
            "diarization" => EtaPhase::Diarizing,
            "done" => EtaPhase::Finishing,
            "transcription" if self.stage.starts_with("Writing") => EtaPhase::Writing,
            _ => EtaPhase::Transcribing,
        }
    }

    /// The audio length the phase rates are priced against: the decoder's own
    /// total when it has one, the probed duration otherwise.
    fn eta_duration(&self) -> f32 {
        self.work_total
            .max(self.shown_duration().unwrap_or(0.0) as f64)
            .max(0.0) as f32
    }

    /// Diarization still to run, when there is a rate to price it with.
    ///
    /// The prior is only `Some` after a pass that actually diarized (see
    /// `learn_from_stats`), so a skipped pass does not price this at zero.
    fn diarize_remaining(&self, duration: f32) -> Option<f32> {
        let rate = self.diarize_prior?;
        Some((1.0 - self.diarize_fraction.clamp(0.0, 1.0)) * duration * rate.max(0.0))
    }

    /// Audio seconds per wall second above which a sample cannot be real.
    ///
    /// A run served entirely from the segment cache reports a large
    /// `audio_seconds` against almost no ASR time, and a run whose Work deltas
    /// arrive back to back looks the same. Learning either would drive every
    /// later estimate to zero, which is how "finishing..." became permanent.
    fn usable_throughput(rate: f32) -> Option<f32> {
        (rate.is_finite() && rate > 0.02 && rate <= MAX_THROUGHPUT).then_some(rate)
    }

    /// A stored diarization rate below this is a skipped pass, not a
    /// measurement: no real pass can run faster than 1% of the audio length.
    /// Loaded as unknown, which restores the built-in default.
    fn usable_diarize(rate: f32) -> Option<f32> {
        (rate.is_finite() && rate >= 0.01).then_some(rate)
    }

    /// Same for the post-ASR tail, whose stored values were dragged down by a
    /// value that was never attribution in the first place.
    fn usable_prepare(rate: f32) -> Option<f32> {
        (rate.is_finite() && rate >= 0.001).then_some(rate)
    }

    fn timer_text(&self) -> String {
        let elapsed = self.elapsed().unwrap_or_else(|| "00:00".to_string());
        if !matches!(self.status, Status::Running) {
            return elapsed;
        }
        let Some(view) = self.eta_shown else {
            return elapsed;
        };
        let phase = view.phase.label();
        match view.remaining.filter(|left| left.is_finite()) {
            Some(left) => format!(
                "{elapsed} \u{00b7} {phase} \u{00b7} ~{} left",
                eta_bucket(left)
            ),
            // No rate for this phase: its name, and no number to be wrong.
            None => format!("{elapsed} \u{00b7} {phase}\u{2026}"),
        }
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
                // Log lines wrap to the full width, so the rail needs a lane of
                // its own here too.
                .spacing(theme::SPACE_SM)
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

    /// Distinct speakers in the transcript, plus whatever the open line editor
    /// is showing, sorted into the rename dialog's natural order.
    ///
    /// The edited row is not in the document yet, so a speaker assigned to it
    /// would otherwise have no palette colour and no place in the chip's cycle.
    fn speaker_order(&self) -> Vec<String> {
        if let Some(order) = self.speaker_order_cache.borrow().as_ref() {
            return order.clone();
        }
        let mut order = distinct_speaker_order(self.shown_lines());
        if let Some(edit) = self.line_edit.as_ref() {
            for row in &edit.rows {
                if !row.speaker.is_empty() && !order.contains(&row.speaker) {
                    order.push(row.speaker.clone());
                }
            }
        }
        // Sorted, so a colour belongs to a name rather than to the order the
        // speakers happened to first appear in.
        sort_speaker_names(&mut order);
        *self.speaker_order_cache.borrow_mut() = Some(order.clone());
        order
    }

    /// True when diarization ran and found a single voice, so every line lost
    /// its label. Worth saying out loud: an unlabelled transcript otherwise
    /// looks like a failure.
    fn only_one_speaker(&self) -> bool {
        self.pipelines_diarize()
            && !self.shown_turns().is_empty()
            && self.speaker_order().is_empty()
            && self
                .shown_lines()
                .iter()
                .any(|line| !line.raw.trim().is_empty())
    }

    /// True when the visible transcript carries ASR confidence values.
    fn has_confidence(&self) -> bool {
        self.shown_lines()
            .iter()
            .any(|line| line.confidence.is_some())
    }

    /// Line indices sorted by start time (untimed lines last), stable.
    fn display_order(&self) -> Vec<usize> {
        if let Some(order) = self.display_order_cache.borrow().as_ref() {
            return order.clone();
        }
        let lines = self.shown_lines();
        let mut order = sorted_display_order(lines);
        if let Some(speaker) = &self.speaker_filter {
            if speaker == LOW_CONFIDENCE_FILTER {
                order.retain(|index| lines.get(*index).is_some_and(is_low_confidence));
            } else {
                order.retain(|index| {
                    lines
                        .get(*index)
                        .is_some_and(|line| line.speaker == *speaker)
                });
            }
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

    /// Row height used to place the virtualisation spacers.
    ///
    /// Measured from the height the rendered window actually had last frame
    /// (`content - spacers`) and latched while it stays in the same ballpark.
    /// Deriving it from `content / rows` on every frame is self-referential —
    /// the reported content already contains the spacers built from it — so
    /// the smallest difference between the estimate and the real rows was
    /// amplified by `rows / window` and crept the pane by tens of pixels on
    /// every scroll event, with no scroll of its own.
    fn pane_line_height(&self, fallback: f32) -> f32 {
        let (start, end, top, bottom) = self.pane_window.get();
        let rendered = end.saturating_sub(start);
        let measured = if rendered > 0 && self.transcript_content > top + bottom {
            (self.transcript_content - top - bottom) / rendered as f32
        } else {
            0.0
        };
        let mut latched = self.pane_row_height.get();
        // A glide reports a new offset every frame, so the window it measures
        // against is moving; keep the latch it started with rather than
        // re-measuring (and possibly re-latching) against every frame of it.
        if self.scroll_anim.is_some() {
            if let Some(height) = latched {
                return height.max(12.0);
            }
        }
        if measured > 1.0 {
            match latched {
                // A real size change (zoom, bubble mode, re-wrap) recalibrates;
                // a pixel of drift while scrolling must not.
                Some(height) if (measured - height).abs() <= height * 0.5 => {}
                _ => latched = Some(measured),
            }
        }
        self.pane_row_height.set(latched);
        latched.unwrap_or(fallback).max(12.0)
    }

    /// Forget the virtualisation geometry after anything that changes row
    /// heights (new view, zoom, bubble or joined mode).
    /// Re-measure the rendered window while keeping the measured row height and
    /// the anchor. Used when a single row changes size - the inline editor
    /// opening, moving to a neighbouring row, or closing: dropping the latched
    /// height puts the pane on its estimate for one frame, which reads as a
    /// jump even though the content never moved.
    fn reflow_pane_geometry(&mut self) {
        self.pane_window.set((0, 0, 0.0, 0.0));
    }

    fn reset_pane_geometry(&mut self) {
        self.pane_window.set((0, 0, 0.0, 0.0));
        self.pane_row_height.set(None);
        self.pane_start.set(None);
    }

    /// True while the live transcript is streaming into the pane.
    fn live_streaming(&self) -> bool {
        self.selected_view.is_none() && matches!(self.status, Status::Running)
    }

    /// The line Follow should keep in view. Unlike [`Self::active_line_index`]
    /// there is no first-line fallback: falling back centres the top of the
    /// transcript, which is the "Follow scrolled to the top" jump. No match
    /// means the playhead is before the first line, so there is nothing above
    /// the viewport to follow.
    fn follow_line_index(&self) -> Option<usize> {
        if self.live_streaming() && !self.scroll_seek {
            self.display_order().last().copied()
        } else {
            self.line_for_playhead(self.playhead)
        }
    }

    /// The scroll the follow logic wants next, or `None` when the transcript is
    /// already close enough that another programmatic `scroll_to` would only
    /// make the pane shimmer. `centred` marks the playhead-tracking path, the
    /// only one that may ease instead of landing.
    ///
    /// `scroll_to` is a one-shot jump, so every call that is not strictly
    /// needed shows up as a visible step. Three things kept it firing:
    ///
    /// * the centred target was derived from `content / rows`, an average row
    ///   height that drifts as streamed text grows, so the target kept moving
    ///   even with nothing else changing — hence the cached, stable estimate in
    ///   [`Self::centred_follow_target`];
    /// * a target the scrollable clamps (or rounds) can never be matched by the
    ///   reported offset, so comparing against `transcript_offset` alone was
    ///   permanently out of the deadband — hence [`FollowIssue`];
    /// * the offset arrives a frame after the scroll, so the same target was
    ///   requested again in the meantime.
    ///
    /// Follow is therefore edge-triggered: a scroll is emitted only when the
    /// desired target moved beyond [`FOLLOW_DEADBAND`] away from the last one,
    /// and a retry at an unchanged situation is allowed once and then never
    /// again. With a paused player — static playhead, line, content and
    /// viewport — this returns `None` on every tick.
    ///
    /// `active` is the line to centre while following playback. The live
    /// streamed transcript pins to the bottom instead, because centring the
    /// newest line with the average-height estimate makes it jump as rows are
    /// added.
    fn next_follow_target(&self, active: Option<usize>) -> Option<FollowTarget> {
        let content = self.transcript_content;
        let viewport = self.transcript_viewport;
        // Nothing can scroll while the content fits the pane. Checked before
        // `content - viewport` on purpose: a transient shrink during a re-wrap
        // or a joined-preview rebuild used to make the bottom 0, which looked
        // like "scroll to the top".
        if viewport <= 0.0 || content <= viewport {
            return None;
        }
        let bottom = content - viewport;
        let (desired, centred) = if self.follow_playhead {
            if self.live_streaming() && !self.scroll_seek {
                (bottom, false)
            } else {
                (self.centred_follow_target(active?)?, true)
            }
        } else if self.selected_view.is_none() && self.transcript_at_bottom {
            // The live view has not been scrolled away: stay at the bottom.
            (bottom, false)
        } else {
            return None;
        };
        // Follow never jumps to the top of the transcript. A clamped 0 comes
        // from centring the first row (the playhead sits before it, or no line
        // matches the playhead), and that yanks the pane away from wherever
        // the playhead actually is. When the pane is already at the top the
        // offset check below settles it.
        if self.follow_playhead && desired <= FOLLOW_DEADBAND {
            return None;
        }
        let settled = |at: f32| (at - desired).abs() <= FOLLOW_DEADBAND;
        // Nothing to do when the view already sits on the target, or a scroll
        // towards it is still in flight.
        if settled(self.transcript_offset) || self.scroll_target.is_some_and(settled) {
            return None;
        }
        let Some(issue) = self.follow_issue else {
            return Some(FollowTarget { y: desired, centred });
        };
        // Edge-triggered. If the target is the one we last asked for there is
        // nothing to do: either the scroll landed (the reported offset is only
        // approximate) or the scrollable clamped it away. Re-requesting it
        // cannot move the view, so the tick goes quiet — which is what makes a
        // paused player stay still however badly the last target settled.
        if settled(issue.target) {
            return None;
        }
        // The target moved. Only a genuine change of inputs is worth another
        // scroll; with the same line, playhead, content and viewport this is a
        // retry, and a retry is allowed once and then never again.
        let same = issue.same_situation(active, self.playhead, content, viewport);
        if same && (issue.retried || issue.at.elapsed() < FOLLOW_MIN_INTERVAL) {
            return None;
        }
        Some(FollowTarget { y: desired, centred })
    }

    /// Centred target for a playback line, using a row height that only moves
    /// when the row count changes (or the content height jumps materially).
    /// Recomputed from `content / rows` it would drift with every streamed
    /// character and nudge the view.
    fn centred_follow_target(&self, active: usize) -> Option<f32> {
        let content = self.transcript_content;
        let viewport = self.transcript_viewport;
        if content <= 0.0 || viewport <= 0.0 {
            return None;
        }
        let order = self.display_order();
        let rows = order.len().max(1);
        let rank = order.iter().position(|index| *index == active)?;
        // The first row has nothing above it, so centring it can only pull the
        // pane towards the top. Follow declines instead of jumping up.
        if rank == 0 {
            return None;
        }
        let average = content / rows as f32;
        let mut cached = self.follow_row_height.borrow_mut();
        let row_height = match *cached {
            Some((cached_rows, height))
                if cached_rows == rows && (average - height).abs() <= height * 0.25 =>
            {
                height
            }
            _ => {
                *cached = Some((rows, average));
                average
            }
        };
        Some(
            ((rank as f32 + 0.5) * row_height - viewport / 2.0)
                .clamp(0.0, (content - viewport).max(0.0)),
        )
    }

    /// Record a follow scroll we are about to issue.
    fn note_follow_issue(&mut self, target: f32, active: Option<usize>) {
        let content = self.transcript_content;
        let viewport = self.transcript_viewport;
        // A retry is a second attempt at the same situation that has not moved
        // the view. It is allowed once; after that the tick stays put until an
        // input changes.
        let retried = self.follow_issue.is_some_and(|prev| {
            (self.transcript_offset - prev.offset).abs() <= FOLLOW_DEADBAND
                && prev.same_situation(active, self.playhead, content, viewport)
        });
        self.follow_issue = Some(FollowIssue {
            at: Instant::now(),
            target,
            offset: self.transcript_offset,
            active,
            playhead: self.playhead,
            content,
            viewport,
            retried,
        });
    }

    /// Row height the glide decision measures a move against.
    fn follow_row_estimate(&self) -> f32 {
        self.pane_line_height((16.0 * self.font_scale * 1.8).max(24.0))
    }

    /// Ask the follow logic for the next scroll and record it. A move that
    /// should ease starts or retargets the glide and returns `None` (the
    /// animator issues its own scrolls); anything else is returned for the
    /// caller to send at once.
    fn follow_scroll(&mut self, active: Option<usize>) -> Option<f32> {
        let target = self.next_follow_target(active)?;
        self.note_follow_issue(target.y, active);
        let from = self
            .scroll_anim
            .map_or(self.transcript_offset, |anim| anim.expected);
        let jump_in_flight = self.scroll_anim.is_some_and(|anim| anim.jump);
        let shape = if self.popup_open() {
            Glide::Snap
        } else {
            glide_shape(
                target.centred,
                false,
                target.y - from,
                self.follow_row_estimate(),
                self.snap_next_follow,
                jump_in_flight,
            )
        };
        self.snap_next_follow = false;
        match shape {
            Glide::Ease { tau } => {
                self.start_glide(target.y, tau, jump_in_flight);
                None
            }
            Glide::Snap => {
                // A snap supersedes any glide, which must not keep writing
                // offsets towards a target the pane is no longer heading for.
                self.scroll_anim = None;
                self.scroll_target = Some(target.y);
                Some(target.y)
            }
        }
    }

    /// Start easing towards `target`, or retarget a glide already running so
    /// its position and velocity carry over instead of restarting.
    fn start_glide(&mut self, target: f32, tau: Duration, jump: bool) {
        match self.scroll_anim.as_mut() {
            Some(anim) => {
                anim.target = target;
                anim.tau = tau;
                anim.jump = jump;
            }
            None => {
                let expected = self.transcript_offset;
                self.follow_offset = Some(expected);
                // The first step is roughly a fifth of the remaining distance,
                // so the "was that us?" tolerance starts at the size of it.
                let step = (target - expected).abs() * glide_fraction(FOLLOW_GLIDE_FRAME, tau);
                self.scroll_anim = Some(ScrollAnim {
                    target,
                    expected,
                    last: Instant::now(),
                    step,
                    tau,
                    jump,
                });
            }
        }
        self.scroll_target = Some(target);
    }

    /// True when this seek is a single deliberate gesture rather than part of a
    /// scrub or a held key. Two seeks inside [`FOLLOW_SEEK_QUIET`] are one
    /// gesture, and a glide per event would leave the pane trailing the pointer,
    /// so only the isolated one eases.
    fn seek_is_isolated(&mut self, now: Instant) -> bool {
        let isolated = self
            .last_seek
            .is_none_or(|last| now.saturating_duration_since(last) >= FOLLOW_SEEK_QUIET);
        self.last_seek = Some(now);
        isolated
    }

    /// Move the pane to a line the user asked for. A single click eases at any
    /// distance; a scrub or a held key lands at once.
    fn seek_to_line(&mut self, line: usize) -> Task<Message> {
        let isolated = self.seek_is_isolated(Instant::now());
        let Some(target) = self.follow_target(line) else {
            return Task::none();
        };
        let active = self.follow_line_index();
        self.note_follow_issue(target, active);
        let shape = if isolated && !self.popup_open() {
            glide_shape(
                true,
                true,
                target - self.transcript_offset,
                self.follow_row_estimate(),
                false,
                false,
            )
        } else {
            Glide::Snap
        };
        match shape {
            Glide::Ease { tau } => {
                self.start_glide(target, tau, true);
                Task::none()
            }
            Glide::Snap => {
                self.scroll_anim = None;
                self.scroll_target = Some(target);
                scrollable::scroll_to(
                    transcript_id(),
                    scrollable::AbsoluteOffset { x: 0.0, y: target },
                )
            }
        }
    }

    /// True when a reported offset is one the running glide asked for. Tolerates
    /// one step of lag: the scrollbar reports the previous step while the next
    /// one is being issued.
    fn glide_covers(&self, offset: f32) -> bool {
        self.scroll_anim.is_some_and(|anim| {
            (offset - anim.expected).abs() <= FOLLOW_STUCK_STEP + anim.step
        })
    }

    /// One step of the running glide, or `None` when it is not due yet (the
    /// step rate is capped) or there is nothing to send.
    fn advance_glide(&mut self, now: Instant) -> Option<f32> {
        let anim = self.scroll_anim?;
        let dt = now.saturating_duration_since(anim.last);
        if dt < FOLLOW_GLIDE_FRAME {
            return None;
        }
        let (next, landed) = glide_advance(anim.expected, anim.target, dt, anim.tau);
        let step = next - anim.expected;
        if !landed && step.abs() < FOLLOW_GLIDE_MIN_STEP {
            // Not worth a scroll (and its echo) for movement nobody can see;
            // keep the state so the next frame steps further.
            return None;
        }
        self.follow_offset = Some(next);
        if landed {
            // The in-flight marker is left in place on purpose: the landing
            // report still has to be recognised as ours, or it would clear the
            // hovered row once per advance.
            self.scroll_anim = None;
        } else {
            self.scroll_anim = Some(ScrollAnim {
                expected: next,
                last: now,
                step,
                ..anim
            });
        }
        Some(next)
    }

    /// Stop any glide. `land` sends the pane to where the glide was heading, so
    /// an interrupted pan leaves the followed line centred; a manual scroll
    /// passes `false` and keeps the position the user chose.
    fn stop_glide(&mut self, land: bool) -> Task<Message> {
        let Some(anim) = self.scroll_anim.take() else {
            return Task::none();
        };
        self.scroll_target = None;
        if land {
            self.follow_offset = Some(anim.target);
            self.scroll_target = Some(anim.target);
            scrollable::scroll_to(
                transcript_id(),
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: anim.target,
                },
            )
        } else {
            self.follow_offset = None;
            Task::none()
        }
    }

    fn scroll_to_line(&mut self, line: usize) -> Task<Message> {
        // A named destination (a seek, a search hit, a keyboard line move) must
        // land at once: a glide towards the previous target would keep writing
        // offsets over it.
        self.scroll_anim = None;
        match self.follow_target(line) {
            Some(y) => {
                let active = self.follow_line_index();
                self.scroll_target = Some(y);
                self.note_follow_issue(y, active);
                scrollable::scroll_to(transcript_id(), scrollable::AbsoluteOffset { x: 0.0, y })
            }
            None => Task::none(),
        }
    }

    /// Bring the view straight back to the playhead after a viewport report
    /// while Follow is on. Returns the offset to scroll to in this same update
    /// so the pane never sits away from the playhead waiting for the next tick.
    ///
    /// `offset` is where the scrollable says it is; `follow_offset` is where
    /// Follow asked it to go.
    fn correct_follow_offset(
        &mut self,
        previous_content: f32,
        offset: f32,
        viewport_h: f32,
        content_h: f32,
    ) -> Option<f32> {
        if !self.follow_playhead {
            return None;
        }
        let expected = self.follow_offset?;
        // Near enough: the scrollable clamps and rounds, so a few pixels off is
        // a landing, not a user scroll.
        if (offset - expected).abs() <= FOLLOW_STUCK_STEP {
            return None;
        }
        // Re-wrapping changes the content height and clamps the offset without
        // any user scroll (scrollbar appears, zoom, window resize, streamed
        // lines). That must not disengage Follow.
        let relayout = (content_h - previous_content).abs() > 0.5;
        // A remounted scrollable starts at 0 even though the content still
        // overflows. Changing the widget tree above it (a toast appearing or
        // expiring, a dialog opening or closing) does exactly that.
        let remount = offset <= FOLLOW_DEADBAND && content_h > viewport_h + FOLLOW_DEADBAND;
        // Correcting only makes sense while the playhead target still exists;
        // a genuine shrink clamps the offset and is adopted instead.
        let reachable = expected <= (content_h - viewport_h).max(0.0) + FOLLOW_DEADBAND;
        if remount || (relayout && reachable) {
            self.scroll_target = Some(expected);
            // Re-issue the playhead target and record the situation we are
            // back in, so the tick does not pile another scroll on top of it.
            self.follow_issue = Some(FollowIssue {
                at: Instant::now(),
                target: expected,
                offset,
                active: self.follow_line_index(),
                playhead: self.playhead,
                content: content_h,
                viewport: viewport_h,
                retried: false,
            });
            return Some(expected);
        }
        if relayout {
            // The content really did get shorter: keep following from the
            // clamped position.
            self.follow_offset = Some(offset);
            return None;
        }
        // A user scroll disengages Follow instead of snapping the transcript
        // back to the playhead. Scrolling never moves the playhead; only the
        // timeline, line selection and playback do.
        self.follow_playhead = false;
        self.follow_offset = None;
        self.scroll_target = None;
        self.scroll_anim = None;
        self.follow_issue = None;
        self.scroll_seek = false;
        self.save_settings();
        None
    }

    /// True when the transcript pane is showing the global empty state.
    fn feature_deck_visible(&self) -> bool {
        // Free-tier upgrade pitch. It is the default pane whenever no
        // transcription is on screen: empty queue, queued files, model loading,
        // decoding and diarization. It yields as soon as lines are visible.
        self.shows(license::Audience::FreeOnly)
            && self.shown_lines().is_empty()
            && self.shown_transcript().trim().is_empty()
    }

    /// Auto-advance the deck whenever it is on screen and no dialog owns the
    /// window.
    fn feature_deck_auto_advance(&self) -> bool {
        self.feature_deck_visible()
            && self.help_open.is_none()
            && !self.paywall_open
            && !self.settings_open
            && !self.summary_open
            && !self.rename_open
            && self.line_edit.is_none()
    }

    /// Five-slide feature deck for the first-run empty state.
    fn feature_deck(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let index = self.feature_slide.min(FEATURE_DECK.len() - 1);
        let slide = &FEATURE_DECK[index];
        let accent = match slide.eyebrow {
            "FREE" => scheme.green,
            "WHY HIPPO" => scheme.pink,
            _ => scheme.blue,
        };

        let eyebrow = row![
            feature_eyebrow(slide.eyebrow, accent, scheme),
            horizontal_space(),
            text(format!("{} of {}", index + 1, FEATURE_DECK.len()))
                .size(theme::TEXT_XS)
                .wrapping(NO_WRAP)
                .color(scheme.muted),
        ]
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

        let copy = column![
            text(slide.title)
                .size(theme::TEXT_XL)
                .font(Font {
                    weight: font::Weight::Bold,
                    ..Font::DEFAULT
                })
                .color(scheme.ink)
                .width(Length::Fill),
            text(slide.body)
                .size(theme::TEXT_MD)
                .color(scheme.muted)
                .width(Length::Fill),
        ]
        .spacing(theme::SPACE_SM);

        // The deck advances on a timer; no manual arrows or dots.
        let content = column![eyebrow, self.feature_vignette(index, accent, &scheme), copy,]
            .spacing(theme::SPACE_LG)
            .width(Length::Fill);

        mouse_area(
            container(
                container(content)
                    .center_x(Length::Fill)
                    .max_width(500.0)
                    .padding([18, 20]),
            )
            .center(Length::Fill)
            .padding([18, 24]),
        )
        .interaction(mouse::Interaction::Pointer)
        .on_press(Message::FeatureAdvance)
        .into()
    }

    /// One abstract UI vignette per slide, all drawn from the active scheme.
    fn feature_vignette(
        &self,
        index: usize,
        accent: Color,
        scheme: &Scheme,
    ) -> Element<'_, Message> {
        let scheme = *scheme;
        let body: Element<Message> = match index {
            0 => feature_vignette_file(scheme),
            1 => feature_vignette_speakers(scheme),
            2 => feature_vignette_queue(scheme),
            3 => feature_vignette_review(scheme),
            _ => feature_vignette_local(scheme),
        };

        container(body)
            .width(Length::Fill)
            .height(Length::Fixed(160.0))
            .padding(14)
            .align_y(iced::Alignment::Center)
            .style(move |_theme| container::Style {
                background: Some(linear_gradient(
                    theme::mix(scheme.bg, accent, 0.06),
                    scheme.bg,
                )),
                border: iced::border::Border {
                    color: scheme.border,
                    width: 1.0,
                    radius: theme::RADIUS_LG.into(),
                },
                shadow: soft_shadow(),
                ..container::Style::default()
            })
            .into()
    }

    /// Friendly placeholder shown when there is no text to render yet.
    fn empty_transcript_state(&self, scheme: &Scheme) -> Element<'_, Message> {
        if self.feature_deck_visible() {
            return self.tagged(license::Audience::FreeOnly, self.feature_deck(scheme));
        }
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
                format!("{detail}. Lines will appear here as they are transcribed."),
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
                "Drop audio here",
                "Drag audio files anywhere into the window, or add them from the left panel."
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
                "Press Transcribe (or Ctrl+Enter) to start the queue.".to_string(),
            )
        } else {
            (
                ICON_CIRCLE,
                scheme.track,
                "Nothing here yet",
                "Add audio files to the queue to get started.".to_string(),
            )
        };

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

        let content = column![
            icon,
            text(title).size(22).font(Font {
                weight: font::Weight::Bold,
                ..Font::DEFAULT
            }),
            text(body)
                .size(14)
                .color(scheme.muted)
                .width(Length::Fill)
                .align_x(iced::Alignment::Center),
        ]
        .spacing(10)
        .align_x(iced::Alignment::Center);

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

    /// The queue card in the left panel: header, add toolbar, list and
    /// run controls.
    fn acceleration_row(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let running = matches!(self.status, Status::Running);
        let selected = self.gpu_selected();
        let reason = self.gpu_unavailable_reason();
        let multiplier = self.gpu_multiplier();
        let pro = self.can(license::Feature::GpuAcceleration);

        // CPU-only builds do not need a toggle or a test action.
        if !asr::gpu_available() {
            return row![
                text("Acceleration")
                    .size(11)
                    .wrapping(NO_WRAP)
                    .color(scheme.muted),
                horizontal_space(),
                self.tip(
                    text("CPU only")
                        .size(11)
                        .wrapping(NO_WRAP)
                        .color(scheme.muted),
                    "This build has no GPU support.",
                    &scheme,
                ),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center)
            .into();
        }

        let failed = self
            .gpu_profile()
            .is_some_and(|profile| profile.gpu_error.is_some());
        let measured = multiplier.is_some();
        let untested = !measured && !failed;
        let gpu_label = if measured && reason.is_none() {
            format!("GPU {:.1}\u{00d7}", multiplier.unwrap_or(1.0))
        } else if untested {
            "Test GPU".to_string()
        } else {
            "GPU".to_string()
        };
        let can_test = pro && self.models_ready();

        let segment = |label: String, selected: bool, muted: bool| {
            button(
                container(text(label).size(12).wrapping(NO_WRAP))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .padding(0)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(segment_button(scheme, selected, muted))
        };

        let cpu_selected = !selected;
        let cpu_button = segment("CPU".to_string(), cpu_selected, false)
            .on_press_maybe((!running && selected).then_some(Message::SetAcceleration(false)));

        let gpu_locked = !pro;
        let gpu_muted = gpu_locked || reason.is_some();
        let gpu_button = {
            let button = segment(gpu_label, selected, gpu_muted);
            if gpu_locked {
                button.on_press_maybe((!running).then_some(Message::LockedGpu))
            } else if reason.is_none() {
                if untested {
                    button.on_press_maybe((!running && can_test).then_some(Message::RetestGpu))
                } else {
                    button.on_press_maybe(
                        (!running && !selected).then_some(Message::SetAcceleration(true)),
                    )
                }
            } else {
                // A stored failure must not be a dead end: the only other way
                // back was editing the settings file. Clicking retests.
                button.on_press_maybe((!running && can_test).then_some(Message::RetestGpu))
            }
        };
        let gpu_button: Element<Message> = if let Some(reason) = &reason {
            let hint = if can_test {
                format!("{reason} Click to test this preset again.")
            } else {
                reason.clone()
            };
            self.tip(gpu_button, hint.as_str(), &scheme)
        } else if untested {
            self.tip(
                gpu_button,
                if can_test {
                    "Test CPU and GPU speed for this preset."
                } else {
                    "Download the transcription models before testing GPU."
                },
                &scheme,
            )
        } else {
            gpu_button.into()
        };

        let action_label = if failed {
            "Test again"
        } else if measured {
            "Retest GPU"
        } else {
            "Test GPU speed"
        };
        let action = button(text(action_label).size(11).wrapping(NO_WRAP))
            .padding([2, 6])
            .on_press_maybe(
                (can_test && !running && self.calibration.is_none()).then_some(Message::RetestGpu),
            )
            .style(text_action_button(scheme));
        let action_tip = if let Some(error) = self
            .gpu_profile()
            .and_then(|profile| profile.gpu_error.as_deref())
        {
            format!("{error} Test GPU speed again.")
        } else if !self.models_ready() {
            "Download the transcription models before testing GPU.".to_string()
        } else if multiplier.is_some() {
            "Retest GPU speed. Replaces the saved result.".to_string()
        } else {
            "Test CPU and GPU speed on a short sample.".to_string()
        };
        let action = self.tip(action, action_tip, &scheme);

        let mut header = row![
            text("Acceleration")
                .size(11)
                .wrapping(NO_WRAP)
                .color(scheme.muted),
            horizontal_space(),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);
        if pro && (measured || failed) {
            header = header.push(action);
        }

        let track = container(
            row![cpu_button, gpu_button]
                .spacing(2)
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .padding(2)
        .width(Length::Fill)
        .height(Length::Fixed(28.0))
        .style(move |_theme| container::Style {
            // Slightly darker than the panel it sits on, so the track reads as
            // an inset well and the selected thumb can sit proud of it.
            background: Some(theme::darken(scheme.panel_bg, 0.14).into()),
            border: iced::border::Border {
                color: theme::mix(scheme.border, scheme.text, 0.12),
                width: 1.0,
                radius: PILL.into(),
            },
            ..container::Style::default()
        });

        column![header, track].spacing(4).into()
    }

    /// Floating copy of the dragged queue row, positioned under the pointer.
    fn queue_drag_ghost(&self, scheme: &Scheme) -> Option<Element<'_, Message>> {
        let index = self.queue_drag?;
        let point = self.queue_pointer?;
        let item = self.queue.get(index)?;
        let name = item
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.path.display().to_string());
        let name = ellipsize_middle(&name, 32);
        let scheme = *scheme;
        let ghost = container(
            row![
                icon(ICON_FILE_TEXT, 12.0),
                text(name).size(12).wrapping(NO_WRAP),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        )
        .padding([6, 10])
        .style(move |_theme| container::Style {
            background: Some(scheme.dialog_bg.into()),
            text_color: Some(scheme.text),
            border: iced::border::Border {
                color: scheme.blue,
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: dialog_shadow(),
            ..container::Style::default()
        });
        Some(
            container(ghost)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::Start)
                .align_y(iced::Alignment::Start)
                .padding(iced::Padding {
                    top: (point.y - 18.0).max(0.0),
                    left: (point.x + 12.0).max(0.0),
                    right: 0.0,
                    bottom: 0.0,
                })
                .into(),
        )
    }

    /// The Transcribe/Retranscribe dialog: the run's options, shown when the
    /// button is pressed instead of occupying the queue panel.
    fn run_setup_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;

        let output = self
            .output_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Audio source directory".to_string());
        let output_short = ellipsize_middle(&output, 38);
        let output_row = row![
            action_text("Save Location")
                .on_press(Message::PickOutput)
                .style(pill_button_secondary),
            text(output_short)
                .size(theme::TEXT_XS)
                .wrapping(NO_WRAP)
                .color(scheme.muted)
                .width(Length::Fill),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let preset: Element<Message> = if !self.shows(license::Audience::ProOnly) {
            // Free tier: Balanced only, and the picker cannot be opened.
            self.tip(
                stack![
                    pick_list(Preset::ALL, Some(Preset::Balanced), Message::PresetChanged,)
                        .style(greyed_pick(scheme))
                        .menu_style(pick_menu_style(scheme))
                        .handle(caret_handle())
                        .width(Length::Fill),
                    mouse_area(
                        container(horizontal_space())
                            .width(Length::Fill)
                            .height(Length::Fill),
                    )
                    .interaction(mouse::Interaction::Pointer)
                    .on_press(Message::LockedPreset),
                ],
                "Speed and Accuracy presets are Pro features",
                &scheme,
            )
        } else {
            pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged)
                .style(pill_pick(scheme))
                .menu_style(pick_menu_style(scheme))
                .handle(caret_handle())
                .width(Length::Fill)
                .into()
        };

        let speaker_pick = pick_list(
            SpeakerOpt::ALL,
            Some(SpeakerOpt::from_value(self.num_speakers)),
            Message::SpeakerCountChanged,
        )
        .menu_style(pick_menu_style(scheme))
        .handle(caret_handle());
        let speakers: Element<Message> = if !self.shows(license::Audience::ProOnly) {
            // Free runs ASR only, so the speaker count has no effect.
            self.tip(
                stack![
                    speaker_pick.style(greyed_pick(scheme)).width(Length::Fill),
                    mouse_area(
                        container(horizontal_space())
                            .width(Length::Fill)
                            .height(Length::Fill),
                    )
                    .interaction(mouse::Interaction::Pointer)
                    .on_press(Message::LockedSpeakers),
                ],
                "Speaker labels are a Pro feature",
                &scheme,
            )
        } else {
            speaker_pick
                .style(pill_pick(scheme))
                .width(Length::Fill)
                .into()
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

        let primary = action_text(self.transcribe_label())
            .on_press(Message::ConfirmRunSetup)
            .style(color_button_outlined(
                scheme.green,
                if theme::is_dark_scheme(&scheme) {
                    Color::WHITE
                } else {
                    scheme.on_accent
                },
                scheme,
            ));
        let body = column![
            text("Transcription Settings").size(20),
            output_row,
            options,
            self.acceleration_row(&scheme),
            row![
                horizontal_space(),
                // The options save as they are changed, so this closes rather
                // than reverting them.
                action_text("Close")
                    .on_press(Message::CloseRunSetup)
                    .style(pill_button_secondary),
                primary,
            ]
            .spacing(8),
        ]
        .spacing(10);
        modal_card(body, 460.0, scheme)
    }

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

        // All three take a 16 px line box, the app's normal control size, so the
        // toolbar is the same height as every other button row: 20.8 + 10 of
        // padding = `CONTROL_HEIGHT`.
        let add_button = self.tip(
            action(
                row![
                    icon_colored(ICON_PLUS, 16.0, scheme.on_accent),
                    text("Add").wrapping(NO_WRAP),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center),
            )
            .on_press(Message::AddFiles)
            .style(color_button(scheme.blue, scheme.on_accent)),
            "Add audio files",
            &scheme,
        );

        let capturing = self.recording.is_some();
        let record_button = self.tip(
            action(
                row![
                    icon_colored(
                        ICON_MIC,
                        14.0,
                        if capturing { scheme.on_accent } else { scheme.text },
                    ),
                    text(if capturing { "Stop" } else { "Record" }).wrapping(NO_WRAP),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center),
            )
            .on_press(if capturing {
                Message::StopRecording
            } else {
                Message::StartRecording
            })
            .style(move |theme, status| {
                if capturing {
                    color_button(scheme.danger, scheme.on_accent)(theme, status)
                } else {
                    pill_button_secondary(theme, status)
                }
            }),
            if capturing {
                "Stop recording"
            } else {
                "Record from the microphone"
            },
            &scheme,
        );

        let folder_button: Element<Message> = if self.can(license::Feature::Batch) {
            self.tip(
                action(icon(ICON_FOLDER_PLUS, 16.0))
                    .on_press(Message::AddFolder)
                    .style(pill_button_secondary),
                "Add a folder",
                &scheme,
            )
        } else {
            self.tip(
                action(icon_colored(ICON_FOLDER_PLUS, 16.0, scheme.muted))
                    .on_press(Message::AddFolder)
                    .style(greyed_button(scheme)),
                "Folder import is a Pro feature",
                &scheme,
            )
        };

        let clear_button = self.tip(
            action(icon(ICON_TRASH, 16.0))
                .on_press_maybe((!running && has_queue).then_some(Message::ClearQueue))
                .style(pill_button_secondary),
            "Clear queue",
            &scheme,
        );

        let toolbar = row![
            add_button,
            record_button,
            folder_button,
            horizontal_space(),
            clear_button,
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center);

        let strip: Option<Element<'_, Message>> = self.recording.as_ref().map(|recorder| {
            let reading = recorder.meter();
            // Normalizing against the room hides overload, so the strip says
            // so in words as well as changing the fill.
            let clipping = reading.clipping;
            let meter = canvas(RecordingMeter {
                level: reading.level,
                peak: reading.peak,
                // In the dark palette `bar_track` is the same grey as the well
                // behind it, which would make the unlit segments disappear;
                // nudging it toward the text colour keeps the whole scale
                // visible without introducing a new token.
                track: theme::mix(scheme.bar_track, scheme.text, 0.08),
                fill: if clipping {
                    theme::lighten(scheme.danger, 0.35)
                } else {
                    scheme.danger
                },
                hold: Color {
                    a: 0.65,
                    ..scheme.text
                },
            })
            .width(Length::Fill)
            .height(Length::Fixed(8.0));
            container(
                row![
                    icon_colored(ICON_MIC, 14.0, scheme.danger),
                    text(if clipping { "Clipping" } else { "Recording" })
                        .size(theme::TEXT_SM)
                        .color(if clipping { scheme.danger } else { scheme.text }),
                    text(format_secs(recorder.elapsed() as f64))
                        .size(theme::TEXT_XS)
                        .color(scheme.muted),
                    meter,
                ]
                .spacing(theme::SPACE_SM)
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .padding([4, 8])
            .style(move |_theme| container::Style {
                background: Some(scheme.field_bg.into()),
                border: iced::border::Border {
                    color: scheme.border,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..container::Style::default()
            })
            .into()
        });

        let body: Element<Message> = if has_queue {
            let mut rows = column![].spacing(4).width(Length::Fill);
            for index in 0..self.queue.len() {
                if self.queue_drag.is_some()
                    && self.queue_drag != Some(index)
                    && self.queue_hover == Some(index)
                {
                    rows = rows.push(
                        container(horizontal_space())
                            .width(Length::Fill)
                            .height(Length::Fixed(2.0))
                            .style(move |_theme| container::Style {
                                background: Some(scheme.blue.into()),
                                border: iced::border::rounded(1.0),
                                ..container::Style::default()
                            }),
                    );
                }
                rows = rows.push(self.queue_row(index, &scheme));
            }
            let list = mouse_area(
                scrollable(rows)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(visible_scrollbar_style),
            )
            .on_move(Message::QueuePointerMoved)
            .on_exit(Message::QueuePointerLeft);
            match self.queue_drag_ghost(&scheme) {
                Some(ghost) => stack![list, ghost].into(),
                None => list.into(),
            }
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

        let controls = row![
            button(
                container(text(self.transcribe_label()))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fixed(30.0))
            // The options live in the dialog this opens; starting is the
            // dialog's job, so nothing is launched by the first press.
            .on_press_maybe(can_start.then_some(Message::OpenRunSetup))
            // Dark mode fills read better with a white label; light mode
            // keeps the dark ink it has always used.
            .style(color_button_outlined(
                scheme.green,
                if theme::is_dark_scheme(&scheme) {
                    Color::WHITE
                } else {
                    scheme.on_accent
                },
                scheme,
            )),
            button(
                container(text("Cancel"))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fixed(30.0))
            .on_press_maybe(running.then_some(Message::Cancel))
            .style(color_button_outlined(
                scheme.danger,
                if theme::is_dark_scheme(&scheme) {
                    Color::WHITE
                } else {
                    scheme.on_accent
                },
                scheme,
            )),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let mut content = column![header, toolbar];
        if let Some(strip) = strip {
            content = content.push(strip);
        }
        let content = content
            .push(body)
            .push(horizontal_rule(1))
            .push(controls)
            .spacing(12)
            .height(Length::Fill);

        container(content)
            .padding(12)
            .width(Length::Fixed(width))
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(scheme.panel_bg.into()),
                border: island_border(&scheme),
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
        // Free locks the view to the file being transcribed: older transcripts
        // cannot be opened while a run is in progress.
        let locked_previous = self.shows(license::Audience::FreeOnly)
            && running
            && matches!(item.status, ItemStatus::Done)
            && self.current_index != Some(index);
        let selectable = !locked_previous
            && (matches!(item.status, ItemStatus::Running)
                || (!running
                    && matches!(item.status, ItemStatus::Pending | ItemStatus::Interrupted))
                || (matches!(item.status, ItemStatus::Done) && item.transcript.is_some()));

        let full_name = item
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.path.display().to_string());
        let name_limit = if matches!(item.status, ItemStatus::Running) {
            22
        } else {
            30
        };
        let name = ellipsize_middle(&full_name, name_limit);
        let duration = item
            .duration
            .map(format_secs)
            .unwrap_or_else(|| "--:--".to_string());

        let (meta, meta_color) = match &item.status {
            ItemStatus::Pending => (format!("{duration} \u{00b7} Queued"), scheme.muted),
            ItemStatus::Interrupted => {
                (format!("{duration} \u{00b7} Interrupted"), scheme.muted)
            }
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
            // Queued, nothing has happened yet: an unfilled ring at the lowest
            // emphasis in the ladder (queued < running < done).
            ItemStatus::Pending => queue_status_icon(ICON_CIRCLE, 15.0, scheme.muted),
            ItemStatus::Interrupted => {
                queue_status_badge(ICON_CLOCK, 14.0, scheme.track, scheme.text)
            }
            ItemStatus::Running => {
                queue_status_badge(ICON_HOURGLASS, 14.0, scheme.track, scheme.text)
            }
            // A check while the run is still going; once the whole queue has
            // finished, the row marks itself as a finished transcript instead.
            ItemStatus::Done if running => {
                queue_status_badge(ICON_CHECK, 18.0, scheme.green, scheme.on_accent)
            }
            ItemStatus::Done => {
                // Styled like the transcript pane's "choose transcript" chip:
                // a light disc with the page drawn in the accent blue, rather
                // than a solid fill.
                queue_status_badge(ICON_FILE_TEXT, 15.0, scheme.track, scheme.blue)
            }
            ItemStatus::Failed(_) => {
                queue_status_badge(ICON_X, 15.0, scheme.danger, scheme.on_accent)
            }
        };

        let name_color = if locked_previous {
            scheme.muted
        } else {
            scheme.text
        };
        let label = column![
            text(name)
                .size(theme::TEXT_SM)
                .wrapping(NO_WRAP)
                .color(name_color),
            text(meta).size(11).wrapping(NO_WRAP).color(meta_color),
        ]
        .spacing(1)
        .width(Length::Fill);

        // Left-edge marker: green while this row is being transcribed, blue
        // while its transcript is on screen.
        let transcribing = self.current_index == Some(index);
        let viewing_item = self.selected == Some(index) && self.selected_view.is_some();
        let marker_color = if transcribing {
            scheme.green
        } else if viewing_item {
            scheme.blue
        } else {
            Color::TRANSPARENT
        };
        let marker = container(horizontal_space())
            .width(Length::Fixed(3.0))
            .height(Length::Fixed(22.0))
            .style(move |_theme| container::Style {
                background: Some(marker_color.into()),
                border: iced::border::rounded(2.0),
                ..container::Style::default()
            });

        let mut content = row![marker, badge, label]
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

        if matches!(item.status, ItemStatus::Interrupted) {
            content = content.push(
                self.tip(
                    action(icon(ICON_ARROW_DOWN, 12.0))
                        .on_press_maybe((!running).then_some(Message::SkipItem(index)))
                        .style(subtle_icon_button),
                    "Skip this file",
                    &scheme,
                ),
            );
        }
        if matches!(item.status, ItemStatus::Failed(_) | ItemStatus::Interrupted) {
            content = content.push(
                self.tip(
                    action(icon(ICON_RETRY, 12.0))
                        .on_press_maybe((!running).then_some(Message::RetryItem(index)))
                        .style(subtle_icon_button),
                    if matches!(item.status, ItemStatus::Interrupted) {
                        match self
                            .last_run
                            .as_ref()
                            .and_then(|run| Preset::from_arg(&run.preset))
                        {
                            Some(preset) => format!("Resume the {} run", preset.label()),
                            None => "Resume".to_string(),
                        }
                    } else {
                        "Retry".to_string()
                    },
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
        let row_text = if locked_previous {
            scheme.muted
        } else {
            scheme.text
        };
        let dragging = self.queue_drag == Some(index);
        let hovered = self.queue_hover == Some(index);
        let background = if dragging {
            Color {
                a: 0.14,
                ..scheme.blue
            }
        } else if viewing {
            row_tint
        } else if selectable && hovered {
            hover_tint
        } else {
            Color::TRANSPARENT
        };
        let border_color = if dragging || viewing {
            scheme.blue
        } else {
            Color::TRANSPARENT
        };
        let row = container(content)
            .padding([6, 8])
            .width(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(background.into()),
                text_color: Some(row_text),
                border: iced::border::Border {
                    color: border_color,
                    width: 1.0,
                    radius: 10.0.into(),
                },
                shadow: if dragging {
                    soft_shadow()
                } else {
                    Shadow::default()
                },
                ..container::Style::default()
            });
        let row = mouse_area(row)
            .interaction(mouse::Interaction::Pointer)
            .on_press(Message::QueueDragStart(index))
            .on_release(Message::QueueDrop(index))
            .on_enter(Message::QueueHover(Some(index)))
            .on_exit(Message::QueueHover(None));
        row.into()
    }

    /// Transcript view: per-line (search tinted) or SMS bubbles.
    fn transcript_element(&self) -> Element<'_, Message> {
        let scheme = self.scheme();
        if self.feature_deck_visible() {
            return self.tagged(license::Audience::FreeOnly, self.feature_deck(&scheme));
        }
        if self.queue.is_empty() {
            return self.empty_transcript_state(&scheme);
        }
        if let Some(error) = &self.view_error {
            let col = column![
                text("Transcript unavailable").size(20),
                text(error.clone()).size(14),
            ]
            .spacing(8)
            .padding(16);
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
        let mut display: Vec<usize> = self.display_order();
        let playhead_line = self.active_line_index();
        let lines = self.shown_lines();
        // The edited row must stay in the rendered window: scrolling it out
        // would unmount the field. It is also put back into the order when the
        // speaker filter would hide it (the panel can change the speaker).
        let editing = self.line_edit.as_ref().map(|edit| edit.index);
        if let Some(index) = editing {
            pin_display_index(&mut display, lines, index);
        }

        // Virtualize the long list. The buffer keeps a few screens of text
        // ready while the spacer heights preserve the scrollbar range. A large
        // scroll jump temporarily renders a wider window so a fast drag does
        // not expose empty space.
        let total = display.len();
        let line_height = self.pane_line_height((size * 1.8).max(24.0));
        let viewport = if self.transcript_viewport > 0.0 {
            self.transcript_viewport
        } else {
            600.0
        };
        const BUFFER_LINES: usize = 16;
        let visible = (viewport / line_height).ceil() as usize + BUFFER_LINES * 2 + 2;
        // Re-anchor the window only when the view leaves it. Moving the anchor
        // shifts every rendered row by (estimate - real height), so re-anchoring
        // on every scroll event is what made the pane creep under the pointer.
        let mut start = ((self.transcript_offset / line_height).floor() as usize)
            .saturating_sub(BUFFER_LINES)
            .min(total);
        if let Some(anchor) = self.pane_start.get().filter(|anchor| *anchor <= total) {
            let band_top = anchor as f32 * line_height;
            let band_bottom =
                ((anchor + visible).min(total) as f32 * line_height - viewport).max(band_top);
            if self.transcript_offset >= band_top && self.transcript_offset <= band_bottom {
                start = anchor;
            }
        }
        let end = (start + visible).min(total);
        self.pane_start.set(Some(start));
        // The edited row is rendered even when it sits outside the window: it
        // goes in its own segment with spacers either side, so scrolling it out
        // of view cannot unmount the field. The window itself is never widened,
        // which would render thousands of rows for a distant edit.
        let editor_rank = editing.and_then(|index| display.iter().position(|i| *i == index));
        let above = editor_rank.filter(|rank| *rank < start);
        let below = editor_rank.filter(|rank| *rank >= end);
        let top_height = above.map_or(start, |rank| rank) as f32 * line_height;
        let bottom_height =
            below.map_or(total - end, |rank| total - rank - 1) as f32 * line_height;
        // Remember the geometry we asked for so the next frame can measure the
        // real height of the rendered window.
        self.pane_window
            .set((start, end, top_height, bottom_height));

        if let Some(rank) = above {
            col = col.push(row_spacer(top_height));
            col = col.push(self.inline_editor_element(size, scheme));
            col = col.push(row_spacer((start - rank - 1) as f32 * line_height));
        } else if top_height > 0.0 {
            col = col.push(row_spacer(top_height));
        }

        // Each speaker keeps a fixed side of the transcript in bubble mode.
        for &idx in &display[start..end] {
            let line = &lines[idx];
            let active = playhead_line == Some(idx);
            // The edited row is a field where it stands, in either view.
            if editing == Some(idx) {
                col = col.push(self.inline_editor_element(size, scheme));
                continue;
            }
            let hovered = self.transcript_line_hover == Some(idx) && !self.pointer_blocked();
            let index = speaker_order
                .iter()
                .position(|name| *name == line.speaker)
                .unwrap_or(if single_speaker { 1 } else { 0 });
            let accent = speaker_palette(index, &scheme);
            // A line with no text still gets a row: skipping it would leave a
            // gap nobody can click and desync the spacer heights from the rows
            // actually rendered.
            if line.raw.trim().is_empty() {
                col = col.push(empty_line_element(idx, accent, &scheme, size, hovered));
                continue;
            }
            let matched = !query.is_empty() && matches.contains(&idx);
            let tint = matched.then_some(if Some(idx) == active_line {
                scheme.search_active
            } else {
                scheme.search_hl
            });
            if self.sms_mode {
                let left = index.is_multiple_of(2);
                let stamp = if self.show_timestamps {
                    line.start
                        .map(|seconds| format_timestamp(seconds as f64))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let bubble: Element<'static, Message> = sms_bubble(
                    &line_text(line),
                    &line.speaker,
                    &stamp,
                    accent,
                    scheme.ink,
                    tint,
                    self.confidence_view && is_low_confidence(line),
                    &scheme,
                    &query,
                    size,
                    active,
                    idx,
                    self.karaoke_for(idx, active),
                )
                .into();
                // The exchange button belongs to the bubble, not to the pane
                // edge: it sits against the bubble's inner side and appears
                // only while the row is hovered, so the playhead row does not
                // carry a button the pointer is not on.
                let swap =
                    (!single_speaker && hovered).then(|| sms_swap_button(idx, &scheme));
                let mut content_row = row![]
                    .spacing(theme::SPACE_SM)
                    .align_y(iced::Alignment::Center)
                    .width(Length::Fill);
                if left || single_speaker {
                    content_row = content_row.push(bubble);
                    if let Some(swap) = swap {
                        content_row = content_row.push(swap);
                    }
                } else {
                    content_row = content_row.push(horizontal_space());
                    if let Some(swap) = swap {
                        content_row = content_row.push(swap);
                    }
                    content_row = content_row.push(bubble);
                }
                // The row itself takes the clicks: a child that handles its own
                // press (the exchange button) keeps it, and everything else
                // falls through to this handler, including the empty half.
                col = col.push(
                    mouse_area(content_row)
                        .interaction(mouse::Interaction::Pointer)
                        // The row takes the same press a line row does, so the
                        // first click seeks and the second opens the editor.
                        .on_press(Message::LineClicked(idx))
                        .on_right_press(Message::FlipLineSpeaker(idx))
                        .on_enter(Message::LineHover(Some(idx)))
                        .on_exit(Message::LineUnhover(idx)),
                );
            } else {
                col = col.push(line_element(
                    line,
                    idx,
                    accent,
                    tint,
                    &query,
                    false,
                    false,
                    scheme.search_hl,
                    &scheme,
                    size,
                    active,
                    self.show_timestamps,
                    hovered,
                    self.confidence_view,
                    self.karaoke_for(idx, active),
                ));
            }
        }

        if let Some(rank) = below {
            col = col.push(row_spacer((rank - end) as f32 * line_height));
            col = col.push(self.inline_editor_element(size, scheme));
            col = col.push(row_spacer(bottom_height));
        } else if bottom_height > 0.0 {
            col = col.push(row_spacer(bottom_height));
        }

        scrollable(col)
            .id(transcript_id())
            .on_scroll(Message::TranscriptScrolled)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(scrollbar_style)
            .into()
    }

    /// The inline editor for `index`, drawn where its transcript row was.
    ///
    /// The row keeps everything except the words themselves: the timestamp and
    /// the speaker label stay exactly where the transcript put them, and only
    /// the text becomes editable. Nothing moves when editing begins.
    fn inline_editor_element(&self, size: f32, scheme: Scheme) -> Element<'_, Message> {
        let Some(row) = self.line_edit.as_ref().and_then(|edit| edit.current_row()) else {
            return horizontal_space().into();
        };
        let accent = line_accent(&self.speaker_order(), &row.speaker, &scheme);
        let stamp = if self.show_timestamps {
            self.edit_lines()
                .get(row.index)
                .and_then(|line| line.start)
                .map(|seconds| format_timestamp(seconds as f64))
                .unwrap_or_default()
        } else {
            String::new()
        };
        if self.sms_mode {
            // A bubble keeps its meta row above the text and its own side of
            // the pane; only the text well is replaced.
            let order = self.speaker_order();
            let single_speaker = order.len() <= 1;
            let index = order
                .iter()
                .position(|name| name == &row.speaker)
                .unwrap_or(if single_speaker { 1 } else { 0 });
            let bubble = inline_bubble_edit(
                &row.content,
                &stamp,
                &row.speaker,
                accent,
                scheme,
                size,
                self.inline_bubble_width(size),
            );
            // The row being edited keeps its exchange button: the field is
            // where the pointer already is, and changing which speaker owns the
            // line while typing it is exactly when the button is wanted. It
            // sits on the bubble's inner side, as it does un-edited, and the
            // reserved slot keeps the field from moving when it appears.
            let swap = (!single_speaker).then(|| sms_swap_button(row.index, &scheme));
            let mut content = row![]
                .spacing(theme::SPACE_SM)
                .align_y(iced::Alignment::Center)
                .width(Length::Fill);
            if index.is_multiple_of(2) || single_speaker {
                content = content.push(bubble);
                if let Some(swap) = swap {
                    content = content.push(swap);
                }
            } else {
                content = content.push(horizontal_space());
                if let Some(swap) = swap {
                    content = content.push(swap);
                }
                content = content.push(bubble);
            }
            return content.into();
        }
        inline_line_edit_row(
            &row.content,
            &stamp,
            &row.speaker,
            accent,
            scheme,
            size,
        )
    }

    /// Width the right-hand pane has to work with: the window less the queue
    /// panel, the gap and the outer padding.
    fn pane_width(&self) -> f32 {
        (self.window_size.width - LEFT_PANEL_WIDTH - PANEL_GAP - 2.0 * OUTER_PADDING).max(320.0)
    }

    /// Width the bubble editor's text well has to wrap to.
    ///
    /// The pane, less the transcript's own padding and scrollbar, less the
    /// bubble's padding and border, less the exchange button's reserved slot --
    /// which the row keeps whether or not the button is showing, so the field
    /// never rewraps when it appears -- capped at the width a bubble may reach.
    fn inline_bubble_width(&self, size: f32) -> f32 {
        let pane = (self.pane_width() - 2.0 * 8.0 - 12.0).max(240.0) - SWAP_RESERVED_WIDTH;
        (pane - 30.0).min((size * 28.0).clamp(240.0, 620.0) - 20.0)
    }

    /// Controls for the line being edited: its speaker, the structural edits
    /// (join, split, delete), undo/redo, and the commit pair. Inserted between the
    /// transcript header and the list while the editor is open.
    fn line_edit_panel(&self, scheme: Scheme) -> Option<Element<'_, Message>> {
        let edit = self.line_edit.as_ref()?;
        let row = edit.current_row()?;
        let index = edit.index;
        let total = self.edit_lines().len();
        // A view can be swapped out from under the editor; the panel is where
        // that shows, so the row is disabled as a whole rather than half-live.
        let writable = self.shown_output().is_some();
        let allowed = writable && self.can(license::Feature::Editing);
        let can_join_prev = allowed && index > 0;
        let can_join_next = allowed && index + 1 < total;
        let can_split = allowed && split_at_caret(row).is_some();
        let undo = allowed && self.line_undo_available();
        let redo = allowed && self.line_redo_available();

        // Circles at `PANEL_ICON_DIAMETER`. The deletion is tinted like the
        // danger it is: with the row compact it sits mid-cluster, so its colour
        // is what keeps it from reading as one more neutral edit.
        let button = |code: char, enabled: bool, danger: bool, message: Message| {
            let ink = if !enabled {
                scheme.muted
            } else if danger {
                scheme.danger
            } else {
                scheme.ink
            };
            icon_circle(code, ink)
                .style(panel_icon_button(scheme, enabled))
                .on_press_maybe(enabled.then_some(message))
        };
        // Cancel stays live whatever the state: it is the way out of a row the
        // panel can no longer write to.
        let cancel = icon_circle(ICON_X, scheme.ink)
            .on_press(Message::CancelLineEdit)
            .style(panel_icon_button(scheme, true));
        // Same box as Reveal: the label sets the height, and the width is the
        // width that label reserves, so the commit pair matches the row of
        // transcript actions beneath the pane.
        let done = action(
            row![
                icon(ICON_CHECK, 14.0),
                text("Done").wrapping(NO_WRAP)
            ]
            .spacing(3)
            .align_y(iced::Alignment::Center),
        )
        .on_press_maybe(allowed.then_some(Message::DoneLineEdit))
        .width(Length::Fixed(action_width("Reveal")))
        .padding([5, 10])
        .style(color_button(scheme.blue, scheme.on_accent));

        // The strip is a full-width band inside the card, so it follows the same
        // grammar as the header above it and the Find row below. Three groups:
        // history at the left edge, what the line can become in the middle, the
        // commit pair at the right where a dialog's buttons would. The middle
        // group is centred by the two elastic gaps around it, which are equal, so
        // it stays put when the pane is wide.
        let history = row![
            self.tip(
                button(ICON_UNDO, undo, false, Message::UndoLineEdit),
                "Undo the last line edit (Ctrl+Z inside the field undoes typing)",
                &scheme,
            ),
            self.tip(
                button(ICON_REDO, redo, false, Message::RedoLineEdit),
                "Redo the last line edit",
                &scheme,
            ),
        ]
        .spacing(LINE_EDIT_GAP)
        .align_y(iced::Alignment::Center);
        // No "Join" label: the two arrows carry their own tooltips, and the
        // word made the strip read as a labelled group where the rest of it is
        // icons.
        // Words have no place in this row: the edited line above already shows
        // whose it is and when, so the strip is controls only.
        let edits = row![
            self.tip(
                button(ICON_SPEAKERS, allowed, false, Message::OpenRename),
                "Rename speakers across the transcript",
                &scheme,
            ),
            self.tip(
                button(ICON_PERSON_PLUS, allowed, false, Message::AddLineSpeaker),
                "Give this line a new speaker",
                &scheme,
            ),
            self.tip(
                button(ICON_JOIN_PREV, can_join_prev, false, Message::MergePrevLine),
                "Join with the line above (Alt+Up)",
                &scheme,
            ),
            self.tip(
                button(ICON_JOIN_NEXT, can_join_next, false, Message::MergeNextLine),
                "Join with the line below (Alt+Down)",
                &scheme,
            ),
            self.tip(
                button(ICON_SPLIT, can_split, false, Message::SplitLineAtCursor),
                "Split at the cursor (Ctrl+Shift+K)",
                &scheme,
            ),
            self.tip(
                button(ICON_TRASH, allowed, true, Message::DeleteLineEdit),
                "Delete this line",
                &scheme,
            ),
        ]
        .spacing(LINE_EDIT_GAP)
        .align_y(iced::Alignment::Center);
        let controls = row![
            history,
            horizontal_space(),
            edits,
            horizontal_space(),
            self.tip(cancel, "Cancel (Esc) and keep the line as it was", &scheme),
            self.tip(done, "Save this line (Enter)", &scheme),
        ]
        .spacing(LINE_EDIT_GAP)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

        // A band inside the card, not a card of its own: no fill, no radius, no
        // shadow. It is closed off from the rows by one hairline along its bottom
        // edge, and the header above it already ends in a rule, so the strip reads
        // as a second toolbar row rather than as something dropped on top of the
        // transcript. The rule is the shared `scheme.border` hairline, so both
        // palettes get the same edge without a second colour.
        Some(
            column![
                container(controls)
                    .width(Length::Fill)
                    .height(Length::Fixed(LINE_EDIT_PANEL_HEIGHT - 1.0))
                    .padding([theme::CONTROL_PADDING[0], theme::SPACE_MD as u16])
                    .align_y(iced::Alignment::Center),
                container(horizontal_space())
                    .width(Length::Fill)
                    .height(Length::Fixed(1.0))
                    .style(move |_theme| container::Style {
                        background: Some(scheme.border.into()),
                        ..container::Style::default()
                    }),
            ]
            .spacing(0)
            .width(Length::Fill)
            .into(),
        )
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

        // How wide the identity block may grow before the filename has to be
        // cut: everything the menus leave, less the badge.
        let (name_budget, meta_budget) = header_identity_budgets(self.window_size.width);

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
            (ICON_FILE_TEXT, scheme.blue, name.to_string(), meta.join(" \u{00b7} "))
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
                if self.only_one_speaker() {
                    "Only one speaker detected".to_string()
                } else {
                    String::new()
                },
            )
        };

        let badge = container(icon_colored(glyph, 13.0, tint))
            .width(Length::Fixed(HEADER_BADGE_WIDTH))
            .height(Length::Fixed(HEADER_BADGE_WIDTH))
            .center_x(Length::Fixed(HEADER_BADGE_WIDTH))
            .center_y(Length::Fixed(HEADER_BADGE_WIDTH))
            .style(move |_theme| container::Style {
                background: Some(Color { a: 0.16, ..tint }.into()),
                border: iced::border::rounded(13.0),
                ..container::Style::default()
            });
        let meta = if self.modified {
            let prefix = if meta.is_empty() {
                String::new()
            } else {
                format!("{meta} \u{00b7} ")
            };
            format!(
                "{prefix}{} \u{00b7} Save As to keep",
                if self.join_lines {
                    "Joined"
                } else {
                    "Modified"
                }
            )
        } else {
            meta
        };
        // Both lines are capped to the same pixel width, so neither can reach
        // past the card's padding: the filename gets the whole budget, the meta
        // gets the same width in its smaller size. `ellipsize_middle` keeps the
        // tail, which is what preserves "Save As to keep" when the meta is cut.
        let title = ellipsize_middle(&title, name_budget);
        let meta = ellipsize_middle(&meta, meta_budget);
        let mut label = column![text(title).size(13).wrapping(NO_WRAP)].spacing(1);
        if !meta.is_empty() {
            label = label.push(
                text(meta)
                    .size(11)
                    .wrapping(NO_WRAP)
                    .color(if self.modified {
                        scheme.text
                    } else {
                        scheme.muted
                    }),
            );
        }

        // The header starts with the card's own menus, left to right File, Edit
        // and View, immediately followed by the file's own identity. Each menu
        // hangs from its own button's left edge; the offset helpers add the
        // fixed button widths to its left. Help belongs to the app header, not
        // to this card, so it is not here.
        let menus: Element<Message> = row![
            self.header_menu_button("File", HeaderMenu::File),
            self.header_menu_button("Edit", HeaderMenu::Edit),
            self.header_menu_button("View", HeaderMenu::View),
        ]
        .spacing(HEADER_GAP)
        .align_y(iced::Alignment::Center)
        .into();

        // The identity sits one row gap after View, and the elastic space takes
        // whatever is left: the menus and the label block keep their own widths,
        // so only the space changes with the window.
        let header = container(
            row![
                menus,
                row![badge, label]
                    .spacing(HEADER_BADGE_GAP)
                    .align_y(iced::Alignment::Center),
                horizontal_space(),
            ]
            .spacing(HEADER_GAP)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding([HEADER_GAP as u16, HEADER_PAD_X as u16]);
        let divider = container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_theme| container::Style {
                background: Some(scheme.border.into()),
                ..container::Style::default()
            });
        Some(column![header, divider].spacing(HEADER_GAP).into())
    }

    /// One of the transcript header's menu buttons. `label` is the menu's name;
    /// the caret says whether it is open.
    fn header_menu_button(
        &self,
        label: &'static str,
        menu: HeaderMenu,
    ) -> Element<'_, Message> {
        let open = self.header_menu == Some(menu);
        let width = match menu {
            HeaderMenu::File => FILE_BUTTON_WIDTH,
            HeaderMenu::Edit => EDIT_BUTTON_WIDTH,
            HeaderMenu::View => VIEW_BUTTON_WIDTH,
            HeaderMenu::Help => HELP_BUTTON_WIDTH,
        };
        action(
            container(
                row![
                    text(label).wrapping(NO_WRAP),
                    icon(if open { ICON_CARET_UP } else { ICON_CARET_DOWN }, theme::TEXT_XS),
                ]
                .spacing(theme::SPACE_XS)
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .center_x(Length::Fill),
        )
        .width(Length::Fixed(width))
        .on_press(Message::ToggleHeaderMenu(menu))
        .padding([5, 0])
        .style(if open {
            pill_button_toggled
        } else {
            pill_button_secondary
        })
        .into()
    }

    /// The File menu: the transcript as a document, not its text.
    fn file_menu(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let has_text = !self.shown_transcript().is_empty();
        let has_target = self.shown_output().is_some()
            || self.shown_audio().is_some()
            || self.output_dir.is_some();
        let running = matches!(self.status, Status::Running);
        let summary = !running && self.has_results();
        menu_card(
            vec![MenuSection {
                rows: vec![
                    MenuRow {
                        label: "Save As",
                        key: Some("Ctrl+S"),
                        checked: false,
                        enabled: has_text,
                        action: has_text.then_some(Message::SaveTranscriptAs),
                    },
                    MenuRow {
                        label: "Reveal",
                        key: None,
                        checked: false,
                        enabled: has_target,
                        action: has_target.then_some(Message::RevealOutput),
                    },
                    MenuRow {
                        label: "Summary",
                        key: None,
                        checked: false,
                        enabled: summary,
                        action: summary.then_some(Message::OpenSummary),
                    },
                    MenuRow {
                        label: "Briefing",
                        key: None,
                        checked: false,
                        enabled: has_text,
                        action: has_text.then_some(if self.can(license::Feature::Briefing) {
                            Message::OpenBriefing
                        } else {
                            Message::LockedBriefing
                        }),
                    },
                ],
                footer: None,
            }],
            scheme,
        )
    }

    /// The Edit menu: transcript-wide edits. Reset is only offered when the
    /// pre-edit copy exists to go back to.
    fn edit_menu(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let running = matches!(self.status, Status::Running);
        let editing = self.can(license::Feature::Editing);
        let can_rename = editing
            && !running
            && self.shown_output().is_some()
            && !self.shown_lines().is_empty();
        // Without a licence the row still explains itself rather than going dead.
        let rename_message = if can_rename {
            Some(Message::OpenRename)
        } else if !editing {
            Some(Message::LockedRename)
        } else {
            None
        };
        let has_text = !self.shown_transcript().is_empty();
        let reset = self.reset_available();
        menu_card(
            vec![MenuSection {
                rows: vec![
                    MenuRow {
                        label: "Rename All",
                        key: None,
                        checked: false,
                        enabled: can_rename,
                        action: rename_message,
                    },
                    MenuRow {
                        label: "Vocabulary",
                        key: None,
                        checked: false,
                        enabled: true,
                        action: Some(Message::OpenVocabulary),
                    },
                    MenuRow {
                        label: "Copy",
                        key: None,
                        checked: false,
                        enabled: has_text,
                        action: has_text.then_some(Message::CopyTranscript),
                    },
                    MenuRow {
                        label: "Reset",
                        key: None,
                        checked: false,
                        enabled: reset,
                        action: reset.then_some(Message::RequestResetTranscript),
                    },
                ],
                footer: None,
            }],
            scheme,
        )
    }

    /// The open View menu: the display toggles, then text zoom.
    ///
    /// Every row is a plain message, never a pick-list selection, so the menu
    /// stays mounted while several are pressed. The zoom pair is the section's
    /// footer and works the same way.
    fn view_menu(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let confidence = self.has_confidence();
        let rows = vec![
            MenuRow {
                label: "Follow",
                key: Some(ViewShortcut::Follow.key()),
                checked: self.follow_playhead,
                enabled: true,
                action: Some(Message::ToggleFollowPlayhead),
            },
            MenuRow {
                label: "Timestamps",
                key: Some(ViewShortcut::Timestamps.key()),
                checked: self.show_timestamps,
                enabled: true,
                action: Some(Message::TimestampsToggled(!self.show_timestamps)),
            },
            MenuRow {
                label: "SMS Mode",
                key: Some(ViewShortcut::Sms.key()),
                checked: self.sms_mode,
                enabled: true,
                action: Some(Message::SmsToggled(!self.sms_mode)),
            },
            MenuRow {
                label: "Join",
                key: Some(ViewShortcut::Join.key()),
                checked: self.join_lines,
                enabled: true,
                action: Some(Message::ToggleJoinLines),
            },
            MenuRow {
                label: "Karaoke",
                key: Some(ViewShortcut::Karaoke.key()),
                checked: self.karaoke,
                enabled: true,
                action: Some(Message::KaraokeToggled(!self.karaoke)),
            },
            MenuRow {
                label: "Confidence",
                key: Some(ViewShortcut::Confidence.key()),
                checked: self.confidence_view,
                enabled: confidence,
                action: confidence.then_some(Message::ToggleConfidenceView),
            },
        ];

        let font_on = self.font_scale;
        let zoom = row![
            self.tip(
                action(icon(ICON_ZOOM_OUT, 14.0))
                    .on_press_maybe((font_on > 0.6).then_some(Message::ZoomOut))
                    .style(pill_button_secondary),
                "Zoom text out",
                &scheme,
            ),
            // Fixed slot: the percentage must not move the two buttons as the
            // number changes width.
            container(text(format!("{:.0}%", font_on * 100.0)).size(theme::TEXT_XS))
                .width(Length::Fixed(44.0))
                .center_x(Length::Fixed(44.0)),
            self.tip(
                action(icon(ICON_ZOOM_IN, 14.0))
                    .on_press_maybe((font_on < 2.5).then_some(Message::ZoomIn))
                    .style(pill_button_secondary),
                "Zoom text in",
                &scheme,
            ),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        let footer = container(zoom)
            .width(Length::Fill)
            .align_x(iced::Alignment::Center)
            .into();
        menu_card(
            vec![MenuSection {
                rows,
                footer: Some(footer),
            }],
            scheme,
        )
    }

    /// The Help menu: the reference dialogs and the panel toggles, then the
    /// appearance choices.
    ///
    /// Theme lives here rather than in Settings because a menu can mark the
    /// current value with a check; the pick list this replaces reset to its
    /// placeholder after every choice, so it could only ever act.
    fn help_menu(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let actions = vec![
            MenuRow {
                label: "About",
                key: None,
                checked: false,
                enabled: true,
                action: Some(Message::HelpChoice(HelpKind::About)),
            },
            MenuRow {
                label: "Licenses",
                key: None,
                checked: false,
                enabled: true,
                action: Some(Message::HelpChoice(HelpKind::Licenses)),
            },
            MenuRow {
                label: "Shortcuts",
                key: None,
                checked: false,
                enabled: true,
                action: Some(Message::HelpChoice(HelpKind::Shortcuts)),
            },
            MenuRow {
                label: "Editing Tips",
                key: None,
                checked: false,
                enabled: true,
                action: Some(Message::HelpChoice(HelpKind::EditingTips)),
            },
            MenuRow {
                label: "Vocabulary",
                key: None,
                checked: false,
                enabled: true,
                action: Some(Message::OpenVocabulary),
            },
            MenuRow {
                label: "Console",
                key: None,
                checked: self.console_open,
                enabled: true,
                action: Some(Message::HelpChoice(HelpKind::Console)),
            },
            MenuRow {
                label: "Settings",
                key: None,
                checked: false,
                enabled: true,
                action: Some(Message::HelpChoice(HelpKind::Settings)),
            },
        ];
        // One row per mode, in the order `AppTheme::ALL` declares, checked when
        // it is the mode in force. One pass: the rows are both the body and the
        // width, so the modes cannot appear twice.
        let appearance: Vec<MenuRow> = AppTheme::ALL
            .iter()
            .map(|mode| MenuRow {
                label: mode.label(),
                key: None,
                checked: self.theme_mode == *mode,
                enabled: true,
                action: Some(Message::SetTheme(*mode)),
            })
            .collect();
        menu_card(
            vec![
                MenuSection {
                    rows: actions,
                    footer: None,
                },
                MenuSection {
                    rows: appearance,
                    footer: None,
                },
            ],
            scheme,
        )
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
    /// Keep the karaoke word list in step with the line under the playhead.
    ///
    /// Reading the sidecar once per line change is cheaper than caching words
    /// for every line, and it cannot drift after an edit.
    fn refresh_karaoke(&mut self) {
        let active = if self.karaoke && !self.join_lines && self.player.is_some() {
            self.active_line_index()
        } else {
            None
        };
        let source = self.shown_output().map(Path::to_path_buf);
        if active == self.karaoke_line && source == self.karaoke_source {
            return;
        }
        if source != self.karaoke_source {
            self.karaoke_all = source
                .as_deref()
                .map(backend::read_all_line_words)
                .unwrap_or_default();
        }
        self.karaoke_line = active;
        self.karaoke_source = source;
        self.karaoke_words = active
            .and_then(|index| self.karaoke_all.get(index).cloned())
            .unwrap_or_default();
    }

    /// The karaoke payload for one line, when it is the active one and the
    /// player is open.
    fn karaoke_for(&self, index: usize, active: bool) -> Option<(&[Word], f32, bool)> {
        (active
            && self.karaoke
            && !self.join_lines
            && self.player.is_some()
            && self.karaoke_line == Some(index)
            && !self.karaoke_words.is_empty())
        .then_some((
            self.karaoke_words.as_slice(),
            self.playhead,
            self.player.as_ref().is_some_and(Player::is_playing),
        ))
    }

    fn active_line_index(&self) -> Option<usize> {
        if self.shown_lines().is_empty() {
            return None;
        }
        let live_streaming = self.live_streaming();
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

    /// The collapsed player's scrubber: one thin seek bar in place of the
    /// waveform, so a collapsed card can still show and set the position.
    fn scrubber_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let duration = self.shown_duration().unwrap_or(0.0).max(0.0);
        // The same gates the transport row uses: a locked player and a file with
        // no duration both leave the bar inert.
        let enabled = self.can(license::Feature::Playback) && duration > 0.0;
        let progress = if duration > 0.0 {
            (self.playhead / duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        canvas(Scrubber {
            progress,
            duration,
            // `track`, not `timeline_bg`: this sits directly on the card, and in
            // the dark palette `timeline_bg` is the same grey as `panel_bg`, so
            // the bar would vanish.
            track: scheme.track,
            played: scheme.progress,
            playhead: scheme.ink,
            enabled,
        })
        .width(Length::Fill)
        .height(Length::Fixed(SCRUBBER_HEIGHT))
        .into()
    }

    /// Waveform/speaker timeline strip (greyed out until audio is probed).
    fn timeline_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let duration = self.shown_duration().unwrap_or(0.0);
        let enabled = duration > 0.0 && !self.shown_peaks().is_empty();
        let locked = !self.can(license::Feature::Playback);
        let program = Timeline {
            peaks: self.shown_peaks_arc(),
            turns: Arc::new(if enabled {
                self.speaker_turns(scheme)
            } else {
                Vec::new()
            }),
            duration,
            playhead: if locked { 0.0 } else { self.playhead },
            track: if locked {
                scheme.track
            } else {
                scheme.timeline_bg
            },
            wave: if locked { scheme.muted } else { scheme.green },
            playhead_color: if locked { scheme.muted } else { scheme.ink },
            enabled,
            zoom: self.waveform_zoom,
        };
        let strip = canvas(program)
            .width(Length::Fill)
            .height(Length::Fixed(TIMELINE_HEIGHT));
        let mut column = column![].spacing(4).width(Length::Fill);
        if locked {
            let click = mouse_area(strip)
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::LockedPlayer);
            column = column.push(self.tip(click, "Player is a Pro feature", scheme));
        } else if enabled {
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
        let turns = self.shown_turns();
        // Diarization turns carry exact ranges, but they are absent after a
        // reload (the clip cache is keyed on the diarizer) and never existed
        // for transcripts the app did not produce. Only trust them when every
        // label they mention is one the transcript actually shows.
        let turns_are_complete = !turns.is_empty()
            && turns
                .iter()
                .all(|turn| order.iter().any(|name| *name == turn.speaker));
        if turns_are_complete {
            return turns
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
                .collect();
        }
        // Otherwise rebuild the bands from the transcript itself so the strip
        // keeps the colours the text and the legend are already using.
        let lines = self.shown_lines();
        let duration = self.shown_duration();
        let order_lines = self.display_order();
        let mut spans = Vec::new();
        for (rank, index) in order_lines.iter().enumerate() {
            let Some(line) = lines.get(*index) else {
                continue;
            };
            let Some(start) = line.start else {
                continue;
            };
            // The band runs to the next timed line, or to the end of the
            // audio for the last one.
            let end = order_lines[rank + 1..]
                .iter()
                .find_map(|next| lines.get(*next).and_then(|line| line.start))
                .or(duration)
                .unwrap_or(start);
            if end <= start {
                continue;
            }
            let palette = order.iter().position(|name| *name == line.speaker);
            spans.push(TimelineTurn {
                start,
                end,
                color: speaker_palette(palette.unwrap_or(0), scheme),
            });
        }
        spans
    }

    /// Playback controls: play/pause, time, and a speed selector.
    fn player_element(&self, scheme: &Scheme) -> Element<'_, Message> {
        let duration = self.shown_duration().unwrap_or(0.0).max(0.0);
        let locked = !self.can(license::Feature::Playback);
        let ready = !locked && self.player_ready();
        let playing = self
            .player
            .as_ref()
            .is_some_and(|player| player.is_playing());
        let glyph = if ready { scheme.ink } else { scheme.muted };

        let play_icon = if playing { ICON_PAUSE } else { ICON_PLAY };
        let play_button: Element<Message> = if locked {
            self.tip(
                action(icon_colored(ICON_PLAY, 15.0, scheme.muted))
                    .on_press(Message::LockedPlayer)
                    .style(greyed_button(*scheme))
                    .padding([8, 10]),
                "Player is a Pro feature",
                scheme,
            )
        } else if ready {
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
        let skip_back: Element<Message> = if locked {
            self.tip(
                action(icon_colored(ICON_REWIND, 13.0, scheme.muted))
                    .on_press(Message::LockedPlayer)
                    .style(greyed_button(*scheme)),
                "Player is a Pro feature",
                scheme,
            )
        } else {
            self.tip(
                action(icon_colored(ICON_REWIND, 13.0, glyph))
                    .on_press_maybe(ready.then_some(Message::SeekBy(-5.0)))
                    .style(pill_button_secondary),
                "Back 5 seconds",
                scheme,
            )
        };
        let skip_forward: Element<Message> = if locked {
            self.tip(
                action(icon_colored(ICON_FORWARD, 13.0, scheme.muted))
                    .on_press(Message::LockedPlayer)
                    .style(greyed_button(*scheme)),
                "Player is a Pro feature",
                scheme,
            )
        } else {
            self.tip(
                action(icon_colored(ICON_FORWARD, 13.0, glyph))
                    .on_press_maybe(ready.then_some(Message::SeekBy(5.0)))
                    .style(pill_button_secondary),
                "Forward 5 seconds",
                scheme,
            )
        };

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

        let speed: Element<Message> = if locked {
            self.tip(
                stack![
                    pick_list(Speed::ALL, Some(Speed::One), Message::SpeedChanged)
                        .style(greyed_pick(*scheme))
                        .menu_style(pick_menu_style(*scheme))
                        .handle(caret_handle()),
                    mouse_area(
                        container(horizontal_space())
                            .width(Length::Fill)
                            .height(Length::Fill),
                    )
                    .interaction(mouse::Interaction::Pointer)
                    .on_press(Message::LockedPlayer),
                ],
                "Player is a Pro feature",
                scheme,
            )
        } else if ready {
            pick_list(
                Speed::ALL,
                Some(Speed::from_factor(self.playback_speed)),
                Message::SpeedChanged,
            )
            .style(speed_pick(*scheme))
            .menu_style(pick_menu_style(*scheme))
            .handle(caret_handle())
            .into()
        } else {
            stack![
                pick_list(Speed::ALL, Some(Speed::One), Message::SpeedChanged)
                    .style(greyed_pick(*scheme))
                    .menu_style(pick_menu_style(*scheme))
                    .handle(caret_handle()),
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

    /// The Find controls: the field, previous/next and the match count. The row
    /// is hidden until Ctrl+F and capped at a third of the window by the caller.
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
        // The field takes every pixel the row can spare, so the arrows and the
        // count run up to its right edge.
        let query_input = text_input("Find", &self.search_query)
            .id(find_id())
            .on_input(Message::SearchChanged)
            .width(Length::Fill)
            .style(pill_input);
        // Same constructor as the line-edit panel's pills, so both rows are the
        // same height by construction rather than by two numbers agreeing.
        let prev = icon_pill(ICON_ARROW_UP, scheme.ink)
            .on_press(Message::FindPrev)
            .style(pill_button_secondary);
        let next = icon_pill(ICON_ARROW_DOWN, scheme.ink)
            .on_press(Message::FindNext)
            .style(pill_button_secondary);
        // Directly after the next arrow, with the row's normal gap: no reserved
        // slot, so "3 / 12" reads as part of the navigation rather than as a
        // label parked at the edge of the row.
        let status = text(status)
            .size(11)
            .wrapping(NO_WRAP)
            .color(status_color);
        let controls = row![query_input, prev, next, status]
            .spacing(theme::SPACE_SM)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill);
        // The speaker filter and Rename All used to sit here. Both cost the
        // field part of a row it had to share; the field keeps the space, and
        // Rename All now lives in the Edit menu.
        controls.into()
    }

    /// Hide the Find row and drop what it was searching, so no invisible query
    /// keeps filtering or highlighting.
    fn close_find(&mut self) {
        self.find_open = false;
        self.find_focus_pending = false;
        self.find_hover = false;
        self.search_query.clear();
        self.search_index = 0;
    }

    /// The dialog that owns the screen, if any. One at a time.
    ///
    /// This is the only place the precedence between dialogs is stated: the
    /// order is [`Modal`]'s variant order, which is also the paint order in
    /// `view`. Escape, the pointer gate and the view all ask this one question,
    /// so they cannot disagree about which dialog is in front.
    fn open_modal(&self) -> Option<Modal> {
        if self.reset_confirm {
            Some(Modal::Reset)
        } else if self.summary_open {
            Some(Modal::Summary)
        } else if self.delete_confirm {
            Some(Modal::DeleteLine)
        } else if self.calibration.is_some() {
            Some(Modal::Calibration)
        } else if self.paywall_open {
            Some(Modal::Paywall)
        } else if self.settings_open {
            Some(Modal::Settings)
        } else if self.help_open.is_some() {
            Some(Modal::Help)
        } else if self.run_setup_open {
            Some(Modal::RunSetup)
        } else if self.rename_open {
            Some(Modal::Rename)
        } else if self.vocabulary_open {
            Some(Modal::Vocabulary)
        } else if self.briefing.is_some() {
            Some(Modal::Briefing)
        } else {
            None
        }
    }

    /// Close one dialog. Returns the follow-up work a close owes.
    ///
    /// Calibration is the only close with side effects: it remembers the preset
    /// that was skipped and starts the queued run if the run was waiting on the
    /// offer. While it is still testing the device it ignores the request.
    fn close_modal(&mut self, modal: Modal) -> Task<Message> {
        match modal {
            Modal::Reset => self.reset_confirm = false,
            Modal::Summary => {
                self.summary_open = false;
                // The tips and the rename were asked for before the summary was
                // ready to show; closing it is the moment to work through them.
                return self.after_completion();
            }
            Modal::DeleteLine => self.delete_confirm = false,
            Modal::Calibration => {
                let Some(calibration) = &self.calibration else {
                    return Task::none();
                };
                if calibration.testing() {
                    return Task::none();
                }
                let preset = calibration.preset;
                let run = calibration.start_run;
                self.calibration = None;
                self.remember_skip(preset);
                if run {
                    self.begin_batch(false);
                }
            }
            Modal::Paywall => self.paywall_open = false,
            Modal::Settings => self.settings_open = false,
            Modal::Help => {
                let was_tips = self.help_open == Some(HelpKind::EditingTips);
                self.help_open = None;
                // The tips are the middle of the completion chain; a reference
                // dialog the reader opened themselves is not.
                if was_tips {
                    return self.after_completion();
                }
            }
            Modal::RunSetup => self.run_setup_open = false,
            Modal::Rename => self.rename_open = false,
            Modal::Vocabulary => self.vocabulary_open = false,
            Modal::Briefing => {
                if let Some(BriefingState::Running(run)) = self.briefing.take() {
                    backend::kill(run.pid.load(Ordering::SeqCst));
                }
                self.briefing = None;
            }
        }
        Task::none()
    }

    /// What to show after a dialog in the completion chain closes: the Editing
    /// Tips card first (unless turned off), then the Rename Speakers form if the
    /// run was the one that called for it.
    ///
    /// The flag is re-read here rather than when it was set, so ticking the box
    /// on the card takes effect at once and an unticked box brings the card back
    /// on the next run.
    fn after_completion(&mut self) -> Task<Message> {
        if self.pending_tips {
            self.pending_tips = false;
            if self.editing_tips {
                self.help_open = Some(HelpKind::EditingTips);
                self.toast = None;
                return Task::none();
            }
        }
        if self.pending_rename {
            self.pending_rename = false;
            return self.open_rename();
        }
        Task::none()
    }

    /// True while any modal dialog is showing.
    ///
    /// The inline line editor is deliberately not a popup: it lives in the
    /// transcript, so hover chrome and tooltips keep working around it.
    fn popup_open(&self) -> bool {
        self.open_modal().is_some()
    }

    /// True while a transient menu owns the pointer: one of the transcript
    /// header's menus, or a pick-list dropdown that reports itself open (the
    /// line editor's speaker picker).
    ///
    /// Kept apart from [`Self::popup_open`] because a menu is not modal. It owns
    /// the pointer, so nothing under it may light up or take a press, but it
    /// leaves the keyboard and the tooltips alone: a menu's own buttons carry
    /// tooltips, and those must keep working while it is open.
    fn menu_open(&self) -> bool {
        self.header_menu.is_some()
    }

    /// True while nothing under the pointer may react: a dialog is up, or a menu
    /// is. Every hover and press affordance that the pointer drives is gated on
    /// this, so an open menu cannot be clicked or hovered through.
    fn pointer_blocked(&self) -> bool {
        self.popup_open() || self.menu_open()
    }

    /// Wrap `content` with a tooltip, unless a dialog is open.
    fn tip<'a>(
        &self,
        content: impl Into<Element<'a, Message>>,
        label: impl Into<String>,
        scheme: &Scheme,
    ) -> Element<'a, Message> {
        let content = content.into();
        let label = label.into();
        // A tooltip on the page underneath must not paint over a dialog.
        if self.popup_open() {
            return content;
        }
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

    fn calibration_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let Some(calibration) = &self.calibration else {
            return container(horizontal_space()).into();
        };
        let preset = calibration.preset.label();
        let close = action(icon(ICON_X, 14.0))
            .on_press(Message::CloseCalibration)
            .style(subtle_icon_button);
        let header = row![
            text("GPU Calibration").size(theme::TEXT_LG),
            horizontal_space(),
            close,
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let mut body = column![header].spacing(10);

        match calibration.phase {
            CalibrationPhase::Offered => {
                body = body.push(
                    text(format!("Test GPU acceleration for the {preset} model?"))
                        .size(theme::TEXT_MD)
                        .color(scheme.text),
                );
                body = body.push(
                    text(
                        "Compares the CPU and GPU on this PC. About 20\u{2013}40 seconds. \
                         Uses a bundled sample, not your audio.",
                    )
                    .size(theme::TEXT_SM)
                    .color(scheme.muted),
                );
                body = body.push(
                    row![
                        horizontal_space(),
                        action_text("Use CPU")
                            .on_press(Message::UseCpuForRun)
                            .style(pill_button_secondary),
                        action_text("Test GPU")
                            .on_press(Message::TestGpu)
                            .style(color_button(scheme.blue, scheme.on_accent)),
                    ]
                    .spacing(8),
                );
            }
            CalibrationPhase::TestingCpu | CalibrationPhase::TestingGpu => {
                let phase = if calibration.phase == CalibrationPhase::TestingCpu {
                    "Testing CPU"
                } else {
                    "Testing GPU"
                };
                body = body.push(
                    text(format!("{phase} \u{00b7} {preset}"))
                        .size(theme::TEXT_MD)
                        .color(scheme.text),
                );
                body = body.push(
                    progress_bar(0.0..=1.0, calibration.progress)
                        .width(Length::Fill)
                        .height(Length::Fixed(10.0))
                        .style(move |_theme: &Theme| progress_bar::Style {
                            background: scheme.bar_track.into(),
                            bar: scheme.blue.into(),
                            border: iced::border::rounded(5.0),
                        }),
                );
                body = body.push(
                    text(format!("{:.0}%", calibration.progress * 100.0))
                        .size(theme::TEXT_XS)
                        .color(scheme.muted),
                );
                body = body.push(
                    text("This compares the model on both backends before your run starts.")
                        .size(theme::TEXT_XS)
                        .color(scheme.muted),
                );
            }
            CalibrationPhase::Result => {
                if let Some(error) = &calibration.error {
                    body = body.push(
                        text("GPU unavailable")
                            .size(theme::TEXT_MD)
                            .color(scheme.danger),
                    );
                    body = body.push(
                        text(error)
                            .size(theme::TEXT_SM)
                            .color(scheme.muted)
                            .width(Length::Fill),
                    );
                    body = body.push(
                        row![
                            horizontal_space(),
                            action_text("Continue on CPU")
                                .on_press(Message::UseCpuForRun)
                                .style(color_button(scheme.blue, scheme.on_accent)),
                        ]
                        .spacing(8),
                    );
                } else {
                    let multiplier = calibration.multiplier.unwrap_or(1.0);
                    let faster = multiplier >= 1.0;
                    body = body.push(
                        row![
                            text("CPU 1\u{00d7}")
                                .size(theme::TEXT_SM)
                                .color(scheme.muted),
                            text(format!("GPU {multiplier:.1}\u{00d7}"))
                                .size(theme::TEXT_SM)
                                .color(if faster { scheme.green } else { scheme.muted }),
                        ]
                        .spacing(16),
                    );
                    body = body.push(
                        text(if faster {
                            format!(
                                "GPU is {multiplier:.1}\u{00d7} CPU. Use GPU for this run \
                                 and make it the default?"
                            )
                        } else {
                            format!("GPU is {multiplier:.1}\u{00d7} CPU. Keep CPU?")
                        })
                        .size(theme::TEXT_SM)
                        .color(scheme.text),
                    );
                    let keep = action_text("Keep CPU").on_press(Message::UseCpuForRun);
                    let gpu = action_text(if faster { "Use GPU" } else { "Use GPU anyway" })
                        .on_press(Message::UseGpuForRun);
                    let (keep, gpu) = if faster {
                        (
                            keep.style(pill_button_secondary),
                            gpu.style(color_button(scheme.blue, scheme.on_accent)),
                        )
                    } else {
                        (
                            keep.style(color_button(scheme.blue, scheme.on_accent)),
                            gpu.style(pill_button_secondary),
                        )
                    };
                    body = body.push(row![horizontal_space(), keep, gpu].spacing(8));
                }
            }
        }

        modal_card(body, 460.0, scheme)
    }

    /// The reset warning. Reverting discards every edit, so it asks first and
    /// says what it goes back to.
    fn reset_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let body = column![
            text("Warning").size(20),
            text("Resetting reverts to the initial transcript.")
                .size(theme::TEXT_SM)
                .color(scheme.muted),
            row![
                horizontal_space(),
                action_text("Cancel")
                    .on_press(Message::CloseResetConfirm)
                    .style(pill_button_secondary),
                action_text("Reset")
                    .on_press(Message::ConfirmResetTranscript)
                    .style(color_button(scheme.danger, scheme.on_accent)),
            ]
            .spacing(8),
        ]
        .spacing(10);
        modal_card(body, 420.0, scheme)
    }

    /// The completion summary: per-file statistics, with a table when the run
    /// had several files.
    fn summary_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let lines = self.stats_lines();
        // Same typography and section treatment as the About dialog: a
        // 20 px title, muted body copy, section labels and dividers.
        enum Block {
            Section(String, Vec<String>),
            Table(Vec<String>, Vec<Vec<String>>),
        }
        let mut blocks: Vec<Block> = Vec::new();
        for line in &lines {
            match line {
                SummaryLine::File(name) => {
                    blocks.push(Block::Section(name.clone(), Vec::new()))
                }
                SummaryLine::Body(text_line) => match blocks.last_mut() {
                    Some(Block::Section(_, rows)) => rows.push(text_line.clone()),
                    Some(Block::Table(..)) | None => {
                        blocks.push(Block::Section(String::new(), vec![text_line.clone()]))
                    }
                },
                SummaryLine::Table(headers, rows) => {
                    blocks.push(Block::Table(headers.clone(), rows.clone()))
                }
            }
        }

        let mut body = column![text("Summary").size(20)].spacing(8);
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                body = body.push(settings_divider(scheme));
            }
            match block {
                Block::Section(name, rows) => {
                    let mut section = column![].spacing(4);
                    if !name.is_empty() {
                        // Owned string: the dialog outlives the local list.
                        section = section.push(
                            text(name.clone())
                                .size(11)
                                .wrapping(NO_WRAP)
                                .font(Font {
                                    weight: font::Weight::Bold,
                                    ..Font::DEFAULT
                                })
                                .color(scheme.muted),
                        );
                    }
                    for row in rows {
                        section = section.push(
                            text(row.clone()).size(theme::TEXT_SM).color(scheme.muted),
                        );
                    }
                    body = body.push(section);
                }
                Block::Table(headers, rows) => {
                    body = body.push(summary_table(headers, rows, scheme));
                }
            }
        }

        // Cap the scrollable height so long queues stay usable.
        let cap = (self.window_size.height * 0.8).max(300.0);
        let table_rows: f32 = blocks
            .iter()
            .map(|block| match block {
                Block::Table(_, rows) => rows.len() as f32 + 1.0,
                Block::Section(..) => 0.0,
            })
            .sum();
        let estimated =
            (lines.len() as f32 + blocks.len() as f32 + table_rows + 1.0) * 22.0 + 24.0;
        let height = estimated.min(cap);

        let scroll = scrollable(body)
            .id(summary_id())
            .spacing(theme::SPACE_SM)
            .style(visible_scrollbar_style)
            .width(Length::Fill)
            .height(Length::Fixed(height));

        let col = column![
            container(scroll).width(Length::Fill),
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

        // The tabulation needs six columns; the plain per-file summary
        // does not.
        let wide = blocks.iter().any(|block| matches!(block, Block::Table(..)));
        modal_card(col, if wide { 520.0 } else { 460.0 }, scheme)
    }

    /// The delete confirmation, previewing the line that is about to go.
    ///
    /// `None` when the editor no longer has a row: the confirmation must not
    /// draw a scrim over nothing. The caller falls back to the bare screen.
    fn delete_line_dialog(&self, scheme: &Scheme) -> Option<Element<'_, Message>> {
        let scheme = *scheme;
        let row = self.line_edit.as_ref().and_then(|edit| edit.current_row())?;
        let preview = row.content.text().trim_end().replace(['\n', '\r'], " ");
        let preview = ellipsize(preview.trim(), 72);
        let speaker = row.speaker.trim();
        let preview = if speaker.is_empty() {
            preview
        } else {
            format!("{speaker}: {preview}")
        };
        // An item view is rewritten on every edit; the live transcript
        // is only written when the user saves. Say which one this is,
        // rather than promising a file write that does not happen.
        let sentence = if self.selected_view.is_some() {
            "This removes the line from the transcript and writes the file. \
             A backup is kept next to it, and Undo puts it back."
        } else {
            "This removes the line from the transcript. The file changes when \
             you save, the save keeps a backup, and Undo puts the line back."
        };
        let body = column![
            text("Delete line?").size(20),
            text(sentence)
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
        Some(modal_card(body, 440.0, scheme))
    }

    /// The About and Shortcuts reference.
    ///
    /// Console and Settings never reach here: they open a panel and a dialog of
    /// their own rather than setting `help_open`.
    fn help_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let Some(kind) = self.help_open else {
            return container(horizontal_space()).into();
        };
        let mut col = column![text(kind.label()).size(20)].spacing(8);
        match kind {
            HelpKind::Shortcuts => {
                // Key in a fixed-width column so the descriptions line up. 140
                // leaves the longest description room to sit on one line in the
                // 460 px card.
                const KEY_WIDTH: f32 = 140.0;
                for (key, description) in [
                    ("Ctrl+O", "Add files"),
                    ("Ctrl+S", "Save transcript as"),
                    ("Ctrl+,", "Settings"),
                    ("Ctrl+F", "Find"),
                    ("Ctrl+Enter", "Transcribe"),
                    ("Ctrl + / - / 0", "Transcript font zoom"),
                    ("Space", "Play / pause"),
                    ("Left / Right", "Seek \u{00b1}5 s"),
                    ("Up / Down", "Move through the transcript"),
                    ("F", "Follow the active line"),
                    (
                        "T / S / J / K / C",
                        "Timestamps / SMS / Join / Karaoke / Confidence",
                    ),
                    ("Alt+Enter", "Edit the active line"),
                    ("Alt+Up / Alt+Down", "Join the edited line up / down"),
                    ("Ctrl+Shift+K", "Split the edited line at cursor"),
                    ("Ctrl+Z", "Undo typing in the line field"),
                    // Enter means one thing in the search and another in the
                    // editor, so each context gets its own row rather than one
                    // line standing for both.
                    ("Enter", "Search: next match"),
                    ("Shift+Enter", "Search: previous match"),
                    ("Enter", "Line editor: save"),
                    ("Esc", "Cancel the editor or dismiss"),
                    ("Tab", "Rename dialog: next field"),
                ] {
                    col = col.push(
                        row![
                            text(key)
                                .size(theme::TEXT_SM)
                                .width(Length::Fixed(KEY_WIDTH)),
                            text(description).size(theme::TEXT_SM).color(scheme.muted),
                        ]
                        .spacing(12),
                    );
                }
            }
            HelpKind::About => {
                col =
                    col.push(
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
                        text("The name comes from HIPAA. Your recordings stay on this machine.")
                            .size(theme::TEXT_SM)
                            .color(scheme.muted),
                        text("Diarization: pyannote segmentation + wespeaker (sherpa-onnx).")
                            .size(theme::TEXT_SM)
                            .color(scheme.muted),
                        text("Transcription: whisper.cpp.")
                            .size(theme::TEXT_SM)
                            .color(scheme.muted),
                        text("Runs offline once the models are downloaded.")
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
                        text("Once models are installed the free tier makes no network requests.")
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
                        text("pyannote (MIT), wespeaker (CC BY 4.0), Whisper (MIT), Qwen2.5 (Apache-2.0).")
                            .size(theme::TEXT_SM)
                            .color(scheme.muted),
                        text("Help → Licenses has the full notices.")
                            .size(theme::TEXT_SM)
                            .color(scheme.muted),
                    ]
                    .spacing(4),
                );
            }
            HelpKind::Licenses => {
                let mut notices = column![].spacing(4).width(Length::Fill);
                for line in LICENSES_TEXT.lines() {
                    let line = line.trim_end();
                    if line.is_empty() {
                        notices = notices.push(container(horizontal_space()).height(Length::Fixed(6.0)));
                    } else if let Some(heading) = line.strip_prefix("## ") {
                        notices = notices.push(
                            text(heading.to_string())
                                .size(theme::TEXT_SM)
                                .font(Font {
                                    weight: font::Weight::Bold,
                                    ..Font::DEFAULT
                                }),
                        );
                    } else if let Some(heading) = line.strip_prefix("# ") {
                        notices = notices.push(
                            text(heading.to_string()).size(theme::TEXT_MD).font(Font {
                                weight: font::Weight::Bold,
                                ..Font::DEFAULT
                            }),
                        );
                    } else {
                        notices = notices.push(
                            text(line.to_string())
                                .size(theme::TEXT_XS)
                                .color(scheme.muted),
                        );
                    }
                }
                col = col.push(
                    scrollable(notices)
                        .height(Length::Fixed(340.0))
                        .width(Length::Fill)
                        .style(visible_scrollbar_style),
                );
            }
            HelpKind::EditingTips => {
                for (title, detail) in [
                    ("Double-click Speaker", "Enter editing mode"),
                    ("Right-Click Speaker", "Change Speaker"),
                    ("Edit >> Rename Speakers", "Bulk rename speakers"),
                ] {
                    col = col.push(
                        column![
                            text(title).size(theme::TEXT_MD).font(Font {
                                weight: font::Weight::Bold,
                                ..Font::DEFAULT
                            }),
                            text(detail).size(theme::TEXT_SM).color(scheme.muted),
                        ]
                        .spacing(2),
                    );
                }
                col = col.push(settings_divider(scheme));
            }
            HelpKind::Console | HelpKind::Settings => {}
        }
        // The tips carry their own switch, and it shares the row with Close.
        // Only the box is pressable: the words beside it are a label, not a
        // second way to hit the same toggle.
        let mut footer = row![].spacing(8).align_y(iced::Alignment::Center);
        footer = footer.push(
            action_text("Close")
                .on_press(Message::CloseHelp)
                .style(color_button(scheme.blue, scheme.on_accent)),
        );
        footer = footer.push(horizontal_space());
        if kind == HelpKind::EditingTips {
            footer = footer.push(
                action(tips_check(!self.editing_tips, scheme))
                    .padding([4, 4])
                    .style(header_menu_item_style(scheme, true))
                    .on_press(Message::SetEditingTips(!self.editing_tips)),
            );
            footer = footer.push(
                text("Don't show these tips again")
                    .size(theme::TEXT_XS)
                    .wrapping(NO_WRAP)
                    .color(scheme.muted),
            );
        }
        col = col.push(container(footer).width(Length::Fill));
        modal_card(col, 460.0, scheme)
    }

    /// The speaker rename form: one field per speaker, saved as a batch.
    fn rename_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
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
        modal_card(col, 380.0, scheme)
    }

    fn briefing_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let title = text("Briefing").size(theme::TEXT_LG);
        let close = action_text("Close")
            .on_press(Message::CloseBriefing)
            .style(pill_button_secondary);

        let mut body = column![title].spacing(theme::SPACE_MD).width(Length::Fill);

        match &self.briefing {
            Some(BriefingState::NeedsModel) => {
                body = body.push(
                    text(
                        "A briefing needs a one-time 1.1 GB model. It runs on this machine; \
                         the transcript is not uploaded.",
                    )
                    .size(theme::TEXT_XS)
                    .color(scheme.muted),
                );
                if let Some(state) = &self.model_download {
                    let fraction = state
                        .total
                        .filter(|total| *total > 0)
                        .map(|total| (state.done as f32 / total as f32).clamp(0.0, 1.0))
                        .unwrap_or(0.0);
                    body = body
                        .push(text(state.label.clone()).size(theme::TEXT_SM))
                        .push(
                            progress_bar(0.0..=1.0, fraction)
                                .height(Length::Fixed(8.0))
                                .style(move |_theme| progress_bar::Style {
                                    background: scheme.bar_track.into(),
                                    bar: scheme.progress.into(),
                                    border: iced::border::rounded(4.0),
                                }),
                        )
                        .push(right_aligned(row![
                            action_text("Cancel")
                                .on_press(Message::CancelModelDownload)
                                .style(pill_button_secondary),
                        ]));
                } else if download::briefing_ready() {
                    body = body.push(right_aligned(
                        row![
                            action_text("Create briefing")
                                .on_press(Message::RetryBriefing)
                                .style(color_button(scheme.blue, scheme.on_accent)),
                            close,
                        ]
                        .spacing(theme::SPACE_MD),
                    ));
                } else {
                    if let Some(error) = &self.model_error {
                        body = body.push(
                            text(error.clone())
                                .size(theme::TEXT_XS)
                                .color(scheme.danger),
                        );
                    }
                    body = body.push(right_aligned(
                        row![
                            action_text("Download model")
                                .on_press(Message::DownloadBriefingModel)
                                .style(color_button(scheme.blue, scheme.on_accent)),
                            close,
                        ]
                        .spacing(theme::SPACE_MD),
                    ));
                }
            }
            Some(BriefingState::Running(run)) => {
                body = body
                    .push(text(run.stage.clone()).size(theme::TEXT_SM).color(scheme.text))
                    .push(
                        progress_bar(0.0..=1.0, run.fraction)
                            .height(Length::Fixed(8.0))
                            .style(move |_theme| progress_bar::Style {
                                background: scheme.bar_track.into(),
                                bar: scheme.progress.into(),
                                border: iced::border::rounded(4.0),
                            }),
                    )
                    .push(
                        text(format!(
                            "{} elapsed. A long recording can take several minutes on a CPU; \
                             cancelling is always safe.",
                            format_secs(run.started.elapsed().as_secs_f64())
                        ))
                        .size(theme::TEXT_XS)
                        .color(scheme.muted),
                    )
                    .push(right_aligned(row![
                        action_text("Cancel")
                            .on_press(Message::CloseBriefing)
                            .style(pill_button_secondary),
                    ]));
            }
            Some(BriefingState::Failed(message)) => {
                body = body
                    .push(
                        text(message.clone())
                            .size(theme::TEXT_SM)
                            .color(scheme.danger),
                    )
                    .push(right_aligned(
                        row![
                            action_text("Try again")
                                .on_press(Message::RetryBriefing)
                                .style(color_button(scheme.blue, scheme.on_accent)),
                            close,
                        ]
                        .spacing(theme::SPACE_MD),
                    ));
            }
            Some(BriefingState::Ready { briefing, lines, .. }) => {
                let mut content = column![].spacing(theme::SPACE_MD).width(Length::Fill);
                if !briefing.overview.trim().is_empty() {
                    content = content.push(
                        text(briefing.overview.clone())
                            .size(theme::TEXT_SM)
                            .color(scheme.text),
                    );
                }
                for (heading, items) in [
                    ("Key points", &briefing.points),
                    ("Decisions", &briefing.decisions),
                    ("Action items", &briefing.actions),
                ] {
                    if items.is_empty() {
                        continue;
                    }
                    content = content.push(
                        text(heading.to_string()).size(theme::TEXT_SM).font(Font {
                            weight: font::Weight::Bold,
                            ..Font::DEFAULT
                        }),
                    );
                    for item in items {
                        let cite = hippo_briefing::citation(item, lines).unwrap_or_default();
                        content = content.push(
                            row![
                                text("\u{2022}").size(theme::TEXT_SM).color(scheme.muted),
                                text(item.text.clone())
                                    .size(theme::TEXT_SM)
                                    .width(Length::Fill),
                                text(cite).size(theme::TEXT_XS).color(scheme.muted),
                            ]
                            .spacing(theme::SPACE_SM)
                            .align_y(iced::Alignment::Start),
                        );
                    }
                }
                body = body
                    .push(
                        scrollable(content)
                            .height(Length::Fixed(360.0))
                            .width(Length::Fill)
                            .style(visible_scrollbar_style),
                    )
                    .push(right_aligned(
                        row![
                            action_text("Save as Markdown")
                                .on_press(Message::SaveBriefing)
                                .style(color_button(scheme.blue, scheme.on_accent)),
                            close,
                        ]
                        .spacing(theme::SPACE_MD),
                    ));
            }
            None => {}
        }

        modal_card(body, 520.0, scheme)
    }

    fn vocabulary_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let (parsed, bad) = Vocabulary::parse_fields(
            &self.vocab_terms.text(),
            &self.vocab_replacements.text(),
        );
        let (_, dropped) =
            parsed.prompt_with_dropped(hippo_transcribe_core::vocabulary::PROMPT_BUDGET);
        let can_save = bad.is_empty();

        let terms = column![
            text("Terms").size(theme::TEXT_SM).color(scheme.text),
            text("Names, acronyms and jargon. One per line.")
                .size(theme::TEXT_XS)
                .color(scheme.muted),
            vocabulary_field(
                &self.vocab_terms,
                "Kubernetes\nSiobhán\nACME Corp",
                Message::VocabTermsAction,
                scheme,
            ),
        ]
        .spacing(theme::SPACE_XS);

        let replacements = column![
            text("Replacements").size(theme::TEXT_SM).color(scheme.text),
            text("One per line, as heard = shown. Whole words, any case.")
                .size(theme::TEXT_XS)
                .color(scheme.muted),
            vocabulary_field(
                &self.vocab_replacements,
                "pie torch = PyTorch\nacme = ACME Corp",
                Message::VocabReplacementsAction,
                scheme,
            ),
        ]
        .spacing(theme::SPACE_XS);

        let mut body = column![
            text("Custom Vocabulary").size(20),
            terms,
            replacements,
        ]
        .spacing(theme::SPACE_XL);

        if !bad.is_empty() {
            body = body.push(
                text(format!(
                    "Not saved: {} — needs heard = shown",
                    bad.join(", ")
                ))
                .size(theme::TEXT_XS)
                .color(scheme.danger),
            );
        }
        if dropped > 0 {
            body = body.push(
                text(format!(
                    "{dropped} term{} skipped — the decoder hint is full.",
                    if dropped == 1 { "" } else { "s" }
                ))
                .size(theme::TEXT_XS)
                .color(scheme.muted),
            );
        }
        body = body
            .push(
                text("Used on the next run. Transcripts already on screen are not changed.")
                    .size(theme::TEXT_XS)
                    .color(scheme.muted),
            )
            .push(
                container(
                    row![
                        action_text("Save")
                            .on_press_maybe(can_save.then_some(Message::SaveVocabulary))
                            .style(color_button(scheme.blue, scheme.on_accent)),
                        action_text("Cancel")
                            .on_press(Message::CloseVocabulary)
                            .style(pill_button_secondary),
                    ]
                    .spacing(theme::SPACE_MD),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            );

        modal_card(body, 460.0, scheme)
    }

    fn settings_dialog(&self, scheme: &Scheme) -> Element<'_, Message> {
        let scheme = *scheme;
        let running = matches!(self.status, Status::Running);

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
        } else if !self.shows(license::Audience::ProOnly) {
            self.tip(
                stack![
                    pick_list(Preset::ALL, Some(Preset::Balanced), Message::PresetChanged,)
                        .style(greyed_pick(scheme))
                        .menu_style(pick_menu_style(scheme))
                        .handle(caret_handle())
                        .width(Length::Fixed(180.0)),
                    mouse_area(
                        container(horizontal_space())
                            .width(Length::Fill)
                            .height(Length::Fill),
                    )
                    .interaction(mouse::Interaction::Pointer)
                    .on_press(Message::LockedPreset),
                ],
                "Speed and Accuracy presets are Pro features",
                &scheme,
            )
        } else {
            pick_list(Preset::ALL, Some(self.preset), Message::PresetChanged)
                .style(pill_pick(scheme))
                .menu_style(pick_menu_style(scheme))
                .handle(caret_handle())
                .width(Length::Fixed(180.0))
                .into()
        };
        let speakers: Element<Message> = if running {
            text("Locked while transcribing")
                .wrapping(NO_WRAP)
                .size(theme::TEXT_XS)
                .color(scheme.muted)
                .into()
        } else if !self.shows(license::Audience::ProOnly) {
            self.tip(
                stack![
                    pick_list(
                        SpeakerOpt::ALL,
                        Some(SpeakerOpt::from_value(self.num_speakers)),
                        Message::SpeakerCountChanged,
                    )
                    .style(greyed_pick(scheme))
                    .menu_style(pick_menu_style(scheme))
                    .handle(caret_handle())
                    .width(Length::Fixed(180.0)),
                    mouse_area(
                        container(horizontal_space())
                            .width(Length::Fill)
                            .height(Length::Fill),
                    )
                    .interaction(mouse::Interaction::Pointer)
                    .on_press(Message::LockedSpeakers),
                ],
                "Speaker labels are a Pro feature",
                &scheme,
            )
        } else {
            pick_list(
                SpeakerOpt::ALL,
                Some(SpeakerOpt::from_value(self.num_speakers)),
                Message::SpeakerCountChanged,
            )
            .style(pill_pick(scheme))
            .menu_style(pick_menu_style(scheme))
            .handle(caret_handle())
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
            .style(pill_pick(scheme))
            .menu_style(pick_menu_style(scheme))
            .handle(caret_handle())
            .width(Length::Fixed(180.0))
            .into()
        };

        let speed = settings_control(
            pick_list(
                Speed::ALL,
                Some(Speed::from_factor(self.playback_speed)),
                Message::SpeedChanged,
            )
            .style(speed_pick(scheme))
            .menu_style(pick_menu_style(scheme))
            .handle(caret_handle())
            .width(Length::Fixed(180.0)),
        );

        let follow_on = self.follow_playhead;
        let follow = settings_control(
            toggle_button(
                if follow_on { "On" } else { "Off" },
                follow_on,
                Message::ToggleFollowPlayhead,
            ),
        );

        let timestamps_on = self.show_timestamps;
        let timestamps = settings_control(
            toggle_button(
                if timestamps_on { "On" } else { "Off" },
                timestamps_on,
                Message::TimestampsToggled(!timestamps_on),
            ),
        );
        let normalize_on = self.normalize;
        let normalize = settings_control(toggle_button(
            if normalize_on { "On" } else { "Off" },
            normalize_on,
            Message::NormalizeToggled(!normalize_on),
        ));
        let mut inputs = vec![SYSTEM_DEFAULT_DEVICE.to_string()];
        inputs.extend(crate::recording::input_devices());
        let current_mic = self
            .input_device
            .clone()
            .unwrap_or_else(|| SYSTEM_DEFAULT_DEVICE.to_string());
        let microphone: Element<Message> = if inputs.len() <= 1 {
            text("No input devices found")
                .size(theme::TEXT_XS)
                .color(scheme.muted)
                .into()
        } else {
            pick_list(inputs, Some(current_mic), Message::InputDeviceChanged)
                .style(pill_pick(scheme))
                .menu_style(pick_menu_style(scheme))
                .handle(caret_handle())
                .width(Length::Fixed(180.0))
                .into()
        };

        let notifications_on = self.notifications;
        let notifications = settings_control(
            toggle_button(
                if notifications_on { "On" } else { "Off" },
                notifications_on,
                Message::NotificationsToggled(!notifications_on),
            ),
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
        .style(pill_button_secondary)
        .width(Length::Fixed(136.0));
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

        let mut body = column![].spacing(theme::SPACE_SM).width(Length::Fill);
        body = body.push(settings_section_label("Appearance", scheme));
        body = body.push(settings_row("Transcript text size", font_controls, scheme));
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Transcription", scheme));
        body = body.push(settings_row("Preset", preset, scheme));
        body = body.push(settings_row("Speakers", speakers, scheme));
        body = body.push(settings_row("Language", language, scheme));
        body = body.push(settings_row("Show timestamps", timestamps, scheme));
        body = body.push(settings_row("Normalize loudness", normalize, scheme));
        body = body.push(settings_divider(scheme));
        if self.can(license::Feature::Playback) {
            body = body.push(settings_section_label("Playback", scheme));
            body = body.push(settings_row("Speed", speed, scheme));
            body = body.push(settings_row("Follow playhead", follow, scheme));
            body = body.push(settings_divider(scheme));
        }
        body = body.push(settings_section_label("Recording", scheme));
        body = body.push(settings_row("Microphone", microphone, scheme));
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Notifications", scheme));
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
        body = body.push(settings_divider(scheme));
        body = body.push(settings_section_label("Diagnostics", scheme));
        body = body.push(settings_row(
            "Log files",
            action_text("Open")
                .on_press(Message::OpenLogFolder)
                .style(pill_button_secondary),
            scheme,
        ));

        let height = (self.window_size.height * 0.46).clamp(300.0, 360.0);
        // `spacing` embeds the rail and reserves its lane, so the right-hand
        // column of controls cannot end up underneath it.
        let scroll = scrollable(body)
            .spacing(theme::SPACE_SM)
            .style(visible_scrollbar_style)
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

    /// Bottom-right notification layer for the right pane.
    ///
    /// The returned tree keeps the same shape whether or not a toast is up.
    /// Swapping between a bare panel and a stack remounts everything below it,
    /// including the transcript scrollable, whose fresh state reports offset 0
    /// — which is why every line edit (all of them raise a toast) looked like
    /// Follow jumping to the top and back.
    fn with_toast<'a>(
        &'a self,
        base: Element<'a, Message>,
        scheme: Scheme,
        bottom: f32,
    ) -> Element<'a, Message> {
        let toast: Option<Element<'a, Message>> = self.toast.as_ref().map(|(message, _)| {
            let dot_color = match toast_tone(message) {
                ToastTone::Success => scheme.green,
                ToastTone::Info => scheme.blue,
                ToastTone::Danger => scheme.danger,
            };
            let toast_text = ellipsize(message, 44);
            let width =
                ((toast_text.chars().count() as f32 * 7.2 + 46.0).clamp(180.0, 320.0)).max(180.0);
            let dot = container(horizontal_space())
                .width(Length::Fixed(6.0))
                .height(Length::Fixed(6.0))
                .style(move |_theme| container::Style {
                    background: Some(dot_color.into()),
                    border: iced::border::rounded(3.0),
                    ..container::Style::default()
                });
            mouse_area(
                container(
                    row![dot, text(toast_text).size(13).wrapping(NO_WRAP)]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                )
                .width(Length::Fixed(width))
                .height(Length::Fixed(36.0))
                .align_y(iced::Alignment::Center)
                .padding([0, 12])
                .style(move |_theme| container::Style {
                    background: Some(scheme.dialog_bg.into()),
                    text_color: Some(scheme.text),
                    border: iced::border::Border {
                        color: scheme.border,
                        width: 1.0,
                        radius: 12.0.into(),
                    },
                    shadow: soft_shadow(),
                    ..container::Style::default()
                }),
            )
            .on_press(Message::DismissToast)
            .into()
        });
        // No toast: an empty, non-interactive filler so the stack shape (and
        // therefore the scrollable's state) is identical either way.
        let layer: Element<'a, Message> = match toast {
            Some(toast) => toast,
            None => horizontal_space().into(),
        };
        let overlay = container(layer)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::End)
            .align_y(iced::Alignment::End)
            .padding(iced::Padding {
                top: 0.0,
                right: 16.0,
                bottom,
                left: 0.0,
            });
        stack![base, overlay].into()
    }

    /// The Help menu's own layer over the window.
    ///
    /// Help lives in the app header now, outside the transcript card, so its
    /// menu cannot ride the card's stack. This is a stack level of its own,
    /// shaped like the toast's: right-aligned, hanging from the header row. A
    /// full-surface catcher sits under the card and only closes the menu, so a
    /// press anywhere else dismisses and does nothing else; hover needs no
    /// catcher, because `pointer_blocked` already makes the page inert while a
    /// menu is open.
    fn help_menu_layer<'a>(&'a self, scheme: &Scheme) -> Element<'a, Message> {
        let open = self.header_menu == Some(HeaderMenu::Help) && !self.popup_open();
        let layer: Element<'a, Message> = if open {
            let catcher = mouse_area(
                container(horizontal_space())
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .interaction(mouse::Interaction::Idle)
            .on_press(Message::CloseHeaderMenu);
            let card = container(self.help_menu(scheme))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::End)
                .align_y(iced::Alignment::Start)
                .padding(iced::Padding {
                    top: HELP_MENU_TOP,
                    right: OUTER_PADDING,
                    bottom: 0.0,
                    left: 0.0,
                });
            stack![catcher, card].into()
        } else {
            // Closed: an empty filler so the stack keeps its shape.
            horizontal_space().into()
        };
        layer
    }

    pub fn view(&self) -> Element<'_, Message> {
        let scheme = self.scheme();
        let running = matches!(self.status, Status::Running);
        let has_queue = !self.queue.is_empty();
        let global_progress = self.overall_shown;

        let clock = row![
            icon_colored(ICON_CLOCK, 14.0, scheme.text),
            // The wall clock must never wrap or be squeezed out by the ETA
            // suffix: it keeps its intrinsic width and the bar takes the rest.
            text(self.timer_text())
                .size(12)
                .wrapping(NO_WRAP)
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
        let label_el: Element<Message> = if let Some(raw) = decoding_label(&self.stage, &label) {
            text(raw).into()
        } else if self.stage.eq_ignore_ascii_case("Decoding") {
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

        // The bar is drawn rather than composed from a `progress_bar`, so the
        // fill's corner radius can follow its own width: a two-pixel fill has to
        // stay a pill instead of inheriting a 15 px radius it cannot fit.
        //
        // The track is the well colour rather than a darkened `bar_track`: the
        // bar now sits on the raised status surface, and in the dark theme
        // `bar_track` is that same grey, so a darkened copy of it would vanish
        // into the box it is drawn on.
        // In dark mode the run card and the bar's groove trade surfaces: the
        // card becomes the darker timeline grey and the groove the lighter
        // dialog grey, so the bar reads as a well inside the card. Light mode
        // keeps the original pairing.
        let dark = theme::is_dark_scheme(&scheme);
        let island_bg = if dark {
            scheme.timeline_bg
        } else {
            scheme.dialog_bg
        };
        let groove = if dark {
            scheme.dialog_bg
        } else {
            scheme.timeline_bg
        };
        let bar = canvas(ProgressTrack {
            fraction: global_progress.clamp(0.0, 1.0),
            track: groove,
            edge: theme::darken(groove, 0.12),
            fill: theme::lighten(scheme.progress, 0.16),
        })
        .width(Length::Fill)
        .height(Length::Fixed(30.0));
        let label = container(label_el)
            .center_x(Length::Fill)
            .center_y(Length::Fill);
        let progress_stack = stack![bar, label].width(Length::Fill);
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
        // Progress, elapsed time and the stage label describe the run, not the
        // recording, so they get their own surface and disappear when the run
        // does. The waveform and player belong together and stay.
        //
        // The surface is the app's raised one (`dialog_bg`: white paper in the
        // light theme, the elevated grey in the dark theme) rather than the grey
        // well it used to share with the bars themselves. The hairline matters in
        // the light theme, where white on the near-white middle card differs by
        // only three levels; the shadow is the card's own `soft_shadow`, so this
        // is not a new elevation level.
        let status_box = container(status_row)
            .width(Length::Fill)
            .padding([8, 10])
            .style(move |_theme| container::Style {
                background: Some(island_bg.into()),
                text_color: Some(scheme.text),
                border: iced::border::Border {
                    color: scheme.border,
                    width: 1.0,
                    radius: 12.0.into(),
                },
                shadow: soft_shadow(),
            });

        // The player keeps its own card. The run progress used to sit inside it,
        // which stacked one white surface on another; it is a sibling card above
        // the player instead.
        //
        // Expanded, the card is a `Player` title matching the queue card's title
        // (12 px padding, 12 px down to the controls), then the waveform, then
        // the transport row. Collapsed, the title goes, the waveform becomes the
        // thin scrubber above the same transport row, and the caret moves into
        // that row so the card is two lines rather than three.
        let collapse = if self.player_collapsed {
            ICON_CARET_UP
        } else {
            ICON_CARET_DOWN
        };
        // The same caret in both states. The triangle alone: no fill, no border,
        // no shadow, and `subtle_icon_button` only washes it when the pointer is
        // on it, so it reads as a disclosure control rather than as a button.
        let caret = self.tip(
            action(icon(collapse, 16.0))
                .on_press(Message::TogglePlayerCollapsed)
                .padding([4, 4])
                .style(subtle_icon_button),
            if self.player_collapsed {
                "Show the waveform"
            } else {
                "Hide the waveform"
            },
            &scheme,
        );

        let mut player_body = column![].spacing(8).width(Length::Fill);
        if !self.player_collapsed {
            player_body = player_body.push(self.timeline_element(&scheme));
        }
        player_body = player_body.push(self.player_element(&scheme));

        let player_box = if self.player_collapsed {
            // Collapsed: no title. The scrubber takes the waveform's place above
            // the transport row, keeping the expanded order, and the caret ends
            // that row so the card is two lines rather than three.
            container(
                column![
                    self.scrubber_element(&scheme),
                    row![self.player_element(&scheme), horizontal_space(), caret]
                        .spacing(theme::SPACE_SM)
                        .align_y(iced::Alignment::Center),
                ]
                .spacing(theme::SPACE_MD)
                .width(Length::Fill),
            )
        } else {
            let title_row = row![
                text("Player")
                    .size(theme::TEXT_MD)
                    .font(Font {
                        weight: font::Weight::Bold,
                        ..Font::DEFAULT
                    })
                    .wrapping(NO_WRAP),
                horizontal_space(),
                caret,
            ]
            .spacing(theme::SPACE_SM)
            .align_y(iced::Alignment::Center);
            container(
                column![title_row, player_body]
                    .spacing(theme::SPACE_LG)
                    .width(Length::Fill),
            )
        };

        let player_box = player_box
            .width(Length::Fill)
            .padding(12)
            .style(move |_theme| container::Style {
                background: Some(scheme.panel_bg.into()),
                border: island_border(&scheme),
                shadow: soft_shadow(),
                ..container::Style::default()
            });

        let mut middle = column![].spacing(8).width(Length::Fill);
        if running || self.completion_pending {
            middle = middle.push(status_box);
        }
        middle = middle.push(player_box);

        // Fixed slots, fixed indices: the header appears and disappears with
        // the run, and anything that changes the scrollable's child index
        // remounts it and resets its offset. The 6 px gap is padding on the
        // slot so the layout is unchanged.
        let header = self.transcript_header(&scheme);
        let has_header = header.is_some();
        let header_slot = container(match header {
            Some(header) => header,
            None => horizontal_space().into(),
        })
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 0.0,
            right: 0.0,
            bottom: if has_header { HEADER_SLOT_GAP } else { 0.0 },
            left: 0.0,
        });
        // The editor strip sits between the header and the list, inside the card,
        // and only its *height* changes: nothing when no editor is open, so the
        // list runs up to the header. It is always the child before the list, so
        // the scrollable stays child 2 in both states and `Tree::diff` matches it
        // by index — the strip's node swaps between a `Space` and a `Row` (a
        // `Container` has no node of its own), but the scrollable's node and its
        // absolute pixel offset are untouched, which is why its offset is not
        // reset. Shifting the offset to match is `strip_scroll`.
        let strip_slot = container(match self.line_edit_panel(scheme) {
            Some(panel) => panel,
            None => horizontal_space().into(),
        })
        .width(Length::Fill)
        .height(Length::Fixed(editor_strip_height(self.line_edit.is_some())));
        let transcript_box = column![header_slot, strip_slot, self.transcript_element()]
            .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill);

        let left_panel: Element<Message> = self.queue_panel(&scheme, LEFT_PANEL_WIDTH);

        // The transcript card keeps a fixed child index. Anything that appears or
        // disappears above it (model notice, progress chrome) or below it (search
        // row) would shift that index, which remounts the scrollable and resets its
        // offset to the top. The conditional content lives in slots instead, and the
        // gaps between islands are slot padding, so the child indices never change.
        let show_middle = has_queue || self.selected_view.is_some() || running;
        let mut top_slot_col = column![].spacing(ISLAND_GAP).width(Length::Fill);
        let mut has_top = false;
        if let Some(notice) = self.model_notice(&scheme) {
            top_slot_col = top_slot_col.push(notice);
            has_top = true;
        }
        // In the empty state the progress/timeline/player chrome has nothing to
        // act on; let the welcome card own the panel instead.
        if show_middle {
            top_slot_col = top_slot_col.push(middle);
            has_top = true;
        }
        let top_slot = container(top_slot_col)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: 0.0,
                right: 0.0,
                bottom: if has_top { ISLAND_GAP } else { 0.0 },
                left: 0.0,
            });
        // Keep the stack structure stable across hover changes. Swapping
        // between a bare column and a stack remounts the scrollable and resets
        // its scroll position to the top.
        // A card menu hangs under its button but inside the card, so its anchor
        // is a left offset rather than a window coordinate. Closed — and for
        // Help, which lives in the app header — the slot is a same-size space:
        // the stack keeps its shape, so the scrollable's tree index and offset
        // never change.
        let card_menu_open = !self.popup_open();
        let view_layer: Element<Message> = match self.header_menu {
            Some(HeaderMenu::File) if card_menu_open => {
                card_menu_layer(self.file_menu(&scheme), file_menu_offset())
            }
            Some(HeaderMenu::Edit) if card_menu_open => {
                card_menu_layer(self.edit_menu(&scheme), edit_menu_offset())
            }
            Some(HeaderMenu::View) if card_menu_open => {
                card_menu_layer(self.view_menu(&scheme), view_menu_offset())
            }
            _ => container(horizontal_space())
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        };
        let transcript_content: Element<Message> = stack![transcript_box, view_layer].into();
        let transcript_card = container(transcript_content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(scheme.panel_bg.into()),
                border: island_border(&scheme),
                shadow: soft_shadow(),
                ..container::Style::default()
            });
        // The Find row is hidden until Ctrl+F: closed, it takes no height and the
        // transcript card reaches the bottom of the pane. Opening it takes the
        // row plus an island gap from the card, shifting the card's bottom up.
        let find_width = (self.window_size.width / 3.0)
            .max(220.0)
            .min(self.pane_width());
        let search_slot: Element<Message> = if has_queue && self.find_open {
            container(
                mouse_area(self.search_element(&scheme))
                    .on_enter(Message::FindHover(true))
                    .on_exit(Message::FindHover(false)),
            )
            .width(Length::Fixed(find_width))
            .into()
        } else {
            horizontal_space().into()
        };
        let bottom_slot = container(search_slot)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: if has_queue && self.find_open {
                    ISLAND_GAP
                } else {
                    0.0
                },
                right: 0.0,
                bottom: 0.0,
                left: 0.0,
            });
        let right_panel: Element<Message> = column![
            top_slot,
            mouse_area(transcript_card)
                .on_press(Message::CloseHeaderMenu)
                .on_exit(Message::TranscriptLeft),
            bottom_slot,
        ]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        // The toast clears the Find row; the editor strip is inside the card, so it
        // changes nothing down here.
        let toast_bottom = if has_queue && self.find_open {
            48.0
        } else {
            20.0
        };
        let right_panel = self.with_toast(right_panel, scheme, toast_bottom);

        let mut header = row![
            iced::widget::image(header_logo_handle())
                .width(Length::Fixed(HEADER_LOGO_SIZE))
                .height(Length::Fixed(HEADER_LOGO_SIZE)),
            column![
                text("Hippo Transcribe").size(theme::TEXT_XL).font(Font {
                    weight: font::Weight::Bold,
                    ..Font::DEFAULT
                }),
                text("HIPAA inspired offline transcription & diarization")
                    .size(theme::TEXT_XS)
                    .color(scheme.muted),
            ]
            .spacing(theme::SPACE_2XS),
            horizontal_space(),
            self.license_button(&scheme),
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center);
        // Development-only Free/Pro switch, beside the license button.
        if cfg!(debug_assertions) {
            header = header.push(self.tier_toggle_button(&scheme));
        }
        // Help ends the row in every build, so its right edge is the row's right
        // edge — and the menu's, since that is what the layer aligns to. The
        // debug-only toggle is pushed above precisely so it cannot move it.
        header = header.push(self.header_menu_button("Help", HeaderMenu::Help));

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
        // Always wrapped: see `drop_overlay` for why its shape must not change.
        base = drop_overlay(base, scheme, self.drag_hover);
        // The Help menu's layer, over the panels rather than inside the card.
        // Always present for the same reason as the wrap above.
        base = stack![base, self.help_menu_layer(&scheme)].into();

        let dialog = match self.open_modal() {
            None => return base,
            Some(Modal::Reset) => self.reset_dialog(&scheme),
            Some(Modal::Summary) => self.summary_dialog(&scheme),
            Some(Modal::DeleteLine) => match self.delete_line_dialog(&scheme) {
                Some(dialog) => dialog,
                None => return base,
            },
            Some(Modal::Calibration) => self.calibration_dialog(&scheme),
            Some(Modal::Paywall) => self.paywall_dialog(&scheme),
            Some(Modal::Settings) => self.settings_dialog(&scheme),
            Some(Modal::Help) => self.help_dialog(&scheme),
            Some(Modal::RunSetup) => self.run_setup_dialog(&scheme),
            Some(Modal::Rename) => self.rename_dialog(&scheme),
            Some(Modal::Vocabulary) => self.vocabulary_dialog(&scheme),
            Some(Modal::Briefing) => self.briefing_dialog(&scheme),
        };
        modal_overlay(base, dialog, scheme)
    }
}

/// A speaker turn, resolved to a draw colour.
#[derive(Debug, Clone)]
struct TimelineTurn {
    start: f32,
    end: f32,
    color: Color,
}

/// Height of the collapsed player's scrubber canvas and of the line drawn inside
/// it. The line is the visible control; the canvas is four times as tall so the
/// pointer has something to land on, and only 12 px so the collapsed card stays
/// short.
const SCRUBBER_HEIGHT: f32 = 12.0;
const SCRUBBER_LINE: f32 = 3.0;

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
            // The playhead is the line alone. A halo shimmered and a wider foot
            // over the speaker band read as a block dropped into it; a solid
            // 2 px line keeps one colour and one width from top to bottom.
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

/// The collapsed player's single-line scrubber.
///
/// A canvas of its own rather than the [`Timeline`] in a shorter box: that one
/// draws mirrored bars, a speaker band and a zoom-aware window, none of which
/// mean anything three pixels tall. This keeps the timeline's vocabulary — a
/// recessed track, a filled played portion and an ink playhead — and its seek
/// mapping, minus the zoom.
struct Scrubber {
    /// Playhead as a fraction of the duration, 0..1.
    progress: f32,
    duration: f32,
    track: Color,
    played: Color,
    playhead: Color,
    /// False when playback is locked or nothing is loaded: the bar still shows
    /// where the playhead is, but takes no presses and draws no head.
    enabled: bool,
}

#[derive(Default)]
struct ScrubberState {
    scrubbing: bool,
}

/// Time under `x` in a full-width scrubber: the mapping `Timeline::time_at`
/// applies at zoom 1, without the visible window.
fn scrub_time_at(x: f32, width: f32, duration: f32) -> f32 {
    if width <= 0.0 || duration <= 0.0 {
        return 0.0;
    }
    (x / width).clamp(0.0, 1.0) * duration
}

impl canvas::Program<Message> for Scrubber {
    type State = ScrubberState;

    fn update(
        &self,
        state: &mut ScrubberState,
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
                        Some(Message::Seek(scrub_time_at(
                            position.x,
                            bounds.width,
                            self.duration,
                        ))),
                    );
                }
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.scrubbing = false;
            }
            // Only while a press started on the bar is held. Like the timeline,
            // a cursor outside the canvas publishes nothing, so the seek follows
            // the pointer only while it is over the bar.
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.scrubbing => {
                if let Some(position) = cursor.position_in(bounds) {
                    return (
                        Status::Captured,
                        Some(Message::Seek(scrub_time_at(
                            position.x,
                            bounds.width,
                            self.duration,
                        ))),
                    );
                }
            }
            _ => {}
        }
        (Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &ScrubberState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let width = bounds.width;
        let height = bounds.height;
        let line_y = (height - SCRUBBER_LINE) / 2.0;
        let radius = (SCRUBBER_LINE / 2.0).into();

        frame.fill(
            &canvas::Path::rounded_rectangle(
                Point::new(0.0, line_y),
                Size::new(width, SCRUBBER_LINE),
                radius,
            ),
            self.track,
        );

        if self.enabled {
            let x = (width * self.progress.clamp(0.0, 1.0)).clamp(0.0, width);
            if x > 0.0 {
                frame.fill(
                    &canvas::Path::rounded_rectangle(
                        Point::new(0.0, line_y),
                        Size::new(x, SCRUBBER_LINE),
                        radius,
                    ),
                    self.played,
                );
            }
            // Full canvas height, so the head crosses the card's own surface
            // where the ink contrasts, not just the played band.
            frame.fill_rectangle(
                Point::new((x - 1.0).max(0.0), 0.0),
                Size::new(2.0, height),
                self.playhead,
            );
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &ScrubberState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if self.enabled && cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }
}

/// An utterance the ASR was unsure about: mean token log-probability below the
/// threshold.
fn is_low_confidence(line: &DocLine) -> bool {
    line.confidence
        .is_some_and(|confidence| confidence < LOW_CONFIDENCE)
}

/// Attach per-line confidences from the transcript's JSON sidecar. A count
/// mismatch means the transcript was edited since the sidecar was written, so
/// the values are dropped rather than misattributed.
fn attach_confidences(mut lines: Vec<DocLine>, confidences: &[Option<f32>]) -> Vec<DocLine> {
    if confidences.len() == lines.len() {
        for (line, confidence) in lines.iter_mut().zip(confidences) {
            line.confidence = *confidence;
        }
    }
    lines
}

/// Order speaker names the way the rename dialog shows them: by the number in
/// a `SPEAKER n` label, then alphabetically for anything renamed by hand.
fn sort_speaker_names(names: &mut [String]) {
    names.sort_by(|a, b| speaker_sort_key(a).cmp(&speaker_sort_key(b)));
}

fn speaker_sort_key(name: &str) -> (u32, String) {
    let named = name.to_ascii_lowercase();
    // Numbered labels sort before hand-renamed ones, which fall back to A-Z.
    match trailing_number(name) {
        Some(index) => (index, named),
        None => (u32::MAX, named),
    }
}

/// The number a name ends in, if any: `SPEAKER 2` ends in 2, `SPEAKER two` and
/// `2 speakers` do not.
fn trailing_number(name: &str) -> Option<u32> {
    name.rsplit(|character: char| !character.is_ascii_digit())
        .next()
        .filter(|digits| !digits.is_empty())
        .and_then(|digits| digits.parse::<u32>().ok())
}

/// The next speaker label to hand out for a transcript that already has
/// `names`: one past the highest number any name ends in, so gaps are not
/// reused (`SPEAKER 1`, `SPEAKER 3` gives `SPEAKER 4`), and one past the count
/// when no name is numbered (`Alice`, `Bob` gives `SPEAKER 3`).
fn next_speaker_name(names: &[String]) -> String {
    let next = match names.iter().filter_map(|name| trailing_number(name)).max() {
        Some(highest) => highest.saturating_add(1),
        None => (names.len() as u32).saturating_add(1),
    };
    format!("SPEAKER {next}")
}

fn speaker_palette(index: usize, scheme: &Scheme) -> Color {
    scheme.speaker_colors[index % scheme.speaker_colors.len()]
}

/// A decoder wrapped in pitch-preserving WSOLA time stretch. The sink always
/// plays at 1x; speed changes only alter the stretcher's tempo.
struct PitchSource {
    decoder: AudioStream,
    stretch: wsola::TimeStretch,
    speed: Arc<AtomicU32>,
    pending: VecDeque<f32>,
    buffer: Vec<f32>,
    channels: u16,
    sample_rate: u32,
    draining: bool,
    done: bool,
}

impl PitchSource {
    fn new(path: &Path, speed: Arc<AtomicU32>) -> Option<Self> {
        // `AudioStream` selects the audio track itself, so the video packets of
        // an MP4 never reach the decoder.
        let decoder = AudioStream::open(path).ok()?;
        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        let stretch = wsola::TimeStretch::new(
            std::num::NonZeroU32::new(sample_rate)?,
            std::num::NonZeroU16::new(channels)?,
        )
        .ok()?;
        let buffer = vec![0.0; stretch.min_output_buff_size() * 4];
        Some(Self {
            decoder,
            stretch,
            speed,
            pending: VecDeque::new(),
            buffer,
            channels,
            sample_rate,
            draining: false,
            done: false,
        })
    }

    fn fill(&mut self) -> bool {
        let tempo = f32::from_bits(self.speed.load(Ordering::Relaxed));
        self.stretch.set_tempo(tempo);
        loop {
            if !self.draining {
                let block = 4096 * self.channels as usize;
                let mut chunk = Vec::with_capacity(block);
                while chunk.len() < block {
                    match self.decoder.next() {
                        Some(sample) => chunk.push(sample as f32 / 32768.0),
                        None => break,
                    }
                }
                let aligned = chunk.len() - chunk.len() % self.channels as usize;
                if aligned > 0 {
                    self.stretch.push(&chunk[..aligned]);
                }
                if chunk.len() < block {
                    self.draining = true;
                    self.stretch.set_draining();
                }
            }

            match self.stretch.pull(&mut self.buffer) {
                Ok(written) if written > 0 => {
                    self.pending.extend(self.buffer[..written].iter().copied());
                    return true;
                }
                Ok(_) => {
                    if self.draining {
                        self.done = true;
                        return false;
                    }
                }
                Err(wsola::Error::NotEnoughInput) => {
                    if self.draining {
                        self.done = true;
                        return false;
                    }
                }
                Err(_) => {
                    self.done = true;
                    return false;
                }
            }
        }
    }
}

impl Iterator for PitchSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if let Some(sample) = self.pending.pop_front() {
                return Some(sample);
            }
            if self.done || !self.fill() {
                return None;
            }
        }
    }
}

impl Source for PitchSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.decoder.duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.decoder
            .seek(pos.as_secs_f64())
            .map_err(|e| rodio::source::SeekError::Other(Box::new(std::io::Error::other(e))))?;
        self.stretch.reset();
        self.pending.clear();
        self.draining = false;
        self.done = false;
        Ok(())
    }
}

/// Audio playback through rodio; the `OutputStream` must stay alive.
struct Player {
    _stream: OutputStream,
    sink: Sink,
    path: PathBuf,
    speed: Arc<AtomicU32>,
}

impl Player {
    fn new(path: &Path, speed: f32) -> Option<Player> {
        let mut stream = OutputStreamBuilder::open_default_stream().ok()?;
        // Rodio logs on drop; the GUI already owns teardown and does not want
        // a stray line on stderr when the player is replaced or the app exits.
        stream.log_on_drop(false);
        let sink = Sink::connect_new(stream.mixer());
        let speed = Arc::new(AtomicU32::new(speed.max(0.1).to_bits()));
        let source = PitchSource::new(path, speed.clone())?;
        sink.append(source);
        sink.pause();
        Some(Player {
            _stream: stream,
            sink,
            path: path.to_path_buf(),
            speed,
        })
    }

    /// Re-append the source after it has been fully consumed, so the sink is
    /// seekable again instead of staying stuck at the end.
    fn rebuild_if_empty(&self) {
        if self.sink.empty() {
            if let Some(source) = PitchSource::new(&self.path, self.speed.clone()) {
                self.sink.append(source);
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

    /// Seek to an audio-timeline position. The sink itself stays at 1x; WSOLA
    /// changes duration without changing pitch.
    fn seek(&self, seconds: f32) {
        self.rebuild_if_empty();
        let _ = self
            .sink
            .try_seek(Duration::from_secs_f32(seconds.max(0.0)));
    }

    fn set_speed(&self, factor: f32) {
        self.speed
            .store(factor.max(0.1).to_bits(), Ordering::Relaxed);
    }
}

/// Square brand mark used in the header and window chrome.
fn header_logo_handle() -> iced::widget::image::Handle {
    static LOGO: OnceLock<iced::widget::image::Handle> = OnceLock::new();
    LOGO.get_or_init(|| {
        let image =
            image::load_from_memory(include_bytes!("../../icons/simple-hippo-square-icon.png"))
                .expect("bundled header icon")
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

fn feature_eyebrow(
    label: &'static str,
    accent: Color,
    scheme: Scheme,
) -> Element<'static, Message> {
    container(
        text(label)
            .size(10)
            .wrapping(NO_WRAP)
            .font(Font {
                weight: font::Weight::Bold,
                ..Font::DEFAULT
            })
            .color(scheme.text),
    )
    .padding([3, 8])
    .style(move |_theme| container::Style {
        background: Some(Color { a: 0.14, ..accent }.into()),
        border: iced::border::Border {
            color: Color { a: 0.38, ..accent },
            width: 1.0,
            radius: PILL.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// A thin bar whose width is `portion / total` of the available row.
fn feature_bar(portion: u16, total: u16, color: Color, height: f32) -> Element<'static, Message> {
    let portion = portion.clamp(1, total.saturating_sub(1).max(1));
    let rest = total.saturating_sub(portion).max(1);
    row![
        container(horizontal_space())
            .width(Length::FillPortion(portion))
            .height(Length::Fixed(height))
            .style(move |_theme| container::Style {
                background: Some(color.into()),
                border: iced::border::rounded(height / 2.0),
                ..container::Style::default()
            }),
        horizontal_space().width(Length::FillPortion(rest)),
    ]
    .width(Length::Fill)
    .into()
}

fn feature_speaker_chip(
    label: &'static str,
    color: Color,
    scheme: Scheme,
) -> Element<'static, Message> {
    container(
        row![
            container(horizontal_space())
                .width(Length::Fixed(6.0))
                .height(Length::Fixed(6.0))
                .style(move |_theme| container::Style {
                    background: Some(color.into()),
                    border: iced::border::rounded(3.0),
                    ..container::Style::default()
                }),
            text(label).size(10).wrapping(NO_WRAP).color(scheme.text),
            icon_colored(ICON_CARET_DOWN, 8.0, scheme.muted),
        ]
        .spacing(theme::SPACE_XS)
        .align_y(iced::Alignment::Center),
    )
    .padding([2, 7])
    .style(move |_theme| container::Style {
        background: Some(Color { a: 0.12, ..color }.into()),
        border: iced::border::Border {
            color: Color { a: 0.34, ..color },
            width: 1.0,
            radius: PILL.into(),
        },
        ..container::Style::default()
    })
    .into()
}

fn feature_choice(
    label: &'static str,
    active: bool,
    accent: Color,
    scheme: Scheme,
) -> Element<'static, Message> {
    container(text(label).size(10).wrapping(NO_WRAP).color(if active {
        scheme.text
    } else {
        scheme.muted
    }))
    .padding([3, 10])
    .style(move |_theme| container::Style {
        background: Some(if active {
            Color { a: 0.15, ..accent }.into()
        } else {
            Color::TRANSPARENT.into()
        }),
        border: iced::border::Border {
            color: if active {
                Color { a: 0.40, ..accent }
            } else {
                scheme.border
            },
            width: 1.0,
            radius: PILL.into(),
        },
        ..container::Style::default()
    })
    .into()
}

fn feature_icon_chip(
    glyph: char,
    label: &'static str,
    scheme: Scheme,
) -> Element<'static, Message> {
    container(
        row![
            icon_colored(glyph, 10.0, scheme.muted),
            text(label).size(10).wrapping(NO_WRAP).color(scheme.text),
        ]
        .spacing(theme::SPACE_XS)
        .align_y(iced::Alignment::Center),
    )
    .padding([3, 8])
    .style(move |_theme| container::Style {
        background: Some(scheme.track.into()),
        border: iced::border::Border {
            color: scheme.border,
            width: 1.0,
            radius: PILL.into(),
        },
        ..container::Style::default()
    })
    .into()
}

fn feature_vignette_file(scheme: Scheme) -> Element<'static, Message> {
    let line = Color {
        a: 0.26,
        ..scheme.text
    };
    let header = row![
        container(icon_colored(ICON_FILE_TEXT, 14.0, scheme.blue))
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(26.0))
            .center_x(Length::Fixed(26.0))
            .center_y(Length::Fixed(26.0))
            .style(move |_theme| container::Style {
                background: Some(
                    Color {
                        a: 0.12,
                        ..scheme.blue
                    }
                    .into()
                ),
                border: iced::border::rounded(8.0),
                ..container::Style::default()
            }),
        text("Audio file")
            .size(11)
            .wrapping(NO_WRAP)
            .color(scheme.text),
        horizontal_space(),
        text("1 speaker")
            .size(10)
            .wrapping(NO_WRAP)
            .color(scheme.muted),
    ]
    .spacing(theme::SPACE_MD)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    let mut lines = column![].spacing(theme::SPACE_LG).width(Length::Fill);
    for portion in [10_u16, 11, 8, 11, 9, 7] {
        lines = lines.push(feature_bar(portion, 12, line, 5.0));
    }

    column![header, lines].spacing(theme::SPACE_LG).into()
}

fn feature_vignette_speakers(scheme: Scheme) -> Element<'static, Message> {
    let line = Color {
        a: 0.26,
        ..scheme.text
    };
    let speakers = [
        ("Speaker 1", scheme.speaker_colors[0], 10_u16, 7_u16),
        ("Speaker 2", scheme.speaker_colors[1], 11, 6),
        ("Speaker 3", scheme.speaker_colors[3], 8, 10),
    ];
    let mut groups = column![].spacing(theme::SPACE_LG).width(Length::Fill);
    for (label, color, first, second) in speakers {
        groups = groups.push(
            column![
                row![
                    feature_speaker_chip(label, color, scheme),
                    horizontal_space()
                ]
                .width(Length::Fill)
                .align_y(iced::Alignment::Center),
                feature_bar(first, 12, line, 4.0),
                feature_bar(second, 12, line, 4.0),
            ]
            .spacing(theme::SPACE_XS)
            .width(Length::Fill),
        );
    }
    groups.into()
}

fn feature_queue_row(
    badge: Element<'static, Message>,
    status: &'static str,
    scheme: Scheme,
) -> Element<'static, Message> {
    let line = Color {
        a: 0.24,
        ..scheme.text
    };
    row![
        badge,
        container(feature_bar(10, 12, line, 5.0)).width(Length::Fill),
        text(status).size(10).wrapping(NO_WRAP).color(scheme.muted),
    ]
    .spacing(theme::SPACE_MD)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill)
    .into()
}

fn feature_vignette_queue(scheme: Scheme) -> Element<'static, Message> {
    let header = row![
        text("Queue")
            .size(10)
            .wrapping(NO_WRAP)
            .font(Font {
                weight: font::Weight::Bold,
                ..Font::DEFAULT
            })
            .color(scheme.muted),
        horizontal_space(),
        icon_colored(ICON_FOLDER_PLUS, 11.0, scheme.muted),
        text("Folder").size(9).wrapping(NO_WRAP).color(scheme.muted),
    ]
    .spacing(theme::SPACE_XS)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    let rows = column![
        feature_queue_row(
            queue_status_badge(ICON_FILE_TEXT, 15.0, scheme.track, scheme.blue),
            "Done",
            scheme,
        ),
        feature_queue_row(
            queue_status_badge(ICON_HOURGLASS, 13.0, scheme.track, scheme.text),
            "Running",
            scheme,
        ),
        feature_queue_row(
            queue_status_icon(ICON_CIRCLE, 15.0, scheme.muted),
            "Queued",
            scheme,
        ),
    ]
    .spacing(theme::SPACE_MD)
    .width(Length::Fill);

    column![header, rows].spacing(theme::SPACE_MD).into()
}

fn feature_vignette_review(scheme: Scheme) -> Element<'static, Message> {
    let wave = canvas(DeckWaveform {
        track: scheme.track,
        wave: scheme.green,
        playhead: scheme.blue,
    })
    .width(Length::Fill)
    .height(Length::Fixed(52.0));

    let line = Color {
        a: 0.38,
        ..scheme.text
    };
    let highlight = container(
        row![
            icon_colored(ICON_PLAY, 9.0, scheme.blue),
            container(feature_bar(11, 12, line, 5.0)).width(Length::Fill),
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fixed(24.0))
    .padding([0, 8])
    .align_y(iced::Alignment::Center)
    .style(move |_theme| container::Style {
        background: Some(
            Color {
                a: 0.10,
                ..scheme.blue
            }
            .into(),
        ),
        border: iced::border::Border {
            color: Color {
                a: 0.32,
                ..scheme.blue
            },
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    });

    let chips = row![
        feature_choice("Speed", true, scheme.blue, scheme),
        feature_choice("Accuracy", false, scheme.blue, scheme),
        horizontal_space(),
    ]
    .spacing(theme::SPACE_SM)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    column![wave, highlight, chips]
        .spacing(theme::SPACE_MD)
        .width(Length::Fill)
        .into()
}

fn feature_vignette_local(scheme: Scheme) -> Element<'static, Message> {
    let mark = container(
        container(
            iced::widget::image(header_logo_handle())
                .width(Length::Fixed(66.0))
                .height(Length::Fixed(66.0)),
        )
        .width(Length::Fixed(96.0))
        .height(Length::Fixed(96.0))
        .center_x(Length::Fixed(96.0))
        .center_y(Length::Fixed(96.0))
        .style(move |_theme| container::Style {
            background: Some(
                Color {
                    a: 0.10,
                    ..scheme.blue
                }
                .into(),
            ),
            border: iced::border::Border {
                color: Color {
                    a: 0.35,
                    ..scheme.blue
                },
                width: 1.0,
                radius: 48.0.into(),
            },
            ..container::Style::default()
        }),
    )
    .width(Length::Fixed(114.0))
    .height(Length::Fixed(114.0))
    .center_x(Length::Fixed(114.0))
    .center_y(Length::Fixed(114.0))
    .style(move |_theme| container::Style {
        border: iced::border::Border {
            color: Color {
                a: 0.18,
                ..scheme.blue
            },
            width: 1.0,
            radius: 57.0.into(),
        },
        ..container::Style::default()
    });

    let chips = row![
        feature_icon_chip(ICON_MIC, "Audio", scheme),
        feature_icon_chip(ICON_FILE_TEXT, "Transcripts", scheme),
    ]
    .spacing(theme::SPACE_SM)
    .align_y(iced::Alignment::Center);

    container(
        column![mark, chips]
            .spacing(theme::SPACE_SM)
            .align_x(iced::Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .into()
}

/// The run progress bar: a rounded track with a rounded fill whose corner
/// radius follows its own width, so a barely-started bar is still a pill.
struct ProgressTrack {
    fraction: f32,
    track: Color,
    edge: Color,
    fill: Color,
}

impl canvas::Program<Message> for ProgressTrack {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let width = bounds.width;
        let height = bounds.height;
        let radius = height / 2.0;
        let track = canvas::Path::rounded_rectangle(
            Point::ORIGIN,
            Size::new(width, height),
            radius.into(),
        );
        frame.fill(&track, self.track);
        frame.stroke(
            &track,
            canvas::Stroke::default()
                .with_color(self.edge)
                .with_width(1.0),
        );

        // Inset by the border, then clamp the radius to half of whichever
        // dimension is smaller: that is what keeps a short fill round.
        let inset = 1.0f32;
        let fill_width = (width * self.fraction - inset).clamp(0.0, width - inset * 2.0);
        let fill_height = (height - inset * 2.0).max(0.0);
        if fill_width >= 0.5 && fill_height > 0.5 {
            let corner = fill_width.min(fill_height) / 2.0;
            let path = canvas::Path::rounded_rectangle(
                Point::new(inset, inset),
                Size::new(fill_width, fill_height),
                corner.into(),
            );
            frame.fill(&path, self.fill);
        }
        vec![frame.into_geometry()]
    }
}

/// The live microphone meter in the recording strip.
///
/// A row of segments, not a `progress_bar`: recording has no completion
/// fraction, and segments read as a level at a glance. The fill follows the
/// recorder's smoothed envelope, the thin tick holds the recent peak, and the
/// fill lightens while the input is clipping.
struct RecordingMeter {
    level: f32,
    peak: f32,
    track: Color,
    fill: Color,
    hold: Color,
}

impl canvas::Program<Message> for RecordingMeter {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let width = bounds.width;
        let height = bounds.height;
        if width <= 0.0 || height <= 0.0 {
            return vec![frame.into_geometry()];
        }

        // Five-pixel segments with two-pixel gaps. The count follows the
        // available width, so the meter keeps its rhythm in a narrow sidebar.
        let segment = 5.0f32;
        let gap = 2.0f32;
        let count = (((width + gap) / (segment + gap)).floor() as usize).max(1);
        let spanned = count as f32 * segment + (count as f32 - 1.0) * gap;
        let start = ((width - spanned) / 2.0).max(0.0);
        let lit = ((self.level.clamp(0.0, 1.0) * count as f32).ceil() as usize).min(count);
        let radius = (segment.min(height) / 2.0).min(2.5);
        for index in 0..count {
            let color = if index < lit { self.fill } else { self.track };
            let x = start + index as f32 * (segment + gap);
            let path = canvas::Path::rounded_rectangle(
                Point::new(x, 0.0),
                Size::new(segment, height),
                radius.into(),
            );
            frame.fill(&path, color);
        }

        // The hold tick only needs drawing when it is ahead of the fill; while
        // the level is still rising the leading edge already marks the peak.
        // It is positioned on the drawn span, not the widget width, so it lines
        // up with the segment the peak belongs to.
        if self.peak > self.level + 0.02 && self.peak > 0.02 && spanned >= 2.0 {
            let x = (start + self.peak.clamp(0.0, 1.0) * spanned - 1.0)
                .clamp(start, start + spanned - 2.0);
            let path = canvas::Path::rounded_rectangle(
                Point::new(x, 0.0),
                Size::new(2.0, height),
                1.0.into(),
            );
            frame.fill(&path, self.hold);
        }

        vec![frame.into_geometry()]
    }
}

/// Abstract waveform used by the review slide: deterministic, no audio needed.
struct DeckWaveform {
    track: Color,
    wave: Color,
    playhead: Color,
}

impl canvas::Program<Message> for DeckWaveform {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let width = bounds.width;
        let height = bounds.height;
        let background =
            canvas::Path::rounded_rectangle(Point::ORIGIN, Size::new(width, height), 8.0.into());
        frame.fill(&background, self.track);

        let count = 48usize;
        let gap = 2.0;
        let bar = ((width - gap * (count as f32 - 1.0)) / count as f32).max(1.0);
        let mid = height / 2.0;
        for index in 0..count {
            let t = index as f32 / count as f32;
            let envelope = (t * std::f32::consts::PI).sin().max(0.16);
            let texture =
                0.35 + 0.65 * ((t * 41.0).sin().abs() * 0.6 + (t * 97.0).cos().abs() * 0.4);
            let amplitude = (height * 0.36 * envelope * texture).clamp(1.5, (mid * 0.88).max(1.5));
            let x = index as f32 * (bar + gap);
            frame.fill_rectangle(
                Point::new(x, mid - amplitude),
                Size::new(bar, amplitude * 2.0),
                Color {
                    a: 0.82,
                    ..self.wave
                },
            );
        }

        // No halo and no cap: the preview is static, so a translucent band or a
        // stub at the top only reads as a smudge and a stray horizontal line.
        let x = (width * 0.42).clamp(0.0, (width - 2.0).max(0.0));
        frame.fill_rectangle(Point::new(x, 0.0), Size::new(2.0, height), self.playhead);

        vec![frame.into_geometry()]
    }
}

/// The caret every pick list draws in its handle: a downward caret when the
/// list is closed, an upward one when it is open.
///
/// The flip is a state cue, not a direction: Material and Fluent/WinUI both
/// rotate the chevron 180 degrees on expand, and neither points it at the side
/// the list appears on — Fluent's own combo boxes open above the control when it
/// sits near the bottom of the screen and still flip the chevron up. iced makes
/// the same point unavoidable here: it chooses whether the menu opens above or
/// below (`overlay/menu.rs:246-255`) and never reports it, and `Status::Opened`
/// is the same in both cases, so a caret that flips can only express state.
fn caret_handle() -> pick_list::Handle<Font> {
    let icon = |code: char| pick_list::Icon {
        font: Font::with_name("bootstrap-icons"),
        code_point: code,
        size: Some(Pixels(12.0)),
        line_height: iced::advanced::text::LineHeight::default(),
        shaping: iced::advanced::text::Shaping::Basic,
    };
    pick_list::Handle::Dynamic {
        closed: icon(ICON_CARET_DOWN),
        open: icon(ICON_CARET_UP),
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
/// Two arrows making a half square: the speakers exchange places. The
/// left-right pair it replaced read as up-and-down movement at this size,
/// because its two arrows sit one above the other; this one bends them round
/// each other so the exchange is the shape.
const ICON_SWAP: char = '\u{f813}';
/// Join this line with the one above / below. An arrow into an edge, so the bar
/// is the neighbouring line it merges with: plain arrows read as moving the row,
/// which is not what the button does.
const ICON_JOIN_PREV: char = '\u{f115}';
const ICON_JOIN_NEXT: char = '\u{f112}';
/// A path forking: cut this line at the caret. Scissors were the obvious
/// choice but collapse into an X at the panel's glyph size, next to a trash can.
const ICON_SPLIT: char = '\u{f549}';
const ICON_UNDO: char = '\u{f117}';
/// Clockwise arrow: redo. Deliberately not shared with `ICON_RETRY` — one glyph
/// for two verbs is the sort of collision that makes an icon row unreadable.
const ICON_REDO: char = '\u{f116}';
const ICON_CARET_UP: char = '\u{f235}';
const ICON_ZOOM_IN: char = '\u{f62c}';
const ICON_ZOOM_OUT: char = '\u{f62d}';
const ICON_CLOCK: char = '\u{f293}';
const ICON_MIC: char = '\u{f490}';
const ICON_FILE_TEXT: char = '\u{f38b}';
const ICON_DOWNLOAD: char = '\u{f30a}';
const ICON_PLUS: char = '\u{f4fe}';
const ICON_FOLDER_PLUS: char = '\u{f3d3}';
const ICON_FOLDER_OPEN: char = '\u{f3d8}';
const ICON_TRASH: char = '\u{f5de}';
/// A group of people: the speakers, not the line. The strip's other glyphs are
/// about this one line; this one opens the transcript-wide rename.
const ICON_SPEAKERS: char = '\u{f4d0}';
/// A person with a plus: a new voice on this line, which the transcript-wide
/// `ICON_SPEAKERS` group cannot say.
const ICON_PERSON_PLUS: char = '\u{f4dd}';
/// A repeat arrow for retrying a failed item. Distinct from the editor's redo
/// glyph, which used to share this codepoint.
const ICON_RETRY: char = '\u{f130}';

/// Full-window drop target, kept in the widget tree at all times.
///
/// The wrapper is unconditional for the same reason as the toast layer:
/// swapping between a bare base and a stack shifts the index of every
/// descendant, which remounts the transcript scrollable and resets its offset
/// to 0. Only the overlay's contents vary, and an inert filler keeps the empty
/// case from touching layout or input.
/// True when a stored GPU error no longer describes the GPU.
///
/// Two failures are about the run, not the backend: a missing model, and a
/// child process that never started. Keeping either muting the preset hides the
/// Test GPU button, so the only way out was editing the settings file by hand.
fn stale_gpu_error(error: &str, models_ready: bool) -> bool {
    if !models_ready {
        return false;
    }
    let error = error.to_ascii_lowercase();
    error.contains("model not found") || error.contains("could not start the transcription process")
}

fn drop_overlay<'a>(
    base: impl Into<Element<'a, Message>>,
    scheme: Scheme,
    hovering: bool,
) -> Element<'a, Message> {
    let overlay: Element<'a, Message> = if hovering {
        drop_card(scheme)
    } else {
        horizontal_space().into()
    };

    stack![base.into(), overlay].into()
}

/// The card shown while a file is dragged over the window.
fn drop_card<'a>(scheme: Scheme) -> Element<'a, Message> {
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

    overlay.into()
}

/// Standard dialog surface shared by every modal.
/// Outline shared by every card ("island") in the review pane: the same hairline
/// the run card already draws. In the light theme the shadow separates cards;
/// on a dark window a shadow barely registers, so the outline does the work.
fn island_border(scheme: &Scheme) -> iced::border::Border {
    if theme::is_dark_scheme(scheme) {
        iced::border::Border {
            color: scheme.border,
            width: 1.0,
            radius: 16.0.into(),
        }
    } else {
        iced::border::rounded(16.0)
    }
}

fn modal_card<'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    scheme: Scheme,
) -> Element<'a, Message> {
    container(content)
        .width(Length::Fixed(width))
        .padding(MODAL_PADDING)
        .style(move |_theme| container::Style {
            background: Some(scheme.dialog_bg.into()),
            border: if theme::is_dark_scheme(&scheme) {
                iced::border::Border {
                    color: scheme.border,
                    width: 1.0,
                    radius: 16.0.into(),
                }
            } else {
                iced::border::rounded(16.0)
            },
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

/// A fixed-column table for the cross-run summary: bold muted header, one
/// right-aligned row per file, then a separated total row.
fn summary_table(
    headers: &[String],
    rows: &[Vec<String>],
    scheme: Scheme,
) -> Element<'static, Message> {
    // The name column absorbs the remaining width; every numeric column has a
    // fixed width so the figures line up without a monospace font.
    let column_width = |index: usize| -> Length {
        match index {
            0 => Length::Fill,
            3 => Length::Fixed(44.0),
            4 => Length::Fixed(30.0),
            5 => Length::Fixed(42.0),
            _ => Length::Fixed(56.0),
        }
    };
    let mut table = column![].spacing(3);
    let header = headers
        .iter()
        .enumerate()
        .fold(row![].spacing(6), |line, (index, cell)| {
            let label = text(cell.clone())
                .size(10)
                .wrapping(NO_WRAP)
                .width(column_width(index))
                .align_x(if index == 0 {
                    iced::Alignment::Start
                } else {
                    iced::Alignment::End
                })
                .font(Font {
                    weight: font::Weight::Bold,
                    ..Font::DEFAULT
                })
                .color(scheme.muted);
            line.push(label)
        });
    table = table.push(header);
    let total_index = rows.len().saturating_sub(1);
    for (index, cells) in rows.iter().enumerate() {
        if index == total_index && rows.len() > 1 {
            table = table.push(settings_divider(scheme));
        }
        let is_total = index == total_index && rows.len() > 1;
        let line = cells
            .iter()
            .enumerate()
            .fold(row![].spacing(6), |line, (column, cell)| {
                let width = column_width(column);
                let mut label = text(cell.clone())
                    .size(theme::TEXT_SM)
                    .wrapping(NO_WRAP)
                    .width(width)
                    .align_x(if column == 0 {
                        iced::Alignment::Start
                    } else {
                        iced::Alignment::End
                    })
                    .color(if is_total { scheme.text } else { scheme.muted });
                if is_total {
                    label = label.font(Font {
                        weight: font::Weight::Bold,
                        ..Font::DEFAULT
                    });
                }
                line.push(label)
            });
        table = table.push(line);
    }
    table.into()
}

/// Layer `dialog` over `base` behind a dimming backdrop that swallows pointer
/// input, so a visible modal blocks everything outside it. The backdrop covers
/// the whole window and takes the cursor for itself, so the widgets underneath
/// never see a hover or a click.
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
    // `Idle` keeps the arrow cursor over the scrim instead of inheriting the
    // text or pointer cursor of whatever sits underneath.
    .interaction(mouse::Interaction::Idle)
    .on_press(Message::Noop)
    .on_right_press(Message::Noop)
    .on_middle_press(Message::Noop)
    // The stack already withholds the wheel from layers under an area that
    // reports an interaction, but capturing it here says so outright: a
    // scrollable behind the scrim must never move.
    .on_scroll(|_| Message::Noop);
    // The dialog itself also consumes stray clicks and keeps an arrow cursor
    // on its non-interactive areas, so nothing behind it is clickable.
    let dialog = mouse_area(dialog)
        .interaction(mouse::Interaction::Idle)
        .on_press(Message::Noop)
        .on_right_press(Message::Noop)
        .on_middle_press(Message::Noop)
        .on_scroll(|_| Message::Noop);
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

/// Coarse "time left" text: the throughput is only good to about this
/// resolution, so finer digits would be false precision.
fn eta_bucket(left: f32) -> String {
    if left >= 600.0 {
        let minutes = ((left / 60.0) / 5.0).round().max(1.0) * 5.0;
        format!("{minutes:.0} min")
    } else if left >= 60.0 {
        let seconds = ((left / 5.0).round() * 5.0).max(5.0);
        format_secs(seconds as f64)
    } else {
        let seconds = ((left / 5.0).round() * 5.0).max(5.0);
        format!("{seconds:.0} s")
    }
}

/// Median of an already-sorted slice.
fn median(sorted: &[f32]) -> f32 {
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) * 0.5
    } else {
        sorted[mid]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastTone {
    Success,
    Info,
    Danger,
}

fn toast_tone(message: &str) -> ToastTone {
    let message = message.to_ascii_lowercase();
    if [
        "fail",
        "error",
        "could not",
        "cannot",
        "missing",
        "unavailable",
        "disabled",
        "read-only",
        "too short",
        "no audio",
        "no output",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        ToastTone::Danger
    } else if [
        "pro",
        "download",
        "copied",
        "imported",
        "queued",
        "stopping",
        "purchase",
        "trial",
        "restored",
        "switched",
        "player is",
        "editing is",
        "reviewing",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        ToastTone::Info
    } else {
        ToastTone::Success
    }
}

fn toast_ttl(message: &str) -> Duration {
    match toast_tone(message) {
        ToastTone::Danger => Duration::from_secs(6),
        ToastTone::Info => Duration::from_secs(3),
        ToastTone::Success => Duration::from_millis(2400),
    }
}

/// Label to render verbatim while decoding, or `None` for the plain
/// "Decoding" placeholder whose animated dots are drawn instead.
fn decoding_label(stage: &str, label: &str) -> Option<String> {
    let stage = stage.trim();
    let lower = stage.to_ascii_lowercase();
    if !lower.starts_with("decoding") {
        return None;
    }
    // A bare "Decoding" keeps the animation; "Decoding 42%" carries detail.
    if lower.len() == "decoding".len() {
        return None;
    }
    Some(label.to_string())
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

/// Always-visible scrollbar, for the queue and any dialog that scrolls.
///
/// A dialog's scroller is its only cue that more rows exist below the fold, and
/// a rail that only appears on hover reads as an empty gutter until then.
fn visible_scrollbar_style(theme: &iced::Theme, status: scrollable::Status) -> scrollable::Style {
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

/// Focuses the first focusable widget in the tree and unfocuses every other
/// one. The inline editor is the first focusable while it is open, and the only
/// way to focus a `text_editor` is through an operation like this one: it has
/// no id to target.
fn focus_first<Message: 'static>() -> impl iced::advanced::widget::Operation<Message> {
    use iced::advanced::widget::operation::Focusable;

    struct FocusFirst {
        current: usize,
    }

    impl<Message> iced::advanced::widget::Operation<Message> for FocusFirst {
        fn focusable(
            &mut self,
            state: &mut dyn Focusable,
            _id: Option<&iced::advanced::widget::Id>,
        ) {
            if self.current == 0 {
                state.focus();
            } else {
                state.unfocus();
            }
            self.current += 1;
        }

        fn container(
            &mut self,
            _id: Option<&iced::advanced::widget::Id>,
            _bounds: Rectangle,
            operate_on_children: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<Message>),
        ) {
            operate_on_children(self);
        }
    }

    FocusFirst { current: 0 }
}

/// The speaker and text a row would write: one transcript line must stay one
/// physical line, so newlines typed into the editor collapse into spaces.
fn line_edit_row_fields(speaker: &str, content: &text_editor::Content) -> (String, String) {
    (
        speaker.trim().to_string(),
        content.text().trim_end().replace(['\n', '\r'], " "),
    )
}

/// The two halves a split at the editor's caret would produce, or `None` when
/// the caret leaves one side empty. The panel greys the split button on the
/// same answer, so a live button cannot refuse what it promised.
fn split_at_caret(row: &LineEditRow) -> Option<(String, String)> {
    // The caret is reported against the raw editor text; normalising the
    // newlines keeps the offset valid because each one becomes one space.
    let text = row.content.text();
    let cursor = cursor_byte_index(&text, row.content.cursor_position());
    let text = text.trim_end().replace(['\n', '\r'], " ");
    split_words_at(&text, cursor.min(text.len()))
        .map(|(left, right)| (left.to_string(), right.to_string()))
}

/// The accent colour of a speaker, resolved through the transcript's speaker
/// order so the line editor tints a line exactly as the transcript pane does.
fn line_accent(order: &[String], speaker: &str, scheme: &Scheme) -> Color {
    let position = order.iter().position(|name| name == speaker).unwrap_or(0);
    speaker_palette(position, scheme)
}

/// Put `index` back into a display order at the rank its start time gives it.
///
/// Used while a line is being edited: the speaker filter can hide the row the
/// moment its speaker changes, and an unmounted row would close the editor.
fn pin_display_index(display: &mut Vec<usize>, lines: &[DocLine], index: usize) {
    if display.contains(&index) || index >= lines.len() {
        return;
    }
    let time = |position: usize| {
        lines
            .get(position)
            .and_then(|line| line.start)
            .unwrap_or(f32::INFINITY)
    };
    let start = time(index);
    let rank = display
        .iter()
        .position(|other| time(*other) > start)
        .unwrap_or(display.len());
    display.insert(rank, index);
}

/// A fixed-height, inert block standing in for transcript rows that are not
/// rendered. The virtualised window keeps its scroll geometry with it.
fn row_spacer(height: f32) -> Element<'static, Message> {
    container(horizontal_space())
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .into()
}

/// Exchange button for a bubble row: swapping a line's speaker without opening
/// the editor. It sits inside the row, next to the bubble's inner edge, at the
/// same height as the control rows above it.
fn sms_swap_button(index: usize, scheme: &Scheme) -> Element<'static, Message> {
    let scheme = *scheme;
    action(icon_colored(ICON_SWAP, SWAP_ICON_SIZE, scheme.ink))
        .padding(theme::CONTROL_PADDING)
        .on_press(Message::FlipLineSpeaker(index))
        .style(pill_button_secondary)
        .into()
}

/// A transcript line with no text at all: a muted placeholder where the row
/// stands, so a blank line in a file is visible and clickable instead of
/// silently vanishing and leaving the spacer heights off by one row.
fn empty_line_element(
    idx: usize,
    accent: Color,
    scheme: &Scheme,
    size: f32,
    hovered: bool,
) -> Element<'static, Message> {
    let scheme = *scheme;
    let content = container(
        row![
            text("Empty line")
                .size(size)
                .color(Color {
                    a: if hovered { 0.9 } else { 0.7 },
                    ..scheme.muted
                }),
            horizontal_space(),
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center),
    )
    .width(Length::Fill)
    .padding([3, 6])
    .style(move |_theme| container::Style {
        background: Some(
            Color {
                a: 0.16,
                ..accent
            }
            .into(),
        ),
        border: iced::border::Border {
            color: Color {
                a: 0.45,
                ..accent
            },
            width: 1.0,
            radius: 6.0.into(),
        },
        ..container::Style::default()
    });
    mouse_area(
        mouse_area(content)
            .interaction(mouse::Interaction::Pointer)
            .on_press(Message::LineClicked(idx))
            .on_right_press(Message::FlipLineSpeaker(idx)),
    )
    .on_enter(Message::LineHover(Some(idx)))
    .on_exit(Message::LineUnhover(idx))
    .into()
}

/// The editor's own chrome is transparent: the surrounding field draws the
/// fill and border, while the text stays the transcript's ink at the
/// transcript's size.
fn line_editor_style(
    scheme: Scheme,
) -> impl Fn(&iced::Theme, text_editor::Status) -> text_editor::Style {
    move |_theme, _status| text_editor::Style {
        background: Color::TRANSPARENT.into(),
        border: iced::border::Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        icon: scheme.muted,
        placeholder: scheme.muted,
        value: scheme.ink,
        selection: Color {
            a: 0.35,
            ..scheme.blue
        },
    }
}

/// A multi-line vocabulary field: one entry per line, in a well like the
/// transcript's line editor so it reads as an input rather than a paragraph.
fn vocabulary_field<'a>(
    content: &'a text_editor::Content,
    placeholder: &'a str,
    on_action: impl Fn(text_editor::Action) -> Message + 'a,
    scheme: Scheme,
) -> Element<'a, Message> {
    let field = text_editor(content)
        .on_action(on_action)
        .placeholder(placeholder)
        .size(theme::TEXT_MD)
        .line_height(iced::advanced::text::LineHeight::Relative(LINE_HEIGHT_FACTOR))
        .wrapping(iced::widget::text::Wrapping::Word)
        .padding(6)
        .height(Length::Fixed(96.0))
        .style(vocabulary_editor_style(scheme));
    container(field)
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(scheme.field_bg.into()),
            border: iced::border::Border {
                color: scheme.border,
                width: 1.0,
                radius: 6.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// The vocabulary field's inner editor: the well is drawn by its container.
fn vocabulary_editor_style(
    scheme: Scheme,
) -> impl Fn(&iced::Theme, text_editor::Status) -> text_editor::Style {
    move |_theme, _status| text_editor::Style {
        background: Color::TRANSPARENT.into(),
        border: iced::border::Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        icon: scheme.muted,
        placeholder: scheme.muted,
        value: scheme.ink,
        selection: Color {
            a: 0.35,
            ..scheme.blue
        },
    }
}

/// The line being edited, drawn in the transcript as a field where the line
/// stands. Everything the row had — the timestamp and the speaker label — stays
/// in place at its usual size and colour; only the words become an editor, and
/// the row's fill turns into the theme's text-field well so the change of state
/// is visible without moving anything.
fn inline_line_edit_row<'a>(
    content: &'a text_editor::Content,
    stamp: &str,
    speaker: &str,
    accent: Color,
    scheme: Scheme,
    size: f32,
) -> Element<'a, Message> {
    let field = text_editor(content)
        .on_action(|action| Message::LineEditAction(0, action))
        .placeholder("Empty line")
        .size(size)
        .line_height(iced::advanced::text::LineHeight::Relative(LINE_HEIGHT_FACTOR))
        .wrapping(iced::widget::text::Wrapping::Word)
        .padding(0)
        .style(line_editor_style(scheme));

    // Same order, spacing and alignment as `line_element`, so the words do not
    // shift by a pixel when the row becomes a field.
    let mut content_row = row![]
        .spacing(6)
        .align_y(iced::Alignment::Start)
        .width(Length::Fill);
    if !stamp.is_empty() {
        content_row = content_row.push(
            text(stamp.to_string())
                .size(size * 0.78)
                .color(scheme.muted),
        );
    }
    if !speaker.is_empty() {
        // Clicking the label cycles the line through the transcript's speakers:
        // the chip is already the line's identity, so it is also the switch.
        content_row = content_row.push(
            mouse_area(speaker_chip(speaker, accent, scheme.ink, size))
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::CycleLineSpeaker),
        );
    }
    content_row = content_row.push(field);

    container(content_row)
        .width(Length::Fill)
        .padding([3, 6])
        .style(move |_theme| container::Style {
            background: Some(scheme.field_bg.into()),
            border: iced::border::Border {
                color: scheme.border,
                width: 1.0,
                radius: 6.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// The bubble view's editor: the same bubble, with its meta row untouched and
/// its text replaced by a well. It keeps the speaker's side of the pane and the
/// bubble's width; only the words are editable.
fn inline_bubble_edit<'a>(
    content: &'a text_editor::Content,
    stamp: &str,
    speaker: &str,
    accent: Color,
    scheme: Scheme,
    size: f32,
    wrap: f32,
) -> Element<'a, Message> {
    let mut body = column![].spacing(2);
    if !speaker.is_empty() || !stamp.is_empty() {
        let mut meta = row![].spacing(6).align_y(iced::Alignment::Center);
        if !speaker.is_empty() {
            meta = meta.push(
                mouse_area(speaker_chip(speaker, accent, scheme.ink, size * 0.72))
                    .interaction(mouse::Interaction::Pointer)
                    .on_press(Message::CycleLineSpeaker),
            );
        }
        if !stamp.is_empty() {
            meta = meta.push(
                text(stamp.to_string())
                    .size(size * 0.72)
                    .color(Color { a: 0.70, ..scheme.ink }),
            );
        }
        body = body.push(meta);
    }
    body = body.push(
        text_editor(content)
            .on_action(|action| Message::LineEditAction(0, action))
            .placeholder("Empty line")
            .size(size)
            .line_height(iced::advanced::text::LineHeight::Relative(LINE_HEIGHT_FACTOR))
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(wrap)
            .padding(0)
            .style(line_editor_style(scheme)),
    );
    container(body)
        .width(Length::Shrink)
        // The swap button's slot is reserved whether or not it is showing, so
        // hovering cannot narrow the bubble and rewrap its text: the button
        // appears inside space the row already accounts for, and the wrap is
        // the same in both states. Line mode reserves the same way.
        .max_width(Pixels(
            (size * 28.0).clamp(240.0, 620.0) - SWAP_RESERVED_WIDTH,
        ))
        .align_x(iced::Alignment::Start)
        .padding([6, 9])
        .style(move |_theme| container::Style {
            background: Some(scheme.field_bg.into()),
            border: iced::border::Border {
                color: scheme.border,
                width: 1.0,
                radius: 10.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// One chat bubble: the speaker chip, the timestamp and the text, in the
/// speaker's colour. Placement is the caller's job, so the bubble can sit next
/// to the row's own controls rather than being pinned to the pane edge.
#[allow(clippy::too_many_arguments)]
fn sms_bubble(
    content: &str,
    speaker: &str,
    stamp: &str,
    color: Color,
    ink: Color,
    tint: Option<Color>,
    unsure: bool,
    scheme: &Scheme,
    query: &str,
    size: f32,
    active: bool,
    index: usize,
    karaoke: Option<(&[Word], f32, bool)>,
) -> Element<'static, Message> {
    // A low-confidence bubble keeps its speaker hue but desaturates and takes a
    // muted outline, so it reads as tentative next to a confident neighbour.
    let background = tint.unwrap_or_else(|| {
        if unsure {
            theme::mix(color, scheme.panel_bg, 0.74)
        } else {
            color
        }
    });
    let ink = if unsure {
        theme::mix(ink, scheme.panel_bg, 0.42)
    } else {
        ink
    };
    let dark = theme::is_dark_scheme(scheme);
    let band = theme::mix(
        background,
        if dark { Color::BLACK } else { Color::WHITE },
        if dark { 0.40 } else { 0.45 },
    );
    let border = if active {
        iced::border::Border {
            color: ink,
            width: 2.0,
            radius: 10.0.into(),
        }
    } else if unsure {
        iced::border::Border {
            color: Color {
                a: 0.85,
                ..scheme.muted
            },
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
        // The label and the timestamp open the editor too: they are the only
        // target on a bubble with no words, and pressing the chip is a direct
        // request for that line.
        body = body.push(
            mouse_area(meta)
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::LineClicked(index)),
        );
    }
    // The words take the press themselves, exactly as they do in line mode.
    // iced's `Rich` text captures a left press over any glyph before the row's
    // own `mouse_area` ever sees it, so a bare `rich_text` here made every word
    // a dead target: the first click never reached `line_pressed`, and a double
    // click could not open the editor. `TextPress` handles the press first and
    // passes the point on, which also puts the caret under the pointer.
    let karaoke_words = match karaoke {
        Some((words, at, playing)) if query.is_empty() => {
            karaoke_spans(content, words, at, ink, band, size, playing)
        }
        _ => None,
    };
    let text_body = match karaoke_words {
        Some(spans) => rich_text(spans),
        None => rich_text(styled_spans(
            content,
            query,
            false,
            false,
            Some(ink),
            scheme.search_hl,
            size,
        )),
    };
    body = body.push(Element::new(TextPress::new(text_body, index)));
    container(body)
        .width(Length::Shrink)
        .max_width(Pixels((size * 28.0).clamp(240.0, 620.0)))
        .align_x(iced::Alignment::Start)
        .padding([6, 9])
        .style(move |_theme| container::Style {
            background: Some(background.into()),
            border,
            ..container::Style::default()
        })
        .into()
}

/// A transparent wrapper around a line's words that reports where a left press
/// landed, in the words' own coordinates.
///
/// The transcript row's seek target covers the whole row, so it cannot say
/// *where* it was hit. This sits above that target over the text alone, takes
/// the press itself, and passes everything else through to the wrapped widget.
/// Knowing the position is what lets a double-click open the inline editor with
/// the caret under the pointer instead of at the end of the line.
struct TextPress<'a> {
    content: Element<'a, Message>,
    index: usize,
}

impl<'a> TextPress<'a> {
    fn new(content: impl Into<Element<'a, Message>>, index: usize) -> Self {
        Self {
            content: content.into(),
            index,
        }
    }
}

impl<'a> iced::advanced::Widget<Message, Theme, Renderer> for TextPress<'a> {
    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<iced::advanced::widget::Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut iced::advanced::widget::Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &self,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> iced::advanced::layout::Node {
        self.content.as_widget().layout(tree, renderer, limits)
    }

    fn operate(
        &self,
        tree: &mut iced::advanced::widget::Tree,
        layout: iced::advanced::Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.content
            .as_widget()
            .operate(tree, layout, renderer, operation);
    }

    fn on_event(
        &mut self,
        tree: &mut iced::advanced::widget::Tree,
        event: iced::Event,
        layout: iced::advanced::Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut iced::advanced::Shell<'_, Message>,
        viewport: &Rectangle,
    ) -> iced::event::Status {
        if let iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            if let Some(point) = cursor.position_in(layout.bounds()) {
                shell.publish(Message::LineTextPressed(self.index, point));
                return iced::event::Status::Captured;
            }
        }

        self.content.as_widget_mut().on_event(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        )
    }

    fn mouse_interaction(
        &self,
        tree: &iced::advanced::widget::Tree,
        layout: iced::advanced::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &iced::advanced::widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &iced::advanced::renderer::Style,
        layout: iced::advanced::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut iced::advanced::widget::Tree,
        layout: iced::advanced::Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<iced::advanced::overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, translation)
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
    hovered: bool,
    mark_confidence: bool,
    karaoke: Option<(&[Word], f32, bool)>,
) -> Element<'static, Message> {
    // A low-confidence line keeps its speaker hue but desaturates and dims its
    // text, so the difference survives next to a confident line of the same
    // speaker rather than reading as a slightly different shade.
    let unsure = mark_confidence && is_low_confidence(line);
    let ink = if unsure {
        theme::mix(scheme.ink, scheme.panel_bg, 0.40)
    } else {
        scheme.ink
    };

    // The colour actually painted behind this row. The karaoke band is derived
    // from it so it reads on the accent, on a low-confidence dull, and on a
    // search tint alike.
    let background = search_tint.unwrap_or_else(|| {
        if unsure {
            theme::mix(accent, scheme.panel_bg, 0.74)
        } else {
            accent
        }
    });
    let dark = theme::is_dark_scheme(scheme);
    let band = theme::mix(
        background,
        if dark { Color::BLACK } else { Color::WHITE },
        if dark { 0.40 } else { 0.45 },
    );

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

    // A press on the words reports where inside them it landed, so opening the
    // editor on a double-click can put the caret under the pointer.
    let words = match karaoke {
        Some((words, at, playing)) if query.is_empty() => {
            karaoke_spans(&rest, words, at, ink, band, size, playing)
        }
        _ => None,
    };
    let body = match words {
        Some(spans) => rich_text(spans),
        None => rich_text(styled_spans(
            &rest,
            query,
            case_sensitive,
            whole_word,
            Some(ink),
            highlight,
            size,
        )),
    };
    content = content.push(Element::new(TextPress::new(body, idx)));
    content = content.push(horizontal_space());
    // Reserved right-edge slot so revealing the swap button never rewraps the text.
    content = content.push(horizontal_space().width(Length::Fixed(size * 1.4)));

    let border = if active {
        iced::border::Border {
            color: scheme.ink,
            width: 2.0,
            radius: 6.0.into(),
        }
    } else if unsure {
        // A distinct outline, not just a paler fill: the row reads as flagged
        // even when several dull lines neighbour each other.
        iced::border::Border {
            color: Color {
                a: 0.85,
                ..scheme.muted
            },
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

    // Swap the speaker of this line. It replaces the old pencil in the same
    // reserved slot, so revealing it never rewraps the line. The playhead line
    // shows it too: that line is the one being read, and requiring the pointer
    // to sit on it made the control look intermittent.
    let swap: Element<'static, Message> = if hovered || active {
        button(icon_colored(ICON_SWAP, size * 1.05, scheme.muted))
            .padding([0, 2])
            .style(|_theme, _status| button::Style {
                background: None,
                border: iced::border::Border::default(),
                ..button::Style::default()
            })
            .on_press(Message::FlipLineSpeaker(idx))
            .into()
    } else {
        horizontal_space().into()
    };
    let swap_overlay: Element<'static, Message> = container(swap)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding([0, 6])
        .align_x(iced::Alignment::End)
        .align_y(iced::Alignment::Center)
        .into();

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
        .on_right_press(Message::FlipLineSpeaker(idx)),
        swap_overlay,
    ]
    .into();
    // The row's own area takes every press the words and the swap button leave
    // alone. It is the ancestor of both, so a child that handles a press stops
    // this one; anything else is a seek.
    mouse_area(line)
        .interaction(mouse::Interaction::Pointer)
        .on_press(Message::LineClicked(idx))
        .on_enter(Message::LineHover(Some(idx)))
        .on_exit(Message::LineUnhover(idx))
        .into()
}

const PILL: f32 = 1000.0;

/// Glyph size of every compact control-row icon.
const ICON_SIZE: f32 = 14.0;

/// Ceiling for a believable audio-seconds-per-wall-second sample. See
/// `App::usable_throughput`.
const MAX_THROUGHPUT: f32 = 200.0;

/// Built-in phase rates, wall seconds per audio second, used until a run
/// measures its own.
///
/// Diarization takes roughly this share of the audio length on the reference
/// machine (210 s for an hour); the post-ASR tail is small beside it. These
/// replace the priors a skipped diarization pass used to drag to zero.
const DIARIZE_DEFAULT: f32 = 0.08;
const PREPARE_DEFAULT: f32 = 0.01;

/// The exchange glyph is the only one that has to read at a glance while the
/// pointer is already on the row, so it is drawn larger than the control icons.
const SWAP_ICON_SIZE: f32 = 20.0;

/// Width a bubble gives up for the speaker-swap button it may show on hover:
/// the glyph, the button's horizontal padding, and the row's gap. Reserved in
/// both states so the bubble never rewraps when the button appears.
const SWAP_RESERVED_WIDTH: f32 = SWAP_ICON_SIZE + 2.0 * theme::CONTROL_PADDING[1] as f32 + theme::SPACE_SM;

/// A button (content-sized / minimum width).
fn action<'a>(content: impl Into<Element<'a, Message>>) -> iced::widget::Button<'a, Message> {
    button(content)
}

/// A compact icon button, sized by `theme::CONTROL_PADDING` alone.
///
/// Every icon control row builds its buttons through this, so the Find row and
/// the line editor panel cannot drift apart: no `Length::Fixed` height, which
/// would set the box *inside* the padding and squeeze the glyph's line box.
fn icon_pill(code: char, colour: Color) -> iced::widget::Button<'static, Message> {
    action(icon_colored(code, ICON_SIZE, colour)).padding(theme::CONTROL_PADDING)
}

/// The line box of an icon glyph, tightened to its own em size.
///
/// This icon font is 300/300 em: ascent exactly one em, descent zero, and its
/// glyph inks are symmetric about the em box's centre (measured: the join
/// arrows and the cancel cross land on 7.00 px of a 14 px box, the trash and
/// the undo arrows within 0.75 px). A line box the size of the glyph therefore
/// centres the ink, which is what lets a circle centre it by layout.
fn icon_line_height(size: f32) -> iced::advanced::text::LineHeight {
    iced::advanced::text::LineHeight::Absolute(Pixels(size))
}

/// A circular icon button: a square box whose radius is half its side, with the
/// glyph centred by layout rather than by padding.
///
/// The line box comes from [`icon_line_height`], so the node the container
/// centres is the glyph itself rather than a taller box with the ink at its top.
fn icon_circle(code: char, colour: Color) -> iced::widget::Button<'static, Message> {
    let glyph = text(code.to_string())
        .font(Font::with_name("bootstrap-icons"))
        .size(ICON_ONLY_SIZE)
        .line_height(icon_line_height(ICON_ONLY_SIZE))
        .style(move |_theme| text::Style { color: Some(colour) });
    action(
        container(glyph)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill),
    )
    .width(Length::Fixed(PANEL_ICON_DIAMETER))
    .height(Length::Fixed(PANEL_ICON_DIAMETER))
    .padding(0)
}

/// Fixed width for a text button so its label can never wrap. The iced default
/// 16 px UI font averages close to 10 px per character for mixed-case Latin
/// text, and the button adds 10 px of padding on each side.
fn action_width(label: &str) -> f32 {
    (label.chars().count() as f32 * 10.0 + 28.0).max(44.0)
}

/// A button with a single-line text label.
///
/// The box is a fixed height with no vertical padding and the label is centred
/// inside it by layout, so the label's line box can never move the text up or
/// down: the size decides the box, not the padding. Every text button in the app
/// comes through here, which is what keeps one row's buttons the same height as
/// the next row's.
fn action_text<'a>(label: &'a str) -> iced::widget::Button<'a, Message> {
    action(
        container(text(label).size(BUTTON_LABEL_SIZE).wrapping(NO_WRAP))
            .height(Length::Fill)
            .center_y(Length::Fill),
    )
    .width(Length::Fixed(action_width(label)))
    .height(Length::Fixed(CONTROL_HEIGHT))
    .padding([0, 10])
}

/// An on/off button: the same box as [`action_text`], filled when on.
///
/// The three toggles in Settings were three copies of this, each choosing its
/// own type size; they now differ only by what they toggle.
fn toggle_button<'a>(label: &'a str, on: bool, message: Message) -> iced::widget::Button<'a, Message> {
    action(
        container(text(label).size(BUTTON_LABEL_SIZE))
            .height(Length::Fill)
            .center_y(Length::Fill),
    )
    .on_press(message)
    .style(move |theme, status| {
        if on {
            pill_button_active(theme, status)
        } else {
            pill_button_secondary(theme, status)
        }
    })
    .width(Length::Fixed(TOGGLE_WIDTH))
    .height(Length::Fixed(CONTROL_HEIGHT))
    .padding([0, 10])
}

/// Rough advance width of a run of text: the same 10 px per character
/// `action_width` assumes at iced's 16 px UI size, scaled for a smaller size. A
/// menu is built before any glyph is measured and its labels never wrap, so the
/// estimate is deliberately generous.
fn estimated_text_width(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.625
}

/// Character budgets for the header's two identity lines, from the window width.
///
/// The filename must not be cut until it comes `HEADER_PAD_X` from the card's
/// right edge, so the block's width is the card minus both paddings, the four
/// menu buttons and their gaps, the row gap, the badge and the gap after it. The
/// row's elastic space is not subtracted: it is what gives way first.
///
/// The result is in characters, not pixels. The view is built without a
/// renderer, so the width can only be divided by the same per-character estimate
/// the rest of the app relies on ([`estimated_text_width`]). That estimate is
/// generous — about a tenth wide on a wide system sans, more on a narrow one —
/// so the cut lands a few characters early instead of overflowing. A
/// pixel-exact cut would need a measured layout, which this does not have.
fn header_identity_budgets(window_width: f32) -> (usize, usize) {
    let menus = FILE_BUTTON_WIDTH
        + EDIT_BUTTON_WIDTH
        + VIEW_BUTTON_WIDTH
        + HELP_BUTTON_WIDTH
        + theme::SPACE_SM * 3.0;
    let card = window_width - OUTER_PADDING * 2.0 - LEFT_PANEL_WIDTH - PANEL_GAP;
    let available = card
        - HEADER_PAD_X * 2.0
        - menus
        - HEADER_GAP
        - HEADER_BADGE_WIDTH
        - HEADER_BADGE_GAP;
    let budget = |size: f32| {
        (available / estimated_text_width("x", size))
            .floor()
            .max(MIN_NAME_CHARS as f32) as usize
    };
    // 13 px filename, 11 px meta: the same width in two type sizes.
    (budget(13.0), budget(11.0))
}

/// One row of a transcript header menu. The label, trailing key, check state and
/// action are one value, so the card's body and the width it is sized to cannot
/// disagree — the duplication the old parallel `rows` arrays allowed.
#[derive(Debug, Clone)]
struct MenuRow<'a> {
    label: &'a str,
    key: Option<&'a str>,
    checked: bool,
    enabled: bool,
    action: Option<Message>,
}

/// A group of rows in a header menu, optionally followed by custom controls
/// (the View menu's zoom pair). Groups are separated by a hairline.
struct MenuSection<'a> {
    rows: Vec<MenuRow<'a>>,
    footer: Option<Element<'a, Message>>,
}

/// Width a header menu needs: the widest of its rows, each being the check slot
/// and its gap, the label, the minimum gap before a trailing key and the key,
/// plus the row's and the card's padding.
///
/// The rows are the same values the body is built from. Rows keep their `Fill`
/// width inside the card so every row — and its hover tint — is the menu's full
/// width and the trailing keys line up in one column.
fn header_menu_width<'r, 'a: 'r>(rows: impl IntoIterator<Item = &'r MenuRow<'a>>) -> f32 {
    let row_width = |label: &str, shortcut: Option<&str>| {
        theme::SPACE_MD * 2.0                       // card padding, both sides
            + theme::SPACE_SM * 2.0                 // row padding `[5, 6]`, both sides
            + HEADER_MENU_CHECK
            + theme::SPACE_SM                       // check slot to label
            + estimated_text_width(label, 16.0)
            + shortcut.map_or(0.0, |key| {
                theme::SPACE_2XL + estimated_text_width(key, theme::TEXT_XS)
            })
    };
    rows.into_iter()
        .map(|row| row_width(row.label, row.key))
        .fold(0.0, f32::max)
}

/// The card every transcript-header menu is drawn in: raised, hairline-outlined
/// and sized to `rows`. Clicks inside it, including its padding, are consumed so
/// only a press outside dismisses the menu.
/// The transcript card's menu layer: `body` hangs `offset` from the card's left
/// edge and `VIEW_MENU_TOP` from its top.
fn card_menu_layer<'a>(body: Element<'a, Message>, offset: f32) -> Element<'a, Message> {
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::Alignment::Start)
        .align_y(iced::Alignment::Start)
        .padding(iced::Padding {
            top: VIEW_MENU_TOP,
            right: 0.0,
            bottom: 0.0,
            left: offset,
        })
        .into()
}

/// The Editing Tips opt-out: a square that fills when set, with the tick centred
/// inside it.
///
/// The tick goes *inside* the bordered box, at the tightened icon line height,
/// so the paragraph it is measured from fits within the box. A line box taller
/// than the space offered leaves cosmic-text with no vertical room for the
/// baseline and it then yields no layout run at all, which measures and draws as
/// nothing.
/// A row of dialog buttons, pushed to the right edge.
fn right_aligned<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .width(Length::Fill)
        .align_x(iced::Alignment::End)
        .into()
}

fn tips_check<'a>(checked: bool, scheme: Scheme) -> Element<'a, Message> {
    const BOX: f32 = 14.0;
    const TICK: f32 = 12.0;

    let content: Element<'a, Message> = if checked {
        text(ICON_CHECK.to_string())
            .font(Font::with_name("bootstrap-icons"))
            .size(TICK)
            .line_height(icon_line_height(TICK))
            .wrapping(NO_WRAP)
            .style(move |_theme| text::Style {
                color: Some(scheme.blue),
            })
            .into()
    } else {
        horizontal_space().into()
    };

    container(content)
        .width(Length::Fixed(BOX))
        .height(Length::Fixed(BOX))
        .align_x(iced::Alignment::Center)
        .align_y(iced::Alignment::Center)
        .style(move |_theme| container::Style {
            background: checked.then(|| scheme.dialog_bg.into()),
            border: iced::border::Border {
                color: if checked { scheme.blue } else { scheme.border },
                width: 1.0,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// A hairline between two menu groups.
fn menu_divider(scheme: Scheme) -> Element<'static, Message> {
    container(
        container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_theme| container::Style {
                background: Some(scheme.border.into()),
                ..container::Style::default()
            }),
    )
    .padding([4, 0])
    .into()
}

/// The card every transcript-header menu is drawn in: raised, hairline-outlined
/// and sized to its rows. Clicks inside it, including its padding, are consumed
/// so only a press outside dismisses the menu.
fn menu_card<'a>(sections: Vec<MenuSection<'a>>, scheme: Scheme) -> Element<'a, Message> {
    let width = header_menu_width(sections.iter().flat_map(|section| section.rows.iter()));
    let mut body = column![].spacing(theme::SPACE_SM).width(Length::Fill);
    for (index, section) in sections.into_iter().enumerate() {
        if index > 0 {
            body = body.push(menu_divider(scheme));
        }
        let mut rows = column![].spacing(2).width(Length::Fill);
        for row in section.rows {
            rows = rows.push(header_menu_item(
                row.label,
                row.checked,
                row.enabled,
                row.action,
                row.key,
                scheme,
            ));
        }
        body = body.push(rows);
        if let Some(footer) = section.footer {
            body = body.push(menu_divider(scheme));
            body = body.push(footer);
        }
    }
    let card = container(body)
        .width(Length::Fixed(width))
        .padding(8)
        .style(move |_theme| container::Style {
            background: Some(scheme.dialog_bg.into()),
            border: iced::border::Border {
                color: scheme.border,
                width: 1.0,
                radius: theme::RADIUS_MD.into(),
            },
            shadow: soft_shadow(),
            ..container::Style::default()
        });
    mouse_area(card)
        .interaction(mouse::Interaction::Idle)
        .on_press(Message::Noop)
        .on_right_press(Message::Noop)
        .on_middle_press(Message::Noop)
        .into()
}

/// One row of a header menu: a leading check slot that keeps every label on the
/// same left edge, the label, then the key that toggles it at the trailing
/// edge. Disabled rows stay in place so the menu's height does not change with
/// the transcript's data.
fn header_menu_item<'a>(
    label: &'a str,
    checked: bool,
    enabled: bool,
    message: Option<Message>,
    shortcut: Option<&'a str>,
    scheme: Scheme,
) -> Element<'a, Message> {
    let check: Element<'static, Message> = if checked && enabled {
        icon(ICON_CHECK, theme::TEXT_XS)
    } else {
        horizontal_space().into()
    };
    let mut content = row![
        container(check)
            .width(Length::Fixed(HEADER_MENU_CHECK))
            .center_x(Length::Fixed(HEADER_MENU_CHECK)),
        // The label carries the same 16 px as the button that opens the menu,
        // which is also the app's body size: a menu is a reading surface, so it
        // does not drop to the caption size.
        text(label).wrapping(NO_WRAP),
    ]
    .spacing(theme::SPACE_SM)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);
    // The key that toggles this row, at the far right where a shortcut hint
    // belongs. A row without one keeps the same label edge and no trailing text.
    if let Some(key) = shortcut {
        content = content.push(horizontal_space());
        content = content.push(
            text(key)
                .size(theme::TEXT_XS)
                .wrapping(NO_WRAP)
                .color(scheme.muted),
        );
    }
    // Every row is pressable, disabled ones included: iced only reports the
    // `Hovered` status for a button that has a press handler, so a row without
    // one could never tint. The disabled row's handler does nothing; its style
    // is what keeps it from reading as actionable.
    action(content)
        .width(Length::Fill)
        .padding([5, 6])
        .style(header_menu_item_style(scheme, enabled))
        .on_press(message.unwrap_or(Message::Noop))
        .into()
}

/// A menu row: no chrome until hovered. A disabled row still tints when the
/// pointer is over it — the menu should read as one live surface — but with a
/// weaker wash, because nothing about it is on offer; its label stays muted and
/// its press does nothing.
fn header_menu_item_style(
    scheme: Scheme,
    enabled: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hot = matches!(
            status,
            button::Status::Hovered | button::Status::Pressed
        );
        let tint = if enabled {
            hover_band(&scheme)
        } else {
            Color {
                a: 0.45,
                ..hover_band(&scheme)
            }
        };
        button::Style {
            background: hot.then(|| tint.into()),
            text_color: if enabled { scheme.ink } else { scheme.muted },
            border: iced::border::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: theme::RADIUS_SM.into(),
            },
            shadow: Shadow::default(),
        }
    }
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

/// Label colour for neutral (unfilled) buttons.
///
/// Dark mode gets pure white so the label reads at full strength against the
/// dark panels; light mode keeps the theme's primary text, since white on a
/// near-white panel would vanish. Filled accent buttons do not use this — they
/// keep `on_accent`, which is chosen for contrast against their own fill.
fn button_label(theme: &iced::Theme) -> Color {
    if theme::is_dark(theme) {
        Color::WHITE
    } else {
        theme.palette().text
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
    // Disabled keeps iced's dimmed label; only the live states go white in
    // dark mode, so a dead button never looks live.
    let text_color = match status {
        button::Status::Disabled => base.text_color,
        _ => button_label(theme),
    };
    button::Style {
        background: Some(linear_gradient(top, bottom)),
        text_color,
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
        text_color: match status {
            // A disabled tinted pill keeps the label it had, so the state still
            // reads as unavailable rather than switching to full-strength white.
            button::Status::Disabled => theme.palette().text,
            _ => button_label(theme),
        },
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

/// Selected toggle: an opaque grey fill, so it reads as a pressed state rather
/// than a translucent tint over the transcript.
fn pill_button_toggled(theme: &iced::Theme, status: button::Status) -> button::Style {
    let base = button::secondary(theme, status);
    let color = style_background_color(&base).unwrap_or(theme.palette().background);
    // Drives the label and the fill tint alike: in dark mode the grey fill is
    // mixed toward white, so the pressed state stays visible.
    let text = button_label(theme);
    let (amount, border_alpha, shadow): (f32, f32, Shadow) = match status {
        button::Status::Hovered => (0.24, 0.55, soft_shadow()),
        button::Status::Pressed => (0.30, 0.65, Shadow::default()),
        button::Status::Disabled => (0.10, 0.25, Shadow::default()),
        button::Status::Active => (0.17, 0.45, soft_shadow()),
    };
    button::Style {
        background: Some(linear_gradient(
            theme::mix(color, text, (amount + 0.05).min(1.0)),
            theme::mix(color, text, amount),
        )),
        text_color: text,
        border: iced::border::Border {
            color: Color {
                a: border_alpha,
                ..text
            },
            width: 1.0,
            radius: PILL.into(),
        },
        shadow,
    }
}

/// One half of the CPU/GPU segmented toggle.
///
/// The selected half is a solid accent thumb with an `on_accent` label — the
/// same fill/label pairing as the filled primary buttons — plus a rim border
/// and [`soft_shadow`], so the current choice is unmistakably the blue one.
/// The other half stays a live-looking label (text at reduced alpha, not the
/// muted grey) so the control reads as two options. `muted` (locked /
/// unavailable) is never blue, so it cannot be mistaken for the selection.
fn segment_button(
    scheme: Scheme,
    selected: bool,
    muted: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let text = button_label(theme);
        // Unselected but selectable: full-strength text at reduced alpha. This
        // is the colour of a choice you can still make, as opposed to the
        // muted grey of one you cannot.
        let idle = Color { a: 0.75, ..text };

        let (background, color, border, shadow) = if muted {
            // Locked or unavailable: no blue at all. When it is nonetheless
            // the saved setting it keeps a faint neutral pill, so the state
            // still reads without looking like an active blue selection.
            let (fill, color) = if selected {
                (
                    Color { a: 0.08, ..text },
                    Color {
                        a: 0.60,
                        ..scheme.muted
                    },
                )
            } else {
                (Color::TRANSPARENT, scheme.muted)
            };
            (fill.into(), color, Color::TRANSPARENT, Shadow::default())
        } else if selected {
            // Solid accent fill. Hover and press only move the lightness, and
            // the label is `on_accent`: dark on the light-theme sky blue,
            // light on the dark-theme blue, in both cases well above 7:1.
            let base = match status {
                button::Status::Hovered => theme::lighten(scheme.blue, 0.12),
                button::Status::Pressed => theme::darken(scheme.blue, 0.12),
                _ => scheme.blue,
            };
            let shadow = match status {
                button::Status::Pressed => Shadow::default(),
                _ => soft_shadow(),
            };
            (
                linear_gradient(theme::lighten(base, 0.08), theme::darken(base, 0.08)),
                scheme.on_accent,
                // Rim blended toward the label colour: a deeper blue edge on
                // the solid fill in both themes.
                theme::mix(scheme.blue, scheme.on_accent, 0.25),
                shadow,
            )
        } else {
            // Hover and press still read on the unselected side. `Disabled`
            // here only means "not clickable right now" (the run is in
            // progress), not locked, so it dims but stays a visible label.
            let (fill, color) = match status {
                button::Status::Hovered => (0.12, text),
                button::Status::Pressed => (0.18, text),
                button::Status::Disabled => (0.0, Color { a: 0.45, ..scheme.text }),
                button::Status::Active => (0.0, idle),
            };
            (
                Color { a: fill, ..text }.into(),
                color,
                Color::TRANSPARENT,
                Shadow::default(),
            )
        };
        button::Style {
            background: Some(background),
            text_color: color,
            border: iced::border::Border {
                color: border,
                width: if selected { 1.0 } else { 0.0 },
                radius: PILL.into(),
            },
            shadow,
        }
    }
}

/// Verb-phrase action used beside the toggle, not as a third segment.
fn text_action_button(scheme: Scheme) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let label = button_label(theme);
        let (background, color) = match status {
            button::Status::Hovered => (
                Color {
                    a: 0.08,
                    ..label
                },
                label,
            ),
            button::Status::Pressed => (
                Color {
                    a: 0.16,
                    ..label
                },
                label,
            ),
            button::Status::Disabled => (
                Color::TRANSPARENT,
                Color {
                    a: 0.30,
                    ..scheme.text
                },
            ),
            _ => (Color::TRANSPARENT, label),
        };
        button::Style {
            background: Some(background.into()),
            text_color: color,
            border: iced::border::rounded(6.0),
            shadow: Shadow::default(),
        }
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

/// The wash under the row the pointer is on, for every menu in the app.
///
/// Both the header menus and the pick lists are drawn on `dialog_bg`, so one
/// rule serves both. The band is the surface mixed away from itself rather than
/// a fixed token: `track` equals `dialog_bg` in the dark palette (both Gray 80),
/// which is why the header menus' hover was invisible there, and `timeline_bg`
/// was only about 1.3:1 against the same surface. These mixes give roughly
/// 1.6:1 in light and 1.75:1 in dark — a clear row under the pointer that is
/// still a wash and not a filled button.
fn hover_band(scheme: &Scheme) -> Color {
    if theme::is_dark_scheme(scheme) {
        theme::lighten(scheme.dialog_bg, 0.18)
    } else {
        theme::darken(scheme.dialog_bg, 0.20)
    }
}

/// The surface every pick-list menu is drawn on.
///
/// iced's default menu is a square popup filled with the window colour and edged
/// with a `background.strong` hairline, which floats with no elevation cue and
/// matches none of the app's surfaces. `menu::Style` has no shadow field, so the
/// fill and the hairline *are* the elevation: this puts it on the raised dialog
/// surface with the card radius, and replaces the row highlight.
///
/// iced has one index for the row under the pointer and for the row that is the
/// current value (`hovered_option`, seeded from the selection when the menu
/// opens), so a single `selected_background` paints both and they cannot be told
/// apart by colour. The band is therefore the same wash the header menus use,
/// and the chosen value stays legible in the closed control above the menu.
fn pick_menu_style(scheme: Scheme) -> impl Fn(&iced::Theme) -> menu::Style {
    move |_theme| menu::Style {
        background: scheme.dialog_bg.into(),
        border: iced::border::Border {
            color: scheme.border,
            width: 1.0,
            radius: theme::RADIUS_MD.into(),
        },
        text_color: scheme.text,
        selected_text_color: scheme.text,
        selected_background: hover_band(&scheme).into(),
    }
}

fn pill_pick(scheme: Scheme) -> impl Fn(&iced::Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let mut style = pick_list::default(theme, status);
        style.border.radius = PILL.into();
        // Open: the handle takes the menu's surface, so the list reads as this
        // control opening rather than as a popup landing under a pill. iced's own
        // open state only tints the border with the accent, which would draw a
        // blue edge above the menu's grey hairline.
        if matches!(status, pick_list::Status::Opened) {
            style.background = scheme.dialog_bg.into();
            style.border.color = scheme.border;
        }
        style
    }
}

/// Borderless pick list whose colours mirror the secondary (Advanced) button.
fn speed_pick(scheme: Scheme) -> impl Fn(&iced::Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let button_status = match status {
            pick_list::Status::Hovered | pick_list::Status::Opened => button::Status::Hovered,
            pick_list::Status::Active => button::Status::Active,
        };
        let base = button::secondary(theme, button_status);
        // The speed label is a button-level label, so it follows the same rule:
        // pure white on a dark theme, the theme text otherwise.
        let text = button_label(theme);
        pick_list::Style {
            text_color: text,
            placeholder_color: text,
            handle_color: text,
            // Open: the same surface as its list, like the pill handles. The
            // border stays transparent — this idiom has no chrome when closed, and
            // growing one only while open would be a new shape rather than the same
            // control opening.
            background: if matches!(status, pick_list::Status::Opened) {
                scheme.dialog_bg.into()
            } else {
                base.background.unwrap_or_else(|| Color::TRANSPARENT.into())
            },
            border: iced::border::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: PILL.into(),
            },
        }
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

/// A control in the line editor panel: the live pill, or the greyed one when
/// the panel already knows the action would refuse. One style function for both
/// states keeps the row's buttons a single type, and the radius is pinned to
/// half the diameter so hover, pressed and disabled all draw a circle rather
/// than a pill whose ends depend on the box's height.
fn panel_icon_button(
    scheme: Scheme,
    enabled: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = if enabled {
            pill_button_secondary(theme, status)
        } else {
            greyed_button(scheme)(theme, status)
        };
        style.border.radius = (PANEL_ICON_DIAMETER / 2.0).into();
        style
    }
}

/// The accent fill with the card hairline around it.
///
/// `color_button` outlines a filled button with a lighter tint of its own hue,
/// which on a dark window leaves the two queue actions looking flush with the
/// panel. In dark mode this wraps them in the same border every card uses.
fn color_button_outlined(
    bg: Color,
    ink: Color,
    scheme: Scheme,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = color_button(bg, ink)(theme, status);
        if theme::is_dark_scheme(&scheme) {
            style.border.color = scheme.border;
        }
        style
    }
}

fn color_button(bg: Color, ink: Color) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let (top, bottom, text, border, border_width, shadow) = match status {
            button::Status::Hovered => (
                theme::lighten(bg, 0.30),
                theme::lighten(bg, 0.10),
                ink,
                Color {
                    a: 0.95,
                    ..theme::lighten(bg, 0.78)
                },
                2.0,
                Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.30),
                    offset: Vector::new(0.0, 4.0),
                    blur_radius: 12.0,
                },
            ),
            button::Status::Pressed => (
                theme::darken(bg, 0.18),
                theme::darken(bg, 0.06),
                ink,
                Color {
                    a: 0.80,
                    ..theme::lighten(bg, 0.65)
                },
                2.0,
                Shadow::default(),
            ),
            // Filled buttons keep their hue when disabled, but they must read
            // as unavailable: a dimmer fill and grey text rather than the full
            // strength label.
            button::Status::Disabled => {
                let faded = Color { a: 0.26, ..bg };
                (
                    faded,
                    faded,
                    Color {
                        a: 0.55,
                        ..theme.palette().text
                    },
                    Color { a: 0.14, ..bg },
                    1.0,
                    Shadow::default(),
                )
            }
            _ => (
                theme::lighten(bg, 0.12),
                theme::darken(bg, 0.04),
                ink,
                Color {
                    a: 0.45,
                    ..theme::lighten(bg, 0.55)
                },
                1.0,
                soft_shadow(),
            ),
        };
        button::Style {
            background: Some(linear_gradient(top, bottom)),
            text_color: text,
            border: iced::border::Border {
                color: border,
                width: border_width,
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

/// Highlight the words already spoken at `at`.
///
/// A band behind the words, not a colour change: the row's own fill comes from
/// the same speaker accent, so tinting the text made it disappear. All glyphs
/// keep `ink`; the current word is underlined while audio is playing. Returns
/// `None` when the stored words do not line up with the rendered text, so the
/// caller falls back to plain spans.
fn karaoke_spans(
    raw: &str,
    words: &[Word],
    at: f32,
    ink: Color,
    band: Color,
    size: f32,
    playing: bool,
) -> Option<Vec<text::Span<'static, Message>>> {
    let runs = karaoke_runs(raw, words, at, playing)?;
    Some(
        runs.into_iter()
            .map(|(text, spoken, current)| {
                let mut span = text::Span::new(text).size(size).color(ink);
                if spoken {
                    span = span.background(band);
                }
                if current {
                    span = span.underline(true);
                }
                span
            })
            .collect(),
    )
}

/// The decisions behind [`karaoke_spans`]: `(text, spoken, current word)`.
///
/// Keeping them separate makes the tricky part — matching words to the rendered
/// text without splitting CJK or punctuation — testable without depending on
/// private span fields.
fn karaoke_runs(
    raw: &str,
    words: &[Word],
    at: f32,
    playing: bool,
) -> Option<Vec<(String, bool, bool)>> {
    if words.is_empty() {
        return None;
    }
    let mut runs = Vec::with_capacity(words.len() * 2);
    let mut cursor = 0usize;

    for (index, word) in words.iter().enumerate() {
        let rest = raw.get(cursor..)?;
        let trimmed = rest.trim_start();
        let gap = rest.len() - trimmed.len();
        if gap > 0 {
            let spoken = index > 0 && words[index - 1].end <= at;
            runs.push((rest[..gap].to_string(), spoken, false));
        }
        if !trimmed.starts_with(&word.text) {
            return None;
        }
        cursor += gap + word.text.len();

        let spoken = word.end <= at;
        let current = playing && word.start <= at && at < word.end;
        runs.push((word.text.clone(), spoken, current));
    }

    // Text after the last word (rare: trailing punctuation the model did not
    // emit as a word) keeps the last word's state rather than breaking the line.
    let tail = raw.get(cursor..).unwrap_or_default();
    if !tail.is_empty() {
        let spoken = words.last().is_some_and(|word| word.end <= at);
        runs.push((tail.to_string(), spoken, false));
    }

    Some(runs)
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
    use iced::advanced::widget::tree::{Tag, Tree};
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
        tier: Option<std::ffi::OsString>,
    }

    impl ConfigEnv {
        fn new(tag: &str) -> Self {
            let dir = temp_dir(tag);
            let xdg = std::env::var_os("XDG_CONFIG_HOME");
            let appdata = std::env::var_os("APPDATA");
            // A tier override inherited from the developer shell must not
            // decide the outcome of a test. `pro_app` grants Pro through the
            // license record instead.
            let tier = std::env::var_os("HIPPO_TRANSCRIBE_TIER");
            // `settings_path` uses APPDATA on Windows and XDG_CONFIG_HOME
            // elsewhere, so set both to keep tests hermetic on every platform.
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            std::env::set_var("APPDATA", &dir);
            std::env::remove_var("HIPPO_TRANSCRIBE_TIER");
            Self {
                dir,
                xdg,
                appdata,
                tier,
            }
        }
    }

    impl Drop for ConfigEnv {
        fn drop(&mut self) {
            restore_env("XDG_CONFIG_HOME", self.xdg.take());
            restore_env("APPDATA", self.appdata.take());
            restore_env("HIPPO_TRANSCRIBE_TIER", self.tier.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    /// App with Pro entitlements so feature-gated tests exercise their feature.
    fn pro_app() -> App {
        let now = std::time::SystemTime::now();
        if let Some(path) = App::license_path() {
            let mut state = license::State::load(&path, now);
            state.grant_pro(now);
            state.save(&path);
        }
        let mut app = App::new();
        app.license.grant_pro(now);
        app
    }

    /// Open the inline editor on `index` through the real message path.
    fn open_line_edit(app: &mut App, index: usize) {
        let _ = app.update(Message::EditLine(index));
        assert!(app.line_edit.is_some(), "the editor did not open on {index}");
    }

    /// The current row of the open line editor, mutably.
    fn current_row_mut(app: &mut App) -> &mut LineEditRow {
        let edit = app.line_edit.as_mut().unwrap();
        let position = edit.current_position();
        edit.rows.get_mut(position).unwrap()
    }

    /// Editor content with the caret `caret` characters in, so a test can put
    /// the cursor mid-line the way a click in the field would.
    fn content_with_caret(text: &str, caret: usize) -> text_editor::Content {
        let mut content = text_editor::Content::with_text(text);
        content.perform(text_editor::Action::Move(text_editor::Motion::Home));
        for _ in 0..caret {
            content.perform(text_editor::Action::Move(text_editor::Motion::Right));
        }
        content
    }

    /// A Pro app with one finished transcription item on disk, opened for
    /// editing. Returns the app, the transcript path and the media directory.
    fn line_edit_fixture(tag: &str, transcript_text: &str) -> (App, PathBuf, PathBuf) {
        let media = temp_dir(&format!("{tag}-media"));
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(&transcript, transcript_text).unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(12.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);
        (app, transcript, media)
    }

    fn open_test_item(app: &mut App, index: usize) {
        let source = app.queue[index].transcript.clone().unwrap();
        let Some(text) = backend::read_transcript(&source) else {
            app.selected = Some(index);
            app.selected_view = Some(ItemView {
                index,
                source: source.clone(),
                transcript: String::new(),
                lines: Vec::new(),
                turns: Vec::new(),
                peaks: Arc::new(Vec::new()),
                duration: app.queue[index].duration.map(|seconds| seconds as f32),
                audio: app.queue[index].audio.clone(),
                output: None,
            });
            app.view_error = Some(format!("Could not read {}", source.display()));
            return;
        };
        let output = source
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("txt"))
            .unwrap_or(false)
            .then(|| source.clone());
        let _ = app.open_item_view(index, Some((text, output)));
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

        let mut app = pro_app();
        assert!(app.queue.is_empty());
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio.clone()),
            status: ItemStatus::Done,
            duration: Some(1.5),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: vec![backend::Turn {
                start: 0.0,
                end: 1.5,
                speaker: "SPEAKER 1".to_string(),
            }],
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
        assert_eq!(loaded[0].turns.len(), 1);
        assert_eq!(loaded[0].turns[0].speaker, "SPEAKER 1");
        assert_eq!(loaded[0].peaks.as_ref(), &vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn first_launch_clears_a_stale_queue() {
        let _guard = env_lock();
        let config = ConfigEnv::new("first-launch-queue");
        let dir = config.dir.clone();
        let config_dir = dir.join("hippo-transcribe");
        std::fs::create_dir_all(&config_dir).unwrap();

        let audio = dir.join("stale.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let state = QueueState {
            last_run: None,
            items: vec![QueueRecord {
                audio: Some(audio.display().to_string()),
                transcript: None,
                duration: None,
                status: "done".to_string(),
                message: None,
                stats: None,
                turns: Vec::new(),
                peaks: Vec::new(),
            }],
            last_batch: vec![audio.display().to_string()],
        };
        std::fs::write(
            config_dir.join("queue.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();

        let app = App::new();
        assert!(app.queue.is_empty());
        assert!(app.batch_paths.is_empty());
        assert!(!App::queue_path().unwrap().exists());
        assert!(App::settings_path().unwrap().exists());
    }

    #[test]
    fn line_text_drops_the_prefix_once() {
        let lines = parse_lines("[00:00] SPEAKER 1: hello there\n[00:05] plain text\n");
        assert_eq!(line_text(&lines[0]), "hello there");
        assert_eq!(line_text(&lines[1]), "plain text");
    }

    #[test]
    fn a_cleared_speaker_label_leaves_no_colon() {
        // Single-speaker transcripts lose their labels; older files still carry
        // the colon that belonged to the label.
        let lines = parse_lines("[00:00] : hello there\n[00:05] hello again\n");
        assert_eq!(line_text(&lines[0]), "hello there");
        assert_eq!(line_text(&lines[1]), "hello again");
    }

    #[test]
    fn a_single_speaker_run_says_so() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("one-speaker");
        let mut app = pro_app();
        app.num_speakers = Some(2);
        // Diarization emitted turns but cleared every label: one voice.
        app.turns = vec![backend::Turn {
            start: 0.0,
            end: 5.0,
            speaker: String::new(),
        }];
        app.lines = vec![DocLine {
            raw: "[00:00] hello".to_string(),
            speaker: String::new(),
            start: Some(0.0),
            confidence: None,
        }];
        app.invalidate_derived();
        assert!(app.only_one_speaker());

        // An ASR-only run has no turns and must not claim a single speaker.
        app.turns.clear();
        app.invalidate_derived();
        assert!(!app.only_one_speaker());
    }

    #[test]
    fn low_confidence_marking_is_off_by_default() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("confidence-default");
        let app = pro_app();
        // Opt-in: the dulled rows are useful for checking a transcript, but
        // they make every other read noisier, so they stay off until asked for.
        assert!(!app.confidence_view);
    }

    #[test]
    fn editing_a_low_confidence_line_vouches_for_it() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("confidence-edit-config");
        let media = temp_dir("confidence-edit-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello there\n[00:05] SPEAKER 2: hi\n",
        )
        .unwrap();
        std::fs::write(
            media.join("talk.json"),
            r#"{
                "lines": [
                    {"time":"00:00","speaker":"SPEAKER 1","text":"hello there","start":0.0,"confidence":-0.8},
                    {"time":"00:05","speaker":"SPEAKER 2","text":"hi","start":5.0,"confidence":-0.2}
                ],
                "turns": [],
                "duration": 10.0
            }"#,
        )
        .unwrap();

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
        assert!(is_low_confidence(&app.selected_view.as_ref().unwrap().lines[0]));

        let _ = app.update(Message::EditLine(0));
        current_row_mut(&mut app).content = text_editor::Content::with_text("hello there, edited");
        let _ = app.update(Message::DoneLineEdit);

        let lines = &app.selected_view.as_ref().unwrap().lines;
        assert_eq!(lines[0].confidence, Some(HUMAN_CONFIDENCE));
        assert!(!is_low_confidence(&lines[0]));
        // The neighbour was not part of the edit, so it keeps its ASR value.
        assert_eq!(lines[1].confidence, Some(-0.2));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn flipping_a_low_confidence_line_vouches_for_it() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("confidence-flip-config");
        let media = temp_dir("confidence-flip-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello there\n[00:05] SPEAKER 2: hi\n",
        )
        .unwrap();
        std::fs::write(
            media.join("talk.json"),
            r#"{
                "lines": [
                    {"time":"00:00","speaker":"SPEAKER 1","text":"hello there","start":0.0,"confidence":-0.8},
                    {"time":"00:05","speaker":"SPEAKER 2","text":"hi","start":5.0,"confidence":-0.2}
                ],
                "turns": [],
                "duration": 10.0
            }"#,
        )
        .unwrap();

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
        assert!(is_low_confidence(&app.selected_view.as_ref().unwrap().lines[0]));

        let _ = app.flip_line_speaker(0);

        let lines = &app.selected_view.as_ref().unwrap().lines;
        assert_eq!(lines[0].speaker, "SPEAKER 2");
        assert_eq!(lines[0].confidence, Some(HUMAN_CONFIDENCE));
        assert!(!is_low_confidence(&lines[0]));
        assert_eq!(lines[1].confidence, Some(-0.2));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn editing_rewrites_the_json_sidecar_confidence() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("confidence-sidecar-config");
        let media = temp_dir("confidence-sidecar-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello there\n[00:05] SPEAKER 2: hi\n",
        )
        .unwrap();
        std::fs::write(
            media.join("talk.json"),
            r#"{
                "lines": [
                    {"time":"00:00","speaker":"SPEAKER 1","text":"hello there","start":0.0,"confidence":-0.8},
                    {"time":"00:05","speaker":"SPEAKER 2","text":"hi","start":5.0,"confidence":-0.2}
                ],
                "turns": [],
                "duration": 10.0
            }"#,
        )
        .unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(10.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        let _ = app.update(Message::EditLine(0));
        current_row_mut(&mut app).content = text_editor::Content::with_text("hello there, edited");
        let _ = app.update(Message::DoneLineEdit);

        // Reloading through the same path the viewer uses sees the voucher.
        let confidences = backend::read_confidences(&transcript);
        assert_eq!(confidences[0], Some(HUMAN_CONFIDENCE));
        assert_eq!(confidences[1], Some(-0.2));

        // Only the confidences changed: the rest of the sidecar is intact.
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(media.join("talk.json")).unwrap())
                .unwrap();
        assert_eq!(value["duration"].as_f64(), Some(10.0));
        assert!(value["turns"].as_array().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&media);
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

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
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

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
        open_line_edit(&mut app, 0);
        let _ = app.update(Message::MergeNextLine);
        assert_eq!(app.selected_view.as_ref().unwrap().lines.len(), 1);
        assert!(app.shown_transcript().contains("hello world again"));

        // The editor stays open on the surviving line with the caret at the end.
        assert_eq!(app.line_edit.as_ref().unwrap().index, 0);
        let row = current_row_mut(&mut app);
        assert_eq!(row.index, 0);
        assert_eq!(row.content.text().trim_end(), "hello world again");
        for _ in 0..11 {
            row.content
                .perform(text_editor::Action::Move(text_editor::Motion::Left));
        }
        let _ = app.update(Message::SplitLineAtCursor);
        let lines = &app.selected_view.as_ref().unwrap().lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "hello");
        assert_eq!(line_text(&lines[1]), "world again");
        // The left half keeps the original start; the right half is placed
        // between it and the end of the timeline by the character ratio.
        assert_eq!(lines[0].start, Some(0.0));
        let right = lines[1].start.unwrap();
        assert!((2.9..3.0).contains(&right), "right start {right}");
        assert!(lines[1].raw.starts_with("[00:02] SPEAKER 1: world again"));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn merge_previous_line_folds_into_the_earlier_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("merge-prev-config");
        let media = temp_dir("merge-prev-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello world\n[00:05] SPEAKER 1: again\n",
        )
        .unwrap();

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
        open_line_edit(&mut app, 1);
        let _ = app.update(Message::MergePrevLine);
        let lines = &app.selected_view.as_ref().unwrap().lines;
        assert_eq!(lines.len(), 1);
        assert!(app.shown_transcript().contains("hello world again"));
        assert!(lines[0].raw.starts_with("[00:00]"));

        let before = app.selected_view.as_ref().unwrap().lines.len();
        open_line_edit(&mut app, 0);
        let _ = app.update(Message::MergePrevLine);
        assert_eq!(app.selected_view.as_ref().unwrap().lines.len(), before);
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("No previous line to merge")
        );

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn line_editor_tints_the_edited_line_with_its_speaker_colour() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("line-accent-config");
        let media = temp_dir("line-accent-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello\n[00:05] SPEAKER 2: hi there\n",
        )
        .unwrap();

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);

        let scheme = app.scheme();
        let order = app.speaker_order();
        assert_eq!(order, vec!["SPEAKER 1".to_string(), "SPEAKER 2".to_string()]);

        // Editing line 0 tints the editor with the first palette slot; editing
        // line 1 tints it with the second. Same tokens as the transcript.
        for (index, position) in [(0usize, 0usize), (1, 1)] {
            open_line_edit(&mut app, index);
            let edited = app.line_edit.as_ref().unwrap().current_row().unwrap();
            assert_eq!(
                line_accent(&order, &edited.speaker, &scheme),
                speaker_palette(position, &scheme)
            );
            assert_eq!(
                line_accent(&order, &app.shown_lines()[index].speaker, &scheme),
                speaker_palette(position, &scheme)
            );
            let _ = app.view();
        }
        assert_ne!(speaker_palette(0, &scheme), speaker_palette(1, &scheme));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn line_editor_undo_and_redo_rewrite_the_transcript() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("undo-config");
        let media = temp_dir("undo-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        let original = "[00:00] SPEAKER 1: hello world\n[00:05] SPEAKER 1: again\n";
        std::fs::write(&transcript, original).unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(10.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        open_line_edit(&mut app, 0);
        assert!(!app.line_undo_available());
        let _ = app.update(Message::MergeNextLine);
        assert_eq!(app.selected_view.as_ref().unwrap().lines.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] SPEAKER 1: hello world again\n"
        );
        assert!(app.line_undo_available());
        assert!(!app.line_redo_available());

        let _ = app.update(Message::UndoLineEdit);
        let lines = &app.selected_view.as_ref().unwrap().lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "hello world");
        assert_eq!(line_text(&lines[1]), "again");
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);
        // The editor stays open and shows the restored line.
        let edit = app.line_edit.as_ref().unwrap();
        assert_eq!(edit.index, 0);
        assert_eq!(edit.rows.len(), 1);
        let row = edit.current_row().unwrap();
        assert_eq!(row.index, 0);
        assert_eq!(row.content.text().trim_end(), "hello world");
        assert!(app.line_redo_available());

        let _ = app.update(Message::RedoLineEdit);
        assert_eq!(app.selected_view.as_ref().unwrap().lines.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] SPEAKER 1: hello world again\n"
        );

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn done_writes_the_edited_row_and_undo_restores_it() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("multi-edit-config");
        let media = temp_dir("multi-edit-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n[00:09] SPEAKER 1: three\n";
        std::fs::write(&transcript, original).unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(12.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        let _ = app.update(Message::EditLine(1));
        {
            let edit = app.line_edit.as_mut().unwrap();
            // One row only: the field replaces the line it edits.
            assert_eq!(edit.rows.len(), 1);
            assert_eq!(edit.index, 1);
            assert_eq!(edit.current_position(), 0);
            edit.rows[0].speaker = "SPEAKER 1".to_string();
            edit.rows[0].content = text_editor::Content::with_text("edited");
        }
        // The inline field and the panel lay out without panicking.
        let _ = app.view();
        let _ = app.update(Message::DoneLineEdit);
        assert!(app.line_edit.is_none());
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 1: edited\n[00:09] SPEAKER 1: three\n"
        );
        // One row rewritten, one undo step.
        assert_eq!(app.line_history.undo.len(), 1);

        let _ = app.update(Message::UndoLineEdit);
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn deleting_the_edited_line_asks_first_and_undo_restores_it() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("delete-line-config");
        let media = temp_dir("delete-line-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n[00:09] SPEAKER 1: three\n";
        std::fs::write(&transcript, original).unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(12.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        let _ = app.update(Message::EditLine(1));
        // Asking is not deleting: the file is untouched until confirmation.
        let _ = app.update(Message::DeleteLineEdit);
        assert!(app.delete_confirm);
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);

        // Escape closes the confirmation and leaves the editor open.
        let _ = app.update(Message::Escape);
        assert!(!app.delete_confirm);
        assert!(app.line_edit.is_some());

        let _ = app.update(Message::DeleteLineEdit);
        let _ = app.update(Message::ConfirmDeleteLine);
        assert!(app.line_edit.is_none());
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] SPEAKER 1: one\n[00:09] SPEAKER 1: three\n"
        );

        // The removal is one undo step, like every other line edit.
        let _ = app.update(Message::UndoLineEdit);
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn an_empty_line_is_not_written_into_the_transcript() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("empty-line-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, transcript, media) = line_edit_fixture("empty-line", original);

        open_line_edit(&mut app, 0);
        current_row_mut(&mut app).content = text_editor::Content::with_text("");
        let _ = app.update(Message::DoneLineEdit);

        // A blank line would have no row of its own: the field stays open and
        // nothing is written.
        assert!(app.line_edit.is_some(), "the editor closed on an empty line");
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);
        assert_eq!(line_text(&app.edit_lines()[0]), "one");
        // The panel and the field lay out with nothing in them.
        let _ = app.view();

        // Escape is still the way out, and leaves the line alone.
        let _ = app.update(Message::Escape);
        assert!(app.line_edit.is_none());
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn splitting_leaves_the_caret_at_the_start_of_the_new_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("split-caret-config");
        let original = "[00:00] SPEAKER 1: hello there\n[00:05] SPEAKER 2: bye\n";
        let (mut app, _transcript, media) = line_edit_fixture("split-caret", original);

        open_line_edit(&mut app, 0);
        current_row_mut(&mut app).content = content_with_caret("hello there", 5);
        let _ = app.update(Message::SplitLineAtCursor);

        let lines = app.edit_lines().to_vec();
        assert_eq!(line_text(&lines[0]), "hello");
        assert_eq!(line_text(&lines[1]), "there");

        // The editor follows the text that was cut off, caret at its start.
        let edit = app.line_edit.as_ref().expect("the editor stayed open");
        assert_eq!(edit.index, 1);
        let row = edit.current_row().expect("a row on the new line");
        assert_eq!(row.content.text().trim_end(), "there");
        assert_eq!(row.content.cursor_position(), (0, 0));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn joining_leaves_the_caret_where_the_two_lines_met() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("join-caret-config");
        let original =
            "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 1: two\n[00:09] SPEAKER 1: three\n";
        let (mut app, _transcript, media) = line_edit_fixture("join-caret", original);

        // Upward: the editor lands on the surviving line, at the join.
        open_line_edit(&mut app, 1);
        let _ = app.update(Message::MergePrevLine);
        let edit = app.line_edit.as_ref().expect("the editor stayed open");
        assert_eq!(edit.index, 0);
        let row = edit.current_row().expect("a row on the surviving line");
        assert_eq!(row.content.text().trim_end(), "one two");
        assert_eq!(row.content.cursor_position(), (0, 4));

        // Downward: same rule, caret just past the original text.
        let _ = app.update(Message::MergeNextLine);
        let row = app
            .line_edit
            .as_ref()
            .and_then(|edit| edit.current_row())
            .expect("a row on the joined line");
        assert_eq!(row.content.text().trim_end(), "one two three");
        assert_eq!(row.content.cursor_position(), (0, 8));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn enter_over_an_open_picker_does_not_save_the_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("picker-enter-config");
        let original = "[00:00] SPEAKER 1: one\n";
        let (mut app, transcript, media) = line_edit_fixture("picker-enter", original);

        open_line_edit(&mut app, 0);
        current_row_mut(&mut app).content = text_editor::Content::with_text("edited");

        // A menu owns Enter while it is open.
        let _ = app.update(Message::ToggleHeaderMenu(HeaderMenu::View));
        let _ = app.update(Message::EnterSubmit);
        assert!(app.line_edit.is_some(), "Enter committed under the menu");
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);

        // With the menu closed Enter saves, as Done does.
        let _ = app.update(Message::CloseHeaderMenu);
        let _ = app.update(Message::EnterSubmit);
        assert!(app.line_edit.is_none());
        assert!(std::fs::read_to_string(&transcript)
            .unwrap()
            .contains("edited"));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn an_open_menu_takes_the_pointer_off_the_transcript() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("menu-pointer-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("menu-pointer", original);

        // With nothing open a row lights up as usual.
        let _ = app.update(Message::LineHover(Some(0)));
        assert_eq!(app.transcript_line_hover, Some(0));

        // Opening the View menu takes the pointer off the rows: no other row can
        // light up while the menu is up. The remembered row is kept, because
        // clearing it meant nothing re-entered it when the menu closed -- the
        // row's `on_enter` does not fire again while the pointer stays inside --
        // so its swap button stayed gone until the pointer left and came back.
        let _ = app.update(Message::ToggleHeaderMenu(HeaderMenu::View));
        assert_eq!(app.header_menu, Some(HeaderMenu::View));
        assert!(!app.popup_open(), "a menu is not a dialog");
        // The catcher and the menu layer build and lay out without the card
        // losing its shape.
        let _ = app.view();
        assert_eq!(app.transcript_line_hover, Some(0));
        let _ = app.update(Message::LineHover(Some(1)));
        assert_eq!(
            app.transcript_line_hover,
            Some(0),
            "a row lit up under the open View menu"
        );

        // Closing it gives the hover back.
        let _ = app.update(Message::CloseHeaderMenu);
        let _ = app.update(Message::LineHover(Some(1)));
        assert_eq!(app.transcript_line_hover, Some(1));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_menu_row_hover_is_visible_in_both_palettes() {
        fn channel(value: f32) -> f32 {
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }

        fn luminance(color: Color) -> f32 {
            0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
        }

        fn contrast(a: Color, b: Color) -> f32 {
            let (hi, lo) = if luminance(a) >= luminance(b) {
                (luminance(a), luminance(b))
            } else {
                (luminance(b), luminance(a))
            };
            (hi + 0.05) / (lo + 0.05)
        }

        // Every menu — the header's own and every pick list — is drawn on
        // `dialog_bg`. The old fixed band was 1.30:1 in light and 1.31:1 in dark,
        // and the header's `track` tint was 1.27:1 and 1.00:1 (invisible in dark).
        for scheme in [Scheme::light(), Scheme::dark()] {
            let ratio = contrast(hover_band(&scheme), scheme.dialog_bg);
            assert!(
                ratio >= 1.5,
                "{ratio:.2}:1 is too quiet to read as a row under the pointer"
            );
            assert!(
                ratio <= 2.2,
                "{ratio:.2}:1 is a filled button, not a wash"
            );
        }
    }

    #[test]
    fn the_filename_budget_follows_the_window_width() {
        // 1024 is the app's minimum: a 648 px card leaves the identity block
        // 256 px, which is 31 characters at 13 px and 37 at 11 px.
        let (name, meta) = header_identity_budgets(1024.0);
        assert_eq!(name, 31);
        assert_eq!(meta, 37);

        // A wide window gives the whole name back; the fixed 20 characters this
        // replaced would have cut it anyway.
        assert_eq!(header_identity_budgets(1600.0).0, 102);

        // Never negative or unreadable, even below the app's minimum width.
        assert_eq!(header_identity_budgets(300.0).0, MIN_NAME_CHARS);
        assert_eq!(header_identity_budgets(0.0).1, MIN_NAME_CHARS);
    }

    #[test]
    fn each_header_menu_offset_advances_by_the_button_before_it() {
        // The menus hang from their buttons' left edges, so each offset is the
        // header padding plus the buttons to its left and the gaps between
        // them. Pinned here because a hardcoded offset would drift from the
        // button widths it is supposed to match.
        assert_eq!(file_menu_offset(), HEADER_PAD_X);
        assert_eq!(
            edit_menu_offset() - file_menu_offset(),
            FILE_BUTTON_WIDTH + theme::SPACE_SM
        );
        assert_eq!(
            view_menu_offset() - edit_menu_offset(),
            EDIT_BUTTON_WIDTH + theme::SPACE_SM
        );
    }

    #[test]
    fn every_header_menu_renders() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("all-menus-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("all-menus", original);
        for menu in [
            HeaderMenu::File,
            HeaderMenu::Edit,
            HeaderMenu::View,
            HeaderMenu::Help,
        ] {
            app.header_menu = Some(menu);
            let _ = app.view();
        }
        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_menu_row_with_a_key_is_no_wider_than_its_label_plus_key() {
        let wide = [
            MenuRow {
                label: "Timestamps",
                key: Some("t"),
                checked: false,
                enabled: true,
                action: None,
            },
            MenuRow {
                label: "Join",
                key: Some("j"),
                checked: false,
                enabled: true,
                action: None,
            },
        ];
        let narrow = [MenuRow {
            label: "Join",
            key: Some("j"),
            checked: false,
            enabled: true,
            action: None,
        }];
        assert!(header_menu_width(&wide) > header_menu_width(&narrow));

        // The card is sized from the same row value the body is built from: one
        // card padding, one row padding, the check slot, its gap, the label, the
        // minimum gap before a key and the key itself.
        let expected = theme::SPACE_MD * 2.0
            + theme::SPACE_SM * 2.0
            + HEADER_MENU_CHECK
            + theme::SPACE_SM
            + estimated_text_width("Timestamps", 16.0)
            + theme::SPACE_2XL
            + estimated_text_width("t", theme::TEXT_XS);
        assert!((header_menu_width(&wide) - expected).abs() < 0.01);
    }

    #[test]
    fn the_help_menu_hangs_from_the_header_row() {
        // The app header row's height is its logo's, and it sits inside the
        // outer padding; the menu drops one small gap below it. Its right edge
        // is `OUTER_PADDING` in, which is the row's own right edge — the Help
        // button ends that row.
        assert_eq!(HELP_MENU_TOP, OUTER_PADDING + HEADER_LOGO_SIZE + theme::SPACE_SM);
        assert_eq!(HELP_MENU_TOP, 85.0);
    }


    #[test]
    fn the_line_editor_keys_are_wired_to_their_actions() {
        use keyboard::key::Named;
        // Alt+arrows join, Enter and Escape commit and cancel.
        assert!(matches!(
            App::key_press(Key::Named(Named::ArrowUp), Modifiers::ALT),
            Some(Message::MergePrevLine)
        ));
        assert!(matches!(
            App::key_press(Key::Named(Named::ArrowDown), Modifiers::ALT),
            Some(Message::MergeNextLine)
        ));
        assert!(matches!(
            App::key_press(Key::Character("K".into()), Modifiers::COMMAND | Modifiers::SHIFT),
            Some(Message::SplitLineAtCursor)
        ));
        assert!(matches!(
            App::key_press(Key::Named(Named::Enter), Modifiers::default()),
            Some(Message::EnterSubmit)
        ));
        assert!(matches!(
            App::key_press(Key::Named(Named::Escape), Modifiers::default()),
            Some(Message::Escape)
        ));
        // Zoom keeps working with the shift key held, as it produces "+".
        assert!(matches!(
            App::key_press(Key::Character("+".into()), Modifiers::COMMAND | Modifiers::SHIFT),
            Some(Message::ZoomIn)
        ));
        // Ctrl+S saves, and the bare key stays the SMS toggle: the single-key
        // arm is guarded by `!modifiers.command()`, so the two cannot shadow
        // each other.
        assert!(matches!(
            App::key_press(Key::Character("s".into()), Modifiers::COMMAND),
            Some(Message::SaveTranscriptAs)
        ));
        assert!(matches!(
            App::key_press(Key::Character("s".into()), Modifiers::default()),
            Some(Message::ViewShortcut(ViewShortcut::Sms))
        ));
    }

    #[test]
    fn clearing_the_find_query_leaves_the_app_idle() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("find-clear-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("find-clear", original);

        // Type through the real message path, then clear it the same way.
        let _ = app.update(Message::SearchChanged("one".to_string()));
        assert_eq!(app.matching_lines().len(), 1);
        let _ = app.update(Message::SearchChanged(String::new()));

        // A cleared query must not leave anything that rebuilds the row: no
        // tick subscription (10 Hz rebuilds of the field), no pending window
        // change, no stale match index.
        assert!(app.search_query.is_empty());
        assert_eq!(app.search_index, 0);
        assert!(
            !app.needs_ticks(),
            "a cleared query left the tick loop running"
        );
        assert!(
            !app.window_dirty,
            "a cleared query left a window change pending"
        );

        // Ticking in that state changes nothing, so the field cannot be rebuilt
        // frame after frame from the app side.
        let _ = app.update(Message::Tick);
        assert!(app.search_query.is_empty());
        assert!(!app.needs_ticks());

        // And the row still renders with the empty query.
        let _ = app.view();

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_find_row_keeps_its_shape_when_the_query_is_cleared() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("find-shape-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("find-shape", original);
        let scheme = app.scheme();

        // The match counter sits in a fixed slot: its text grows and shrinks on
        // every keystroke, but the row's children — and therefore the arrows
        // beside it — must not be re-flowed when the query changes.
        app.search_query = "one".to_string();
        let typed = {
            let row = app.search_element(&scheme);
            iced::advanced::widget::Tree::new(row.as_widget()).children.len()
        };
        app.search_query.clear();
        let cleared = {
            let row = app.search_element(&scheme);
            iced::advanced::widget::Tree::new(row.as_widget()).children.len()
        };

        assert_eq!(typed, cleared, "the Find row changed shape with the query");

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn merging_next_folds_the_typed_text_into_the_line_below() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("flush-merge-config");
        let media = temp_dir("flush-merge-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 1: two\n[00:09] SPEAKER 1: three\n",
        )
        .unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(12.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        let _ = app.update(Message::EditLine(1));
        {
            // Typed text is in the field only until the merge reads it; the
            // merge combines it with the line below and moves the indices.
            let row = current_row_mut(&mut app);
            row.speaker = "SPEAKER 2".to_string();
            row.content = text_editor::Content::with_text("two edited");
        }
        let _ = app.update(Message::MergeNextLine);

        let lines = &app.selected_view.as_ref().unwrap().lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].speaker, "SPEAKER 1");
        assert_eq!(line_text(&lines[0]), "one");
        assert_eq!(lines[1].speaker, "SPEAKER 2");
        assert_eq!(line_text(&lines[1]), "two edited three");
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two edited three\n"
        );
        // One merge is one undo step, and it is the same stack as a save.
        assert_eq!(app.line_history.undo.len(), 1);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn speaker_turns_fall_back_to_the_transcript_lines() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("turns-config");
        let media = temp_dir("turns-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello\n[00:04] SPEAKER 2: hi\n[00:08] SPEAKER 1: bye\n",
        )
        .unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(12.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        let scheme = app.scheme();
        assert_eq!(
            app.speaker_order(),
            vec!["SPEAKER 1".to_string(), "SPEAKER 2".to_string()]
        );
        let first = speaker_palette(0, &scheme);
        let second = speaker_palette(1, &scheme);
        assert_ne!(first, second);

        // No diarization turns: the strip is rebuilt from the transcript.
        let spans = app.speaker_turns(&scheme);
        assert!(spans.len() >= 3, "expected at least three spans");
        assert_eq!((spans[0].start, spans[0].end), (0.0, 4.0));
        assert_eq!((spans[1].start, spans[1].end), (4.0, 8.0));
        assert_eq!((spans[2].start, spans[2].end), (8.0, 12.0));
        assert_eq!(spans[0].color, first);
        assert_eq!(spans[1].color, second);
        assert_eq!(spans[2].color, first);

        // Matching turns still win: exact ranges, painted verbatim.
        app.selected_view.as_mut().unwrap().turns = vec![
            backend::Turn {
                start: 1.0,
                end: 3.0,
                speaker: "SPEAKER 2".to_string(),
            },
            backend::Turn {
                start: 5.0,
                end: 6.0,
                speaker: "SPEAKER 1".to_string(),
            },
        ];
        let spans = app.speaker_turns(&scheme);
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].start, spans[0].end), (1.0, 3.0));
        assert_eq!(spans[0].color, second);
        assert_eq!((spans[1].start, spans[1].end), (5.0, 6.0));
        assert_eq!(spans[1].color, first);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn transcribing_only_covers_untranscribed_files() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("scope-config");
        let dir = temp_dir("scope-media");

        let mut app = pro_app();
        for name in ["a.mp3", "b.mp3"] {
            let path = dir.join(name);
            std::fs::write(&path, b"audio").unwrap();
            let transcript = dir.join(format!("{name}.txt"));
            std::fs::write(&transcript, "[00:00] SPEAKER 1: hi\n").unwrap();
            app.queue.push(QueueItem {
                path: path.clone(),
                audio: Some(path),
                status: ItemStatus::Done,
                duration: Some(4.0),
                transcript: Some(transcript),
                stats: None,
                turns: Vec::new(),
                peaks: Arc::new(vec![0.1]),
            });
        }

        // Nothing selected, nothing queued: the queue is not re-run.
        app.selected = None;
        app.start();
        assert_eq!(app.queue[0].status, ItemStatus::Done);
        assert_eq!(app.queue[1].status, ItemStatus::Done);
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("Select a finished file to transcribe it again")
        );

        // A selected finished file is the only one queued again.
        app.selected = Some(1);
        app.start();
        assert_eq!(app.queue[0].status, ItemStatus::Done);
        assert!(matches!(app.queue[1].status, ItemStatus::Pending));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrubbing_the_timeline_jumps_the_transcript() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("seek-jump-config");
        let media = temp_dir("seek-jump-media");
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n[00:10] SPEAKER 1: three\n",
        )
        .unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(20.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);
        // A measured viewport is what makes scroll targets computable.
        app.transcript_content = 1200.0;
        app.transcript_viewport = 400.0;
        app.scroll_target = None;

        let expected = app.follow_target(1).unwrap();
        let _ = app.update(Message::Seek(5.0));
        assert_eq!(app.playhead, 5.0);
        assert!(app.scroll_seek);
        assert_eq!(app.scroll_target, Some(expected));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn an_impossible_throughput_never_becomes_an_eta() {
        // A cached run reports a huge audio/asr ratio; learning it would leave
        // every later estimate at zero, which shows as a permanent "finishing".
        assert_eq!(App::usable_throughput(3004.33), None);
        assert_eq!(App::usable_throughput(f32::INFINITY), None);
        assert_eq!(App::usable_throughput(0.0), None);
        assert_eq!(App::usable_throughput(12.5), Some(12.5));

        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-implausible");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.work_done = 10.0;
        app.work_total = 100.0;
        // A value already saved by an older build is ignored rather than used:
        // the phase still has a view, but no number.
        app.eta_prior = Some(3004.33);
        let view = app.estimate_remaining().expect("a phase");
        assert_eq!(view.remaining, None);
    }

    #[test]
    fn the_wall_clock_is_never_dropped_for_the_eta() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("clock-config");

        let mut app = pro_app();
        app.status = Status::Running;
        app.run_start = Some(Instant::now() - Duration::from_secs(42));

        // No estimate yet: elapsed only.
        app.eta_shown = None;
        let text = app.timer_text();
        assert!(text.starts_with("0:4"), "elapsed missing: {text}");
        assert!(!text.contains('\u{00b7}'));

        // With an estimate the elapsed time still leads the string.
        app.stage_id = "transcription".to_string();
        app.eta_shown = Some(EtaView {
            phase: EtaPhase::Transcribing,
            remaining: Some(900.0),
        });
        let text = app.timer_text();
        assert!(text.starts_with("0:4"), "elapsed dropped: {text}");
        assert!(text.contains("min left"), "estimate missing: {text}");
    }

    #[test]
    fn filled_buttons_dim_when_disabled() {
        let scheme = Scheme::dark();
        let theme = AppTheme::Dark.iced();
        let style = color_button(scheme.green, scheme.on_accent);
        let active = style(&theme, button::Status::Active);
        let disabled = style(&theme, button::Status::Disabled);

        // The label loses its strength, so the button reads as unavailable.
        assert!(active.text_color.a > 0.9, "{}", active.text_color.a);
        assert!(disabled.text_color.a < 0.7, "{}", disabled.text_color.a);
        assert_ne!(disabled.text_color, active.text_color);
    }

    #[test]
    fn default_zoom_is_seventy_percent() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("zoom-default");
        let mut app = App::new();
        assert!((app.font_scale - 0.7).abs() < 0.001, "{}", app.font_scale);
        let _ = app.update(Message::ZoomIn);
        let _ = app.update(Message::ZoomOut);
        let _ = app.update(Message::ZoomReset);
        assert!((app.font_scale - 0.7).abs() < 0.001, "{}", app.font_scale);
    }

    #[test]
    fn rename_lists_speakers_in_numeric_order() {
        let mut names = vec![
            "SPEAKER 3".to_string(),
            "Alice".to_string(),
            "SPEAKER 2".to_string(),
            "SPEAKER 10".to_string(),
            "SPEAKER 1".to_string(),
        ];
        sort_speaker_names(&mut names);
        assert_eq!(
            names,
            vec![
                "SPEAKER 1".to_string(),
                "SPEAKER 2".to_string(),
                "SPEAKER 3".to_string(),
                "SPEAKER 10".to_string(),
                "Alice".to_string(),
            ]
        );
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

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
        app.speaker_filter = Some("SPEAKER 1".to_string());
        app.invalidate_derived();

        let order = app.display_order();
        assert_eq!(order.len(), 2);
        assert!(order
            .iter()
            .all(|index| app.shown_lines()[*index].speaker == "SPEAKER 1"));

        // Clearing the filter keeps the current scroll position.
        app.scroll_target = Some(321.0);
        app.speaker_filter = None;
        app.invalidate_derived();
        assert_eq!(app.scroll_target, Some(321.0));

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
        let mut app = pro_app();
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

        let mut app = pro_app();
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

        open_test_item(&mut app, 0);
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

        let mut app = pro_app();
        app.queue.push(make_item("old.mp3"));
        app.queue.push(make_item("first.mp3"));
        app.queue.push(make_item("second.mp3"));
        app.batch_paths = vec![media.join("first.mp3"), media.join("second.mp3")];

        let files: Vec<String> = app
            .stats_lines()
            .into_iter()
            .filter_map(|line| match line {
                SummaryLine::File(name) => Some(name),
                SummaryLine::Body(_) | SummaryLine::Table(..) => None,
            })
            .collect();
        assert_eq!(files, vec!["first.mp3", "second.mp3"]);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn summary_tabulates_every_run_when_the_queue_held_more_than_one() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("batch-summary-table-config");
        let media = temp_dir("batch-summary-table-media");

        let make_item = |name: &str, audio: f64, run: f64, words: usize| {
            let path = media.join(name);
            std::fs::write(&path, b"audio").unwrap();
            let mut stats = backend::Stats {
                audio_seconds: audio,
                run_seconds: run,
                speakers: 2,
                ..Default::default()
            };
            stats
                .speaker_stats
                .insert("A".to_string(), backend::SpeakerStat { seconds: audio as f32, segments: 3, words });
            QueueItem {
                path: path.clone(),
                audio: Some(path),
                status: ItemStatus::Done,
                duration: Some(audio),
                transcript: None,
                stats: Some(stats),
                turns: Vec::new(),
                peaks: Arc::new(Vec::new()),
            }
        };

        let mut app = pro_app();
        app.queue.push(make_item("first.mp3", 60.0, 30.0, 100));
        app.queue.push(make_item("second.mp3", 120.0, 40.0, 250));
        app.batch_paths = vec![media.join("first.mp3"), media.join("second.mp3")];

        let lines = app.stats_lines();
        let Some(SummaryLine::Table(headers, rows)) = lines.first() else {
            panic!("expected the cross-run table first");
        };
        assert_eq!(headers.first().map(String::as_str), Some("File"));
        // One row per file plus the total.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], "first.mp3");
        assert_eq!(rows[2][0], "2 files");
        assert_eq!(rows[2][1], "3:00");
        assert_eq!(rows[2][3], "2.6\u{00d7}");
        assert_eq!(rows[2][5], "350");

        // A single file keeps the plain per-file block, with no table.
        app.queue.remove(1);
        app.batch_paths = vec![media.join("first.mp3")];
        assert!(!app
            .stats_lines()
            .iter()
            .any(|line| matches!(line, SummaryLine::Table(..))));

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

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
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
        open_test_item(&mut app, 0);
        assert!(app.view_error.is_some());
        assert!(app.shown_lines().is_empty());
    }

    #[test]
    fn the_editor_strip_is_empty_until_a_line_is_edited() {
        // Closed: nothing between the header and the list, so the list runs up to
        // the header.
        assert_eq!(editor_strip_height(false), 0.0);
        // Open: the strip's own box, with no gap on either side of it: it is a band
        // inside the card now, not an island between cards.
        assert_eq!(editor_strip_height(true), LINE_EDIT_PANEL_HEIGHT);
    }

    #[test]
    fn the_strip_shifts_the_content_back_under_the_pointer() {
        // Opening: the offset grows by the strip's height so the content holds still.
        assert_eq!(compensate_offset(200.0, LINE_EDIT_PANEL_HEIGHT, f32::MAX), 238.0);
        // The edited row's top caps it: past that the field would sit under the strip.
        assert_eq!(compensate_offset(200.0, LINE_EDIT_PANEL_HEIGHT, 210.0), 210.0);
        // Closing: the offset shrinks by the strip's height, and stops at the top of
        // the document — where the content simply starts lower instead.
        assert_eq!(compensate_offset(200.0, -LINE_EDIT_PANEL_HEIGHT, f32::MAX), 162.0);
        assert_eq!(compensate_offset(20.0, -LINE_EDIT_PANEL_HEIGHT, f32::MAX), 0.0);
    }

    #[test]
    fn opening_the_editor_shifts_the_scroll_instead_of_the_row() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("strip-scroll-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n[00:09] SPEAKER 1: three\n[00:12] SPEAKER 2: four\n";
        let (mut app, _transcript, media) = line_edit_fixture("strip-scroll", original);

        // Four 400 px rows in a 400 px pane; the second line is centred at 400.
        app.transcript_content = 1600.0;
        app.transcript_viewport = 400.0;
        app.pane_row_height.set(Some(24.0));
        app.transcript_offset = 300.0;
        app.scroll_target = None;

        // Opening the editor inserts three strip-height px above the list, so the
        // offset takes them back: the content does not move on screen.
        let _ = app.update(Message::EditLine(1));
        assert!(app.line_edit.is_some());
        assert_eq!(
            app.scroll_target,
            Some(300.0 + LINE_EDIT_PANEL_HEIGHT),
            "opening must shift the list back under the row"
        );

        // Closing gives the height back and the offset goes with it.
        app.transcript_offset = 300.0 + LINE_EDIT_PANEL_HEIGHT;
        app.scroll_target = None;
        let _ = app.update(Message::CancelLineEdit);
        assert_eq!(app.scroll_target, Some(300.0));

        // A row within the strip's height of the list's top cannot hold still: it
        // would go under the strip, so the shift stops where that row's top meets the
        // strip's bottom edge.
        app.transcript_offset = 560.0;
        app.scroll_target = None;
        let (top, _) = app.edited_row_place(1).expect("the pane is measured");
        assert!(top < LINE_EDIT_PANEL_HEIGHT, "the row should be in the band");
        let _ = app.update(Message::EditLine(1));
        assert_eq!(app.scroll_target, Some(560.0 + top));

        // At the top of the document there is nothing further up to shift in: closing
        // stops at zero and the content simply starts lower.
        app.transcript_offset = 20.0;
        app.scroll_target = None;
        let _ = app.update(Message::CancelLineEdit);
        assert_eq!(app.scroll_target, Some(0.0));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_bubble_word_press_reaches_the_edit_path() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("bubble-word-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("bubble-word", original);

        // This is what the wrapped words send. iced's rich text swallows a press
        // over a glyph before the row's `mouse_area` sees it, so the bubble's
        // words carry `TextPress` exactly as line mode's do.
        app.sms_mode = true;
        let point = Point::new(12.0, 6.0);
        let _ = app.update(Message::LineTextPressed(1, point));
        assert!(app.line_edit.is_none(), "one click must not open the field");
        assert_eq!(app.playhead, 5.0, "the first click still seeks");

        let _ = app.update(Message::LineTextPressed(1, point));
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(1));
        assert_eq!(
            app.line_edit_caret,
            Some(point),
            "the caret lands where the press did, as it does in line mode"
        );

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn an_exit_never_clears_a_newer_hover() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("hover-order-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("hover-order", original);

        // Moving up a row enqueues the entering row's `Some(i)` and the leaving
        // row's exit in the same update. A bare `None` on exit wiped the newer
        // hover, and the row could not re-enter while the pointer stayed on it,
        // so its swap button disappeared until the pointer left and returned.
        let _ = app.update(Message::LineHover(Some(1)));
        let _ = app.update(Message::LineUnhover(0));
        assert_eq!(app.transcript_line_hover, Some(1));

        let _ = app.update(Message::LineUnhover(1));
        assert_eq!(app.transcript_line_hover, None);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_double_click_on_a_bubble_opens_the_editor() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("bubble-edit-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("bubble-edit", original);

        // This is the message a bubble row sends. In SMS mode the bubble is the
        // thing to point at, so its press has to behave like a line row's: seek on
        // the first click, edit on the second.
        app.sms_mode = true;
        app.playhead = 0.0;
        let _ = app.update(Message::LineClicked(1));
        assert!(app.line_edit.is_none(), "one click must not open the field");
        assert_eq!(app.playhead, 5.0, "the first click still seeks");

        let _ = app.update(Message::LineClicked(1));
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(1));

        // Escape still cancels and leaves the line alone.
        let _ = app.update(Message::Escape);
        assert!(app.line_edit.is_none());
        assert_eq!(line_text(&app.edit_lines()[1]), "two");

        // A press on another bubble while editing commits the open one and moves
        // the editor there, exactly as a line row does.
        let _ = app.update(Message::LineClicked(0));
        let _ = app.update(Message::LineClicked(0));
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(0));
        let _ = app.update(Message::LineClicked(1));
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(1));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn right_click_flips_and_double_click_edits_a_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("flip-speaker");
        let media = temp_dir("flip-speaker-media");
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello\n[00:05] SPEAKER 2: hi\n[00:09] SPEAKER 1: bye\n",
        )
        .unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(10.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        let _ = app.update(Message::FlipLineSpeaker(1));
        assert_eq!(app.shown_lines()[1].speaker, "SPEAKER 1");
        // Only the clicked line moves; its neighbours keep their speakers.
        assert_eq!(app.shown_lines()[0].speaker, "SPEAKER 1");
        assert_eq!(app.shown_lines()[2].speaker, "SPEAKER 1");
        assert!(app.modified);
        let written = std::fs::read_to_string(&transcript).unwrap();
        assert!(written.contains("[00:05] SPEAKER 1: hi"));
        assert!(written.contains("[00:09] SPEAKER 1: bye"));

        // The swap joined the same undo stack as a text edit.
        assert!(app.line_undo_available());
        let _ = app.update(Message::UndoLineEdit);
        assert_eq!(app.shown_lines()[1].speaker, "SPEAKER 2");
        assert!(std::fs::read_to_string(&transcript)
            .unwrap()
            .contains("[00:05] SPEAKER 2: hi"));
        let _ = app.update(Message::RedoLineEdit);
        assert_eq!(app.shown_lines()[1].speaker, "SPEAKER 1");

        // Double-click opens the inline editor even while Follow is on: the
        // editor turns Follow off itself, which is what the earlier gate got
        // wrong -- with Follow on by default, double-click did nothing at all.
        app.follow_playhead = true;
        let _ = app.update(Message::LineClicked(0));
        let _ = app.update(Message::LineClicked(0));
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(0));
        assert!(!app.follow_playhead);
        let _ = app.update(Message::CancelLineEdit);

        app.follow_playhead = false;
        let _ = app.update(Message::LineClicked(2));
        assert!(app.line_edit.is_none(), "a single click must not open it");
        let _ = app.update(Message::LineClicked(2));
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(2));

        // Entering the editor turns Follow off.
        let _ = app.update(Message::CancelLineEdit);
        app.follow_playhead = true;
        let _ = app.update(Message::EditLine(0));
        assert!(app.line_edit.is_some());
        assert!(!app.follow_playhead);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn reset_warns_before_reverting_to_the_initial_transcript() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("reset-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, transcript, media) = line_edit_fixture("reset", original);

        open_line_edit(&mut app, 0);
        current_row_mut(&mut app).content = text_editor::Content::with_text("changed");
        let _ = app.update(Message::DoneLineEdit);
        assert!(app.modified, "the edit should mark the transcript modified");
        assert!(app.reset_available(), "the pre-edit copy should exist");

        // The request only warns; nothing is reverted until it is confirmed.
        let _ = app.update(Message::RequestResetTranscript);
        assert!(app.reset_confirm);
        assert!(app.popup_open(), "the warning is a modal");
        assert!(app.shown_transcript().contains("changed"));

        // Cancelling leaves the edit alone.
        let _ = app.update(Message::CloseResetConfirm);
        assert!(!app.reset_confirm);
        assert!(app.shown_transcript().contains("changed"));

        // Confirming reverts the document and the file.
        let _ = app.update(Message::RequestResetTranscript);
        let _ = app.update(Message::ConfirmResetTranscript);
        assert!(!app.reset_confirm);
        assert!(!app.modified);
        assert_eq!(app.shown_transcript(), original);
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), original);

        // It is one undo step, like any other edit.
        let _ = app.update(Message::UndoLineEdit);
        assert!(app.shown_transcript().contains("changed"));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_transcribe_button_says_retranscribe_for_a_finished_item() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("retranscribe-label");
        let original = "[00:00] SPEAKER 1: one\n";
        let (mut app, _transcript, media) = line_edit_fixture("retranscribe", original);

        // The fixture opens its only item, which is Done and selected.
        assert_eq!(app.transcribe_label(), "Retranscribe");

        app.queue.push(QueueItem {
            path: media.join("queued.mp3"),
            audio: None,
            status: ItemStatus::Pending,
            duration: Some(1.0),
            transcript: None,
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.1; 8]),
        });
        assert_eq!(
            app.transcribe_label(),
            "Transcribe",
            "a pending item means Start will run the queue"
        );

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_transcribe_button_opens_a_setup_dialog() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("run-setup-dialog");
        let original = "[00:00] SPEAKER 1: one\n";
        let (mut app, _transcript, media) = line_edit_fixture("run-setup", original);

        let _ = app.update(Message::OpenRunSetup);
        assert!(app.run_setup_open);
        assert!(app.popup_open(), "the setup is modal");
        let _ = app.view();

        let _ = app.update(Message::Escape);
        assert!(!app.run_setup_open);

        let _ = app.update(Message::OpenRunSetup);
        let _ = app.update(Message::CloseRunSetup);
        assert!(!app.run_setup_open);

        // Starting closes the dialog; the run itself is gated on models.
        let _ = app.update(Message::OpenRunSetup);
        let _ = app.update(Message::ConfirmRunSetup);
        assert!(!app.run_setup_open);

        let _ = std::fs::remove_dir_all(&media);
    }

    /// Every dialog, so a variant added to `Modal` cannot go untested: the
    /// escape and render passes below each walk this list.
    const ALL_MODALS: [Modal; 9] = [
        Modal::Reset,
        Modal::Summary,
        Modal::DeleteLine,
        Modal::Calibration,
        Modal::Paywall,
        Modal::Settings,
        Modal::Help,
        Modal::RunSetup,
        Modal::Rename,
    ];

    /// Raise one dialog the way the app would.
    fn raise_modal(app: &mut App, modal: Modal) {
        match modal {
            Modal::Reset => app.reset_confirm = true,
            Modal::Summary => app.summary_open = true,
            Modal::DeleteLine => {
                // The confirmation previews the line being edited, so it needs
                // an editor to preview.
                let _ = app.update(Message::EditLine(1));
                app.delete_confirm = true;
            }
            Modal::Calibration => {
                app.calibration = Some(Calibration::new(Preset::Accuracy, false))
            }
            Modal::Paywall => app.paywall_open = true,
            Modal::Settings => app.settings_open = true,
            Modal::Help => app.help_open = Some(HelpKind::About),
            Modal::RunSetup => app.run_setup_open = true,
            Modal::Rename => app.rename_open = true,
            Modal::Vocabulary => {
                let _ = app.open_vocabulary();
            }
            Modal::Briefing => app.briefing = Some(BriefingState::NeedsModel),
        }
    }

    #[test]
    fn escape_closes_every_open_modal() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("escape-modals");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("escape-modals", original);

        for modal in ALL_MODALS {
            raise_modal(&mut app, modal);
            assert_eq!(app.open_modal(), Some(modal), "{modal:?} did not open");
            assert!(app.popup_open(), "{modal:?} is not a popup");
            let _ = app.update(Message::Escape);
            assert_eq!(app.open_modal(), None, "{modal:?} survived Escape");
        }

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn every_modal_dialog_builds() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("modal-render");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("modal-render", original);

        for modal in ALL_MODALS {
            raise_modal(&mut app, modal);
            let _ = app.view();
        }

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_editing_tips_follow_the_summary_before_the_rename() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("tips-chain");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("tips-chain", original);

        app.summary_open = true;
        app.pending_tips = true;
        app.pending_rename = true;

        // Summary -> tips -> rename, in that order, each closing handing on.
        let _ = app.update(Message::CloseSummary);
        assert_eq!(app.help_open, Some(HelpKind::EditingTips));
        assert!(!app.rename_open);

        let _ = app.update(Message::CloseHelp);
        assert_eq!(app.help_open, None);
        assert!(app.rename_open);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn ticking_the_tips_box_skips_them_and_still_renames() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("tips-off");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("tips-off", original);

        let _ = app.update(Message::SetEditingTips(false));
        assert!(!app.editing_tips);

        app.summary_open = true;
        app.pending_tips = true;
        app.pending_rename = true;
        let _ = app.update(Message::CloseSummary);
        assert_eq!(app.help_open, None, "the card was turned off");
        assert!(app.rename_open, "the rename still gets its turn");

        // The next completion does not queue the card at all.
        app.rename_open = false;
        app.pending_rename = false;
        assert!(app.present_completion());
        assert!(!app.pending_tips);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_tips_are_still_reachable_from_the_help_menu_when_suppressed() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("tips-help");
        let original = "[00:00] SPEAKER 1: one\n";
        let (mut app, _transcript, media) = line_edit_fixture("tips-help", original);

        let _ = app.update(Message::SetEditingTips(false));
        let _ = app.update(Message::HelpChoice(HelpKind::EditingTips));
        assert_eq!(app.help_open, Some(HelpKind::EditingTips));
        let _ = app.view();

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_editing_tips_setting_round_trips_through_settings() {
        let _guard = env_lock();
        let _dir = ConfigEnv::new("editing-tips");

        let mut app = pro_app();
        assert!(app.editing_tips, "a fresh profile shows the tips");

        let _ = app.update(Message::SetEditingTips(false));
        let reloaded = pro_app();
        assert!(!reloaded.editing_tips);
    }


    #[test]
    fn the_editor_strip_can_open_the_speaker_rename() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("strip-rename");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("strip-rename", original);

        open_line_edit(&mut app, 1);
        // The strip's Rename Speakers button sends the same message as Edit ->
        // Rename Speakers: the typed row is committed before the form takes over.
        let _ = app.update(Message::OpenRename);
        assert!(app.line_edit.is_none(), "the edit is committed, not lost");
        assert!(app.rename_open);
        assert_eq!(app.rename_fields.len(), 2);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn next_speaker_name_counts_from_the_highest_number() {
        fn names(list: &[&str]) -> Vec<String> {
            list.iter().map(|name| name.to_string()).collect()
        }

        // The usual case: two detected voices, so the next is the third.
        assert_eq!(
            next_speaker_name(&names(&["SPEAKER 1", "SPEAKER 2"])),
            "SPEAKER 3"
        );
        // Gaps are not reused: a removed voice's number stays retired.
        assert_eq!(
            next_speaker_name(&names(&["SPEAKER 1", "SPEAKER 3"])),
            "SPEAKER 4"
        );
        // A hand-renamed voice keeps its slot in the count.
        assert_eq!(
            next_speaker_name(&names(&["Alice", "SPEAKER 2"])),
            "SPEAKER 3"
        );
        assert_eq!(
            next_speaker_name(&names(&["SPEAKER 10", "Bob"])),
            "SPEAKER 11"
        );
        // Nothing is numbered: one past the count.
        assert_eq!(
            next_speaker_name(&names(&["Alice", "Bob"])),
            "SPEAKER 3"
        );
        assert_eq!(next_speaker_name(&[]), "SPEAKER 1");
    }

    #[test]
    fn the_editor_strip_adds_a_speaker_to_the_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("strip-add-speaker");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, transcript, media) = line_edit_fixture("strip-add-speaker", original);

        open_line_edit(&mut app, 1);
        let _ = app.update(Message::AddLineSpeaker);
        assert_eq!(current_row_mut(&mut app).speaker, "SPEAKER 3");
        assert_eq!(
            app.toast.as_ref().map(|(message, _)| message.as_str()),
            Some("Speaker: SPEAKER 3")
        );
        // The new voice is in the order straight away, so the chip has a colour
        // and the cycle has somewhere to go.
        assert_eq!(
            app.speaker_order(),
            vec![
                "SPEAKER 1".to_string(),
                "SPEAKER 2".to_string(),
                "SPEAKER 3".to_string()
            ]
        );
        let _ = app.update(Message::CycleLineSpeaker);
        assert_eq!(current_row_mut(&mut app).speaker, "SPEAKER 1");

        // A second press hands out the next number, not the same one again.
        let _ = app.update(Message::AddLineSpeaker);
        assert_eq!(current_row_mut(&mut app).speaker, "SPEAKER 4");

        // Saving writes it into the transcript, which is also the sidecar's
        // input, so it exports with the rest.
        let _ = app.update(Message::DoneLineEdit);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("SPEAKER 4: two"), "{saved}");
        assert_eq!(app.shown_lines()[1].speaker, "SPEAKER 4");

        // Cancelling drops the name it added with the rest of the edit.
        open_line_edit(&mut app, 0);
        let _ = app.update(Message::AddLineSpeaker);
        assert!(app.speaker_order().iter().any(|name| name == "SPEAKER 5"));
        let _ = app.update(Message::CancelLineEdit);
        assert!(!app.speaker_order().iter().any(|name| name == "SPEAKER 5"));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_dialog_keeps_the_pointer_off_the_queue() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("queue-behind-dialog");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("queue-dialog", original);

        let _ = app.update(Message::QueueHover(Some(0)));
        assert_eq!(app.queue_hover, Some(0));

        app.summary_open = true;
        let _ = app.update(Message::Noop);
        assert_eq!(app.queue_hover, None, "no row lights up under the scrim");

        // Moving across the queue behind the summary changes nothing.
        let _ = app.update(Message::QueueHover(Some(1)));
        assert_eq!(app.queue_hover, None);
        let _ = app.view();

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_dialog_drops_the_hovered_row_and_refuses_new_hover() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("hover-behind-dialog");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("hover-dialog", original);

        let _ = app.update(Message::LineHover(Some(0)));
        assert_eq!(app.transcript_line_hover, Some(0));

        app.settings_open = true;
        let _ = app.update(Message::Noop);
        assert_eq!(app.transcript_line_hover, None, "no chrome under a scrim");

        let _ = app.update(Message::LineHover(Some(1)));
        assert_eq!(app.transcript_line_hover, None);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_front_modal_is_the_one_escape_closes() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("modal-precedence");
        let original = "[00:00] SPEAKER 1: one\n";
        let (mut app, _transcript, media) = line_edit_fixture("modal-precedence", original);

        // Two dialogs can be set at once in edge cases. `open_modal` states the
        // order once, so the dialog drawn is the dialog Escape closes rather
        // than two separate chains disagreeing.
        app.summary_open = true;
        app.settings_open = true;
        assert_eq!(app.open_modal(), Some(Modal::Summary));
        let _ = app.view();

        let _ = app.update(Message::Escape);
        assert_eq!(app.open_modal(), Some(Modal::Settings));
        let _ = app.update(Message::Escape);
        assert_eq!(app.open_modal(), None);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn closing_the_line_editor_drops_its_delete_confirmation() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("stale-delete");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("stale-delete", original);

        open_line_edit(&mut app, 1);
        let _ = app.update(Message::DeleteLineEdit);
        assert!(app.delete_confirm);

        // The confirmation belongs to the line being edited. Cancelling the
        // editor must not leave a modal flag behind with nothing to draw.
        let _ = app.update(Message::CancelLineEdit);
        assert!(!app.delete_confirm);
        assert!(!app.popup_open());
        let _ = app.view();

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn ctrl_f_reveals_the_find_row_and_escape_hides_it() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("find-row-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("find-row", original);

        assert!(!app.find_open, "the row starts hidden");
        let _ = app.update(Message::FocusFind);
        assert!(app.find_open, "Ctrl+F must reveal the row");
        assert!(app.find_focus_pending, "focus waits for the first layout");

        app.search_query = "one".to_string();
        let _ = app.update(Message::Escape);
        assert!(!app.find_open);
        assert!(
            app.search_query.is_empty(),
            "closing the row clears what it was searching"
        );

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_find_row_closes_when_its_field_loses_focus() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("find-focus-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("find-focus", original);

        let _ = app.update(Message::FocusFind);
        // The frame that mounts the row clears this; the poll is off until then.
        app.find_focus_pending = false;

        // The pointer being over the row keeps it alive: next/prev are clicks
        // outside the field.
        app.find_hover = true;
        let _ = app.update(Message::FindFocusPolled(false));
        assert!(app.find_open, "the row closed under the pointer");

        app.find_hover = false;
        let _ = app.update(Message::FindFocusPolled(false));
        assert!(!app.find_open, "losing focus must hide the row");

        // A focused field keeps it open.
        let _ = app.update(Message::FocusFind);
        app.find_focus_pending = false;
        let _ = app.update(Message::FindFocusPolled(true));
        assert!(app.find_open);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_view_menu_keys_toggle_their_toggles_outside_text_fields() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("view-keys-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("view-keys", original);

        // A focused field owns every one of them.
        let timestamps = app.show_timestamps;
        let _ = app.update(Message::ViewShortcutChecked(ViewShortcut::Timestamps, true));
        assert_eq!(app.show_timestamps, timestamps);

        let _ = app.update(Message::ViewShortcutChecked(ViewShortcut::Timestamps, false));
        assert_ne!(app.show_timestamps, timestamps);

        let sms = app.sms_mode;
        let _ = app.update(Message::ViewShortcutChecked(ViewShortcut::Sms, false));
        assert_ne!(app.sms_mode, sms);

        let join = app.join_lines;
        let _ = app.update(Message::ViewShortcutChecked(ViewShortcut::Join, false));
        assert_ne!(app.join_lines, join);

        // No confidence sidecar here, so the key and the menu row both stay inert.
        let confidence = app.confidence_view;
        let _ = app.update(Message::ViewShortcutChecked(ViewShortcut::Confidence, false));
        assert_eq!(app.confidence_view, confidence);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_join_preview_rewrites_the_shown_document_not_the_editable_one() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("shown-doc");
        let media = temp_dir("shown-doc-media");
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: one\n\
             [00:02] SPEAKER 1: two\n\
             [00:09] SPEAKER 2: three\n",
        )
        .unwrap();

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);

        // No run filled the live buffer: the screen is showing the item, and
        // both the shown and the editable document are that item's lines.
        assert!(app.lines.is_empty());
        assert_eq!(app.shown_lines().len(), 3);
        assert_eq!(app.document_lines().len(), 3);
        assert_eq!(app.edit_lines().len(), 3);

        // Join rewrites what is shown. The document underneath — and so the
        // editor and the file — keeps its own lines.
        let _ = app.update(Message::ToggleJoinLines);
        assert_eq!(app.shown_lines().len(), 2);
        assert_eq!(app.document_lines().len(), 3);
        assert_eq!(app.edit_lines().len(), 3);
        assert!(app.shown_transcript().contains("one two"));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_f_key_toggles_follow_only_outside_a_text_field() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-key-config");
        let original = "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n";
        let (mut app, _transcript, media) = line_edit_fixture("follow-key", original);

        let before = app.follow_playhead;
        // A focused field owns the key: the checked form does nothing.
        let _ = app.update(Message::ViewShortcutChecked(ViewShortcut::Follow, true));
        assert_eq!(app.follow_playhead, before);
        let _ = app.update(Message::ViewShortcutChecked(ViewShortcut::Follow, false));
        assert_ne!(app.follow_playhead, before);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn joining_adjacent_lines_and_save_as() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("join-config");
        let media = temp_dir("join-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: hello there\n\
             [00:05] SPEAKER 1: hello again\n\
             [00:07] SPEAKER 2: hi\n",
        )
        .unwrap();

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
        assert_eq!(app.shown_lines().len(), 3);

        let _ = app.update(Message::ToggleJoinLines);
        assert!(app.join_lines);
        assert!(app.modified);
        assert_eq!(app.shown_lines().len(), 2);
        assert!(app
            .shown_transcript()
            .contains("[00:00] SPEAKER 1: hello there hello again"));

        let joined = media.join("talk-joined.txt");
        let _ = app.update(Message::SaveTranscriptAsPicked(Some(joined.clone())));
        let saved = std::fs::read_to_string(&joined).unwrap();
        assert!(saved.contains("SPEAKER 1: hello there hello again"));
        assert!(!app.modified);

        let _ = app.update(Message::ToggleJoinLines);
        assert!(!app.join_lines);
        assert_eq!(app.shown_lines().len(), 3);

        let _ = std::fs::remove_dir_all(&media);
    }

    fn export_fixture(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let media = temp_dir(tag);
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(&transcript, "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n").unwrap();
        std::fs::write(
            media.join("talk.json"),
            r#"{
                "lines": [
                    {"time": "00:00", "speaker": "SPEAKER 1", "text": "one", "start": 0.0, "confidence": -0.2, "end": 2.0},
                    {"time": "00:05", "speaker": "SPEAKER 2", "text": "two", "start": 5.0, "confidence": -0.2, "end": 9.0}
                ],
                "turns": [],
                "duration": 10.0
            }"#,
        )
        .unwrap();
        (media, transcript)
    }

    #[test]
    fn save_as_srt_uses_the_sidecar_bounds() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("srt-config");
        let (media, transcript) = export_fixture("srt-media");

        let mut app = pro_app();
        let audio = media.join("talk.mp3");
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
        open_test_item(&mut app, 0);

        let out = media.join("talk.srt");
        let _ = app.update(Message::SaveTranscriptAsPicked(Some(out.clone())));
        let saved = std::fs::read_to_string(&out).unwrap();
        // Not the next line's 5.0: the sidecar knows this cue ends at 2.0.
        assert!(saved.contains("00:00:00,000 --> 00:00:02,000"), "{saved}");
        assert!(saved.contains("00:00:05,000 --> 00:00:09,000"), "{saved}");

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn save_as_refuses_to_overwrite_the_sidecar() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("sidecar-config");
        let (media, transcript) = export_fixture("sidecar-media");

        let mut app = pro_app();
        let audio = media.join("talk.mp3");
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
        open_test_item(&mut app, 0);

        let sidecar = media.join("talk.json");
        let before = std::fs::read_to_string(&sidecar).unwrap();
        let _ = app.update(Message::SaveTranscriptAsPicked(Some(sidecar.clone())));

        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), before);
        assert!(app
            .toast
            .as_ref()
            .is_some_and(|(message, _)| message.contains("metadata")));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn vocabulary_saves_and_reopens_from_both_menus() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("vocab-config");

        let mut app = pro_app();
        // Both the Edit and the Help menu send this message.
        let _ = app.update(Message::OpenVocabulary);
        assert_eq!(app.open_modal(), Some(Modal::Vocabulary));
        assert!(app.vocab_terms.text().trim().is_empty());

        app.vocab_terms = text_editor::Content::with_text("Kubernetes\nACME");
        app.vocab_replacements = text_editor::Content::with_text("pie torch = PyTorch");
        let _ = app.update(Message::SaveVocabulary);
        assert_eq!(app.open_modal(), None);

        let stored = Vocabulary::load();
        assert_eq!(stored.terms, vec!["Kubernetes", "ACME"]);
        assert_eq!(stored.replacements.len(), 1);
        assert_eq!(stored.replacements[0].to, "PyTorch");

        // Reopening shows what was saved.
        let _ = app.update(Message::OpenVocabulary);
        assert_eq!(app.vocab_terms.text().trim_end(), "Kubernetes\nACME");
        assert_eq!(
            app.vocab_replacements.text().trim_end(),
            "pie torch = PyTorch"
        );
        let _ = app.update(Message::CloseVocabulary);
        assert_eq!(app.open_modal(), None);
    }

    #[test]
    fn command_enter_saves_the_vocabulary_instead_of_starting() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("vocab-enter");

        let mut app = pro_app();
        let _ = app.update(Message::OpenVocabulary);
        app.vocab_terms = text_editor::Content::with_text("Kubernetes");
        app.vocab_replacements = text_editor::Content::with_text("");

        // Cmd/Ctrl+Enter is `Message::Start`; with the editor up it must save,
        // not start a run behind the dialog.
        let _ = app.update(Message::Start);

        assert!(!app.vocabulary_open);
        assert_eq!(Vocabulary::load().terms, vec!["Kubernetes"]);
    }

    #[test]
    fn a_malformed_replacement_keeps_the_dialog_open() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("vocab-bad");

        let mut app = pro_app();
        let _ = app.update(Message::OpenVocabulary);
        app.vocab_replacements = text_editor::Content::with_text("acme corp");
        let _ = app.update(Message::SaveVocabulary);

        assert!(app.vocabulary_open);
        assert!(app
            .toast
            .as_ref()
            .is_some_and(|(message, _)| message.contains("heard = shown")));
    }

    fn interrupted_item(audio: PathBuf) -> QueueItem {
        QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Interrupted,
            duration: Some(10.0),
            transcript: None,
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn an_interrupted_row_restores_as_interrupted() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("interrupt-config");
        let media = temp_dir("interrupt-media");
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            status: ItemStatus::Running,
            ..interrupted_item(audio.clone())
        });
        app.save_queue();

        let (loaded, _) = App::load_queue_and_batch();
        assert!(matches!(loaded[0].status, ItemStatus::Interrupted));
    }

    #[test]
    fn an_interrupted_row_offers_resume_and_skip() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("resume-config");
        let media = temp_dir("resume-media");
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();

        let mut app = pro_app();
        app.queue.push(interrupted_item(audio.clone()));
        assert_eq!(app.transcribe_label(), "Resume");

        let parked = hippo_transcribe_core::pipeline::checkpoint_path(&audio);
        std::fs::write(&parked, "[00:00] SPEAKER 1: hi\n").unwrap();

        let _ = app.update(Message::SkipItem(0));

        assert!(matches!(app.queue[0].status, ItemStatus::Failed(_)));
        assert!(!parked.exists());
        assert_eq!(app.transcribe_label(), "Transcribe");
    }

    #[test]
    fn the_microphone_choice_is_remembered() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("mic-config");

        let mut app = pro_app();
        let _ = app.update(Message::InputDeviceChanged("USB Microphone".to_string()));
        assert_eq!(app.input_device.as_deref(), Some("USB Microphone"));
        assert_eq!(
            App::load_settings().input_device.as_deref(),
            Some("USB Microphone")
        );

        let _ = app.update(Message::InputDeviceChanged(
            SYSTEM_DEFAULT_DEVICE.to_string(),
        ));
        assert_eq!(app.input_device, None);
        assert_eq!(App::load_settings().input_device, None);
    }

    #[test]
    fn stopping_without_a_recording_is_harmless() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("mic-idle");

        let mut app = pro_app();
        let _ = app.update(Message::StopRecording);
        assert!(app.recording.is_none());
    }

    #[test]
    fn karaoke_bands_the_spoken_prefix_without_recolouring_text() {
        let words = vec![
            Word {
                start: 0.0,
                end: 0.5,
                text: "hello".to_string(),
            },
            Word {
                start: 0.5,
                end: 1.0,
                text: "there".to_string(),
            },
        ];
        let runs = karaoke_runs("hello there", &words, 0.6, true).unwrap();
        assert_eq!(runs[0], ("hello".to_string(), true, false));
        assert_eq!(runs[1], (" ".to_string(), true, false));
        assert_eq!(runs[2], ("there".to_string(), false, true));

        // Paused: no current-word underline.
        let paused = karaoke_runs("hello there", &words, 0.6, false).unwrap();
        assert_eq!(paused[2], ("there".to_string(), false, false));

        // The rendered spans keep the text and never change its colour.
        let spans = karaoke_spans(
            "hello there",
            &words,
            0.6,
            Color::BLACK,
            Color::WHITE,
            14.0,
            true,
        )
        .unwrap();
        let rebuilt: String = spans.iter().map(|span| span.text.as_ref()).collect();
        assert_eq!(rebuilt, "hello there");
        assert!(spans.iter().all(|span| span.color == Some(Color::BLACK)));
        assert!(spans[2].underline);
    }

    #[test]
    fn karaoke_handles_cjk_punctuation_and_mismatches() {
        let cjk = vec![
            Word {
                start: 0.0,
                end: 0.4,
                text: "你好".to_string(),
            },
            Word {
                start: 0.4,
                end: 0.8,
                text: "世界".to_string(),
            },
        ];
        let runs = karaoke_runs("你好世界", &cjk, 0.5, true).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], ("你好".to_string(), true, false));
        assert_eq!(runs[1], ("世界".to_string(), false, true));

        let punct = vec![Word {
            start: 0.0,
            end: 0.5,
            text: "there,".to_string(),
        }];
        assert_eq!(
            karaoke_runs("there,", &punct, 0.6, true).unwrap()[0],
            ("there,".to_string(), true, false)
        );

        // Double spaces are part of the gap, not a reason to bail.
        let runs = karaoke_runs("hello  there", &words_for_test(), 2.0, true).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1], ("  ".to_string(), true, false));

        // A document whose words no longer line up falls back to plain spans.
        assert!(karaoke_runs("goodbye", &punct, 0.6, true).is_none());
    }

    fn words_for_test() -> Vec<Word> {
        vec![
            Word {
                start: 0.0,
                end: 0.5,
                text: "hello".to_string(),
            },
            Word {
                start: 1.0,
                end: 1.5,
                text: "there".to_string(),
            },
        ]
    }

    #[test]
    fn karaoke_is_a_remembered_view_option() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("karaoke-config");

        let mut app = pro_app();
        assert!(app.karaoke, "on by default");
        let _ = app.update(Message::KaraokeToggled(false));
        assert!(!app.karaoke);
        assert!(!App::load_settings().karaoke);

        let reloaded = App::new();
        assert!(!reloaded.karaoke);
    }

    #[test]
    fn loudness_normalization_is_remembered() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("normalize-config");

        let mut app = pro_app();
        assert!(app.normalize, "on by default");
        let _ = app.update(Message::NormalizeToggled(false));
        assert!(!app.normalize);
        assert!(!App::load_settings().normalize);

        let reloaded = App::new();
        assert!(!reloaded.normalize);
    }

    #[test]
    fn the_licence_notices_are_embedded() {
        // The store package has no README; Help → Licenses is the only place a
        // user can read the attribution.
        assert!(LICENSES_TEXT.contains("CC BY 4.0"));
        assert!(LICENSES_TEXT.contains("Qwen2.5"));
        assert!(LICENSES_TEXT.contains("Apache-2.0"));
        assert_eq!(HelpKind::Licenses.label(), "Licenses");
    }

    #[test]
    fn a_briefing_without_a_transcript_says_so() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("briefing-empty");

        let mut app = pro_app();
        let _ = app.update(Message::OpenBriefing);

        assert!(app.briefing.is_none());
        assert!(app
            .toast
            .as_ref()
            .is_some_and(|(message, _)| message.contains("Transcribe")));
    }

    #[test]
    fn a_missing_briefing_model_offers_the_download() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("briefing-model");
        let (media, transcript) = export_fixture("briefing-media");

        // Keep the real model directory out of the test's way.
        let models_dir = media.join("models");
        std::env::set_var("HIPPO_TRANSCRIBE_MODELS_DIR", &models_dir);

        let mut app = pro_app();
        let audio = media.join("talk.mp3");
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
        open_test_item(&mut app, 0);

        let _ = app.update(Message::OpenBriefing);

        assert!(matches!(app.briefing, Some(BriefingState::NeedsModel)));
        assert_eq!(app.open_modal(), Some(Modal::Briefing));
        let _ = app.update(Message::CloseBriefing);
        assert!(app.briefing.is_none());

        std::env::remove_var("HIPPO_TRANSCRIBE_MODELS_DIR");
        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_briefing_is_a_pro_feature() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("briefing-free");

        let mut app = App::new();
        let _ = app.update(Message::OpenBriefing);

        assert!(app.briefing.is_none());
        assert!(app
            .toast
            .as_ref()
            .is_some_and(|(message, _)| message.contains("Pro")));
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

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);

        let _ = app.update(Message::EditLine(0));
        {
            let row = current_row_mut(&mut app);
            assert_eq!(row.speaker, "SPEAKER 1");
            assert_eq!(row.content.text().trim_end(), "hello");
            row.speaker = "ALICE".to_string();
            row.content = text_editor::Content::with_text("hello there");
        }
        // The inline editor renders without panicking.
        let _ = app.view();

        let _ = app.update(Message::DoneLineEdit);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("[00:00] ALICE: hello there"));
        assert!(saved.contains("[00:05] SPEAKER 2: hi"));

        // Enter saves the form from the line editor.
        let _ = app.update(Message::EditLine(1));
        current_row_mut(&mut app).content = text_editor::Content::with_text("hi there");
        let _ = app.update(Message::EnterSubmit);
        assert!(app.line_edit.is_none());
        let saved = std::fs::read_to_string(&transcript).unwrap();
        assert!(saved.contains("[00:05] SPEAKER 2: hi there"));

        // Escape closes the editor and drops what was typed.
        let _ = app.update(Message::EditLine(0));
        current_row_mut(&mut app).content = text_editor::Content::with_text("discarded");
        let _ = app.update(Message::Escape);
        assert!(app.line_edit.is_none());
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] ALICE: hello there\n[00:05] SPEAKER 2: hi there\n"
        );

        // Done and Enter commits share one undo history: two steps back to the
        // transcript as it was when it opened.
        let _ = app.update(Message::UndoLineEdit);
        assert!(std::fs::read_to_string(&transcript)
            .unwrap()
            .contains("[00:00] ALICE: hello there"));
        let _ = app.update(Message::UndoLineEdit);
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] SPEAKER 1: hello\n[00:05] SPEAKER 2: hi\n"
        );

        let backup = transcript_backup_path(&transcript);
        assert!(backup.exists());

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

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);
        assert!(app.player_ready());

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn follow_toggle_is_user_controlled_during_a_run() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-config");

        let mut app = pro_app();
        app.status = Status::Running;
        app.follow_playhead = true;
        let _ = app.update(Message::ToggleFollowPlayhead);
        assert!(!app.follow_playhead);
    }

    #[test]
    fn a_fresh_profile_follows_the_system_theme() {
        let _guard = env_lock();
        let _dir = ConfigEnv::new("theme-default");

        assert_eq!(AppTheme::default(), AppTheme::System);
        let app = pro_app();
        assert_eq!(app.theme_mode, AppTheme::System);
    }

    #[test]
    fn the_scrubber_maps_x_to_time() {
        // The whole width is the whole duration, and halfway is half of it.
        assert_eq!(scrub_time_at(0.0, 200.0, 60.0), 0.0);
        assert_eq!(scrub_time_at(100.0, 200.0, 60.0), 30.0);
        assert_eq!(scrub_time_at(200.0, 200.0, 60.0), 60.0);
        // Past either edge clamps rather than seeking out of range.
        assert_eq!(scrub_time_at(-10.0, 200.0, 60.0), 0.0);
        assert_eq!(scrub_time_at(400.0, 200.0, 60.0), 60.0);
        // No duration, or no width yet, means no time.
        assert_eq!(scrub_time_at(100.0, 200.0, 0.0), 0.0);
        assert_eq!(scrub_time_at(100.0, 0.0, 60.0), 0.0);
    }

    #[test]
    fn the_player_caret_collapses_the_waveform() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("player-collapse");
        let mut app = pro_app();

        assert!(!app.player_collapsed, "the waveform starts visible");
        let _ = app.update(Message::TogglePlayerCollapsed);
        assert!(app.player_collapsed);
        // The card still builds with the timeline gone: the transport row is
        // what the collapsed state keeps.
        let _ = app.view();
        let _ = app.update(Message::TogglePlayerCollapsed);
        assert!(!app.player_collapsed);
    }

    #[test]
    fn theme_choice_round_trips_through_settings() {
        let _guard = env_lock();
        let _dir = ConfigEnv::new("theme");

        let mut app = pro_app();
        // A fresh profile follows the desktop rather than forcing light.
        assert_eq!(app.theme_mode, AppTheme::System);

        let _ = app.update(Message::SetTheme(AppTheme::Dark));
        assert_eq!(app.theme_mode, AppTheme::Dark);

        let reloaded = pro_app();
        assert_eq!(reloaded.theme_mode, AppTheme::Dark);
    }

    #[test]
    fn gpu_failure_turns_acceleration_off_and_persists() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("gpu-failure");
        let mut app = pro_app();
        let preset = app.preset.arg().to_string();
        app.gpu_profiles.insert(
            preset,
            GpuProfile {
                tested: true,
                selected: true,
                ..GpuProfile::default()
            },
        );
        app.save_settings();

        let marker = App::gpu_guard_path().expect("config dir");
        std::fs::write(&marker, "123").unwrap();
        app.events.lock().unwrap().push_back(Event::Error {
            message: "Vulkan device lost".to_string(),
        });
        let _ = app.drain_events();

        assert!(!app.gpu_profile().unwrap().selected);
        assert!(!marker.exists());
        assert!(app
            .console_lines
            .iter()
            .any(|line| line.contains("Vulkan device lost")));
        assert!(app.toast.is_some());

        let reloaded = pro_app();
        assert!(!reloaded.gpu_profile().unwrap().selected);
    }

    #[test]
    fn calibration_events_store_a_per_preset_multiplier() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("gpu-calibration");
        let mut app = pro_app();
        app.preset = Preset::Accuracy;
        app.calibration = Some(Calibration::new(Preset::Accuracy, false));

        app.calibration_events
            .lock()
            .unwrap()
            .push_back(BenchmarkEvent::CpuFinished { seconds: 2.0 });
        app.drain_calibration();
        assert_eq!(
            app.calibration.as_ref().unwrap().phase,
            CalibrationPhase::TestingGpu
        );

        app.calibration_events
            .lock()
            .unwrap()
            .push_back(BenchmarkEvent::GpuFinished { seconds: 1.0 });
        app.drain_calibration();
        assert_eq!(app.calibration.as_ref().unwrap().multiplier, Some(2.0));
        let profile = app.gpu_profile().expect("calibration profile");
        assert!(profile.tested);
        assert_eq!(profile.multiplier, Some(2.0));
        assert!(!profile.selected);

        app.choose_gpu(true);
        assert!(app.calibration.is_none());
        let reloaded = pro_app();
        let profile = reloaded
            .gpu_profiles
            .get("accuracy")
            .expect("persisted profile");
        assert!(profile.selected);
        assert_eq!(profile.multiplier, Some(2.0));
    }

    #[test]
    fn acceleration_selection_is_per_preset() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("gpu-presets");
        let mut app = pro_app();
        app.gpu_profiles.insert(
            "speed".to_string(),
            GpuProfile {
                tested: true,
                selected: true,
                multiplier: Some(2.3),
                ..GpuProfile::default()
            },
        );
        app.gpu_profiles.insert(
            "balanced".to_string(),
            GpuProfile {
                tested: true,
                selected: false,
                ..GpuProfile::default()
            },
        );
        app.save_settings();

        app.preset = Preset::Speed;
        assert_eq!(app.gpu_multiplier(), Some(2.3));
        assert!(app.gpu_profile().unwrap().selected);

        app.preset = Preset::Balanced;
        assert!(!app.gpu_profile().unwrap().selected);

        let reloaded = pro_app();
        assert!(reloaded.gpu_profiles["speed"].selected);
        assert!(!reloaded.gpu_profiles["balanced"].selected);
    }

    #[test]
    fn calibration_dialog_renders_offer_progress_result_and_error() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("gpu-dialog");
        let mut app = pro_app();
        app.preset = Preset::Accuracy;

        app.calibration = Some(Calibration::new(Preset::Accuracy, true));
        let _ = app.view();

        {
            let calibration = app.calibration.as_mut().unwrap();
            calibration.phase = CalibrationPhase::TestingCpu;
            calibration.progress = 0.25;
        }
        let _ = app.view();

        {
            let calibration = app.calibration.as_mut().unwrap();
            calibration.phase = CalibrationPhase::Result;
            calibration.progress = 1.0;
            calibration.multiplier = Some(2.3);
        }
        let _ = app.view();

        {
            let calibration = app.calibration.as_mut().unwrap();
            calibration.error = Some("Vulkan device lost".to_string());
        }
        let _ = app.view();
    }

    #[test]
    fn unclean_gpu_run_disables_acceleration_on_next_launch() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("gpu-crash");
        let mut app = pro_app();
        let preset = app.preset.arg().to_string();
        app.gpu_profiles.insert(
            preset,
            GpuProfile {
                tested: true,
                selected: true,
                ..GpuProfile::default()
            },
        );
        app.save_settings();

        let marker = App::gpu_guard_path().expect("config dir");
        std::fs::write(&marker, "123").unwrap();

        let restarted = App::new();
        assert!(!restarted.gpu_profile().unwrap().selected);
        assert!(!marker.exists());
        assert!(restarted.toast.is_some());
    }

    #[test]
    fn settings_modal_changes_defaults_and_persists() {
        let _guard = env_lock();
        let _dir = ConfigEnv::new("settings-modal");

        let mut app = pro_app();
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

        let reloaded = pro_app();
        assert_eq!(reloaded.theme_mode, AppTheme::Dark);
        assert_eq!(reloaded.preset, Preset::Accuracy);
        // One ZoomIn step from the 70 % default.
        assert!((reloaded.font_scale - 0.8).abs() < 0.001);
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

        let mut app = pro_app();
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

        let mut app = pro_app();
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
    fn all_done_snaps_the_bars_and_clears_the_current_item() {
        let _guard = env_lock();
        let config = ConfigEnv::new("all-done-bars");
        let mut app = pro_app();

        let audio = config.dir.join("done.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = config.dir.join("done.txt");
        std::fs::write(&transcript, b"transcript").unwrap();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio.clone()),
            status: ItemStatus::Done,
            duration: Some(10.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        });
        app.batch_paths = vec![audio];
        app.current = Some(0);
        app.current_index = Some(0);
        app.status = Status::Running;
        app.progress = 0.26;
        app.progress_shown = 0.26;
        app.overall_shown = 0.26;

        app.events.lock().unwrap().push_back(Event::AllDone);
        let _ = app.update(Message::Tick);

        assert!(matches!(app.status, Status::Done));
        assert_eq!(app.progress_shown, 1.0);
        assert_eq!(app.overall_shown, 1.0);
        assert!(app.current_index.is_none());
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
    fn pinning_the_edited_row_keeps_it_in_the_window() {
        fn line(raw: &str, speaker: &str) -> DocLine {
            DocLine {
                raw: raw.to_string(),
                speaker: speaker.to_string(),
                start: parse_time_prefix(raw),
                confidence: None,
            }
        }
        let lines = vec![
            line("[00:00] A: one", "A"),
            line("[00:05] B: two", "B"),
            line("[00:10] A: three", "A"),
        ];
        // A filtered order that hides the middle row gets it back in place.
        let mut display = vec![0, 2];
        pin_display_index(&mut display, &lines, 1);
        assert_eq!(display, vec![0, 1, 2]);

        // Untimed lines sort last, so an untimed edited row goes to the end.
        let mut untimed = lines.clone();
        untimed.push(DocLine {
            raw: "untimed".to_string(),
            speaker: String::new(),
            start: None,
            confidence: None,
        });
        let mut display = vec![0, 1];
        pin_display_index(&mut display, &untimed, 3);
        assert_eq!(display, vec![0, 1, 3]);

        // Already visible: nothing moves, and out-of-range indices are ignored.
        let mut display = vec![0, 1, 2];
        pin_display_index(&mut display, &lines, 1);
        assert_eq!(display, vec![0, 1, 2]);
        pin_display_index(&mut display, &lines, 9);
        assert_eq!(display, vec![0, 1, 2]);
    }

    #[test]
    fn clicking_another_utterance_commits_the_open_edit() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("click-switch-config");
        let media = temp_dir("click-switch-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n",
        )
        .unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(10.0),
            transcript: Some(transcript.clone()),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        open_line_edit(&mut app, 0);
        current_row_mut(&mut app).content = text_editor::Content::with_text("one edited");
        // The click on another utterance writes the open row first, then moves
        // the field to the clicked line, all through the same message.
        let _ = app.update(Message::LineClicked(1));
        assert_eq!(
            std::fs::read_to_string(&transcript).unwrap(),
            "[00:00] SPEAKER 1: one edited\n[00:05] SPEAKER 2: two\n"
        );
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(1));
        assert_eq!(app.line_edit.as_ref().unwrap().rows.len(), 1);
        // The panel and the field lay out without panicking.
        let _ = app.view();

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_inline_editor_renders_without_a_modal() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("inline-render-config");
        let media = temp_dir("inline-render-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: one\n[00:05] SPEAKER 2: two\n",
        )
        .unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio,
            audio: None,
            status: ItemStatus::Done,
            duration: Some(10.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);

        open_line_edit(&mut app, 1);
        // The editor is not a popup: hover chrome and tooltips stay live.
        assert!(!app.popup_open());
        assert!(app.line_edit_panel(app.scheme()).is_some());
        let _ = app.view();
        // Bubble mode renders the same field where the bubble was.
        app.sms_mode = true;
        let _ = app.view();
        let _ = app.update(Message::CancelLineEdit);
        assert!(app.line_edit_panel(app.scheme()).is_none());

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn the_edited_row_stays_rendered_when_it_is_scrolled_out() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("pinned-row-config");
        let media = temp_dir("pinned-row-media");

        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        let mut text = String::new();
        for index in 0..200 {
            text.push_str(&format!(
                "[{:02}:{:02}] SPEAKER 1: line {index}\n",
                index / 60,
                index % 60
            ));
        }
        std::fs::write(&transcript, text).unwrap();

        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio,
            audio: None,
            status: ItemStatus::Done,
            duration: Some(400.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.2; 16]),
        });
        open_test_item(&mut app, 0);
        app.transcript_viewport = 600.0;

        // The edited row is above the scrolled window: it is still rendered,
        // in its own segment, so the field never loses its state.
        open_line_edit(&mut app, 0);
        app.transcript_offset = 3000.0;
        app.pane_start.set(Some(120));
        let _ = app.view();
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(0));

        // And below it.
        let _ = app.update(Message::EditLine(150));
        app.transcript_offset = 0.0;
        app.pane_start.set(Some(0));
        let _ = app.view();
        assert_eq!(app.line_edit.as_ref().map(|edit| edit.index), Some(150));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn queue_summary_text_counts_known_durations() {
        let _guard = env_lock();
        let config = ConfigEnv::new("queue-summary");
        let dir = config.dir.clone();

        let mut app = pro_app();
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
    }

    #[test]
    fn adding_audio_with_a_transcript_makes_it_done() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("adopt-config");
        let media = temp_dir("adopt-media");
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(&transcript, "[00:00] SPEAKER 1: adopted line\n").unwrap();

        let mut app = pro_app();
        let added = app.add_paths(vec![audio.clone()]);
        assert_eq!(added.len(), 1);
        assert!(matches!(app.queue[0].status, ItemStatus::Pending));

        // What the add starts: the lookup finds the transcript, and the read
        // and probe come back with it.
        let hits = find_transcripts(&[(0, audio.clone())], None);
        assert_eq!(hits.len(), 1);
        let text = backend::read_transcript(&hits[0].2).unwrap();
        let _ = app.update(Message::TranscriptAdopted(
            true,
            AdoptedTranscript {
                index: 0,
                audio: audio.clone(),
                transcript: transcript.clone(),
                text,
                duration: Some(12.0),
                peaks: vec![0.3; 8],
                turns: Vec::new(),
            },
        ));

        assert!(matches!(app.queue[0].status, ItemStatus::Done));
        assert_eq!(app.queue[0].transcript.as_deref(), Some(transcript.as_path()));
        assert_eq!(app.queue[0].duration, Some(12.0));
        assert_eq!(app.queue[0].peaks.len(), 8);
        // A single adopted add opens, so the text is on screen immediately.
        assert_eq!(app.selected, Some(0));
        assert!(app.shown_transcript().contains("adopted line"));

        // A run that started while the lookup was in flight keeps the row: the
        // adoption is dropped rather than overwriting it.
        app.queue[0].status = ItemStatus::Running;
        let _ = app.update(Message::TranscriptAdopted(
            true,
            AdoptedTranscript {
                index: 0,
                audio: audio.clone(),
                transcript: media.join("other.txt"),
                text: "other\n".to_string(),
                duration: None,
                peaks: Vec::new(),
                turns: Vec::new(),
            },
        ));
        assert_eq!(app.queue[0].transcript.as_deref(), Some(transcript.as_path()));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn adding_audio_without_a_transcript_leaves_it_pending() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("adopt-none-config");
        let media = temp_dir("adopt-none-media");
        let audio = media.join("silent.mp3");
        std::fs::write(&audio, b"audio").unwrap();

        let mut app = pro_app();
        let added = app.add_paths(vec![audio.clone()]);
        assert_eq!(added.len(), 1);
        // Nothing to adopt, so the add looks exactly as it did before.
        assert!(find_transcripts(&[(0, audio)], None).is_empty());
        assert!(matches!(app.queue[0].status, ItemStatus::Pending));
        assert_eq!(app.queue[0].transcript, None);
        assert_eq!(app.selected, None);

        let _ = std::fs::remove_dir_all(&media);
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
    fn speaker_options_are_single_or_detected() {
        assert_eq!(SpeakerOpt::ALL.len(), 2);
        assert_eq!(SpeakerOpt::from_value(None), SpeakerOpt::Multiple);
        assert_eq!(SpeakerOpt::from_value(Some(0)), SpeakerOpt::Multiple);
        assert_eq!(SpeakerOpt::from_value(Some(2)), SpeakerOpt::Multiple);
        assert_eq!(SpeakerOpt::from_value(Some(1)), SpeakerOpt::Single);
        assert_eq!(SpeakerOpt::Single.value(), Some(1));
        // Multiple hands the diarizer no count, so it detects.
        assert_eq!(SpeakerOpt::Multiple.value(), None);
    }

    #[test]
    fn a_failed_child_spawn_does_not_mute_gpu_forever() {
        // The launcher failed, so nothing is known about the backend: the preset
        // must offer Test GPU again instead of staying muted.
        assert!(stale_gpu_error(
            "could not start the transcription process: No such file or directory (os error 2)",
            true
        ));
        assert!(stale_gpu_error("whisper model not found", true));
        // A real backend failure stays.
        assert!(!stale_gpu_error("Vulkan device lost", true));
        // With the models missing the reason is real, not stale.
        assert!(!stale_gpu_error("whisper model not found", false));
    }

    #[test]
    fn failed_items_can_be_retried_when_idle() {
        let _guard = env_lock();
        let config = ConfigEnv::new("retry-config");
        let dir = config.dir.clone();
        let audio = dir.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();

        let mut app = pro_app();
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

        let mut app = pro_app();
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

        let _ = app.update(Message::QueueDragStart(1));
        let _ = app.update(Message::QueueDrop(0));
        assert_eq!(names(&app), vec!["b.mp3", "a.mp3", "c.mp3"]);
        // The dropped entry becomes the focused/selected item.
        assert_eq!(app.selected, Some(0));
        assert_eq!(app.selected_view.as_ref().unwrap().index, 0);

        let _ = app.update(Message::QueueDragStart(0));
        let _ = app.update(Message::QueueDrop(1));
        assert_eq!(names(&app), vec!["a.mp3", "b.mp3", "c.mp3"]);

        // Reordering is locked while transcribing.
        app.status = Status::Running;
        let _ = app.update(Message::QueueDragStart(2));
        let _ = app.update(Message::QueueDrop(1));
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
    fn clicking_a_finished_transcript_opens_it_during_a_run() {
        let _guard = env_lock();
        let dir = temp_dir("finished-click-media");

        let done_audio = dir.join("done.mp3");
        std::fs::write(&done_audio, b"audio").unwrap();
        let transcript = dir.join("done.txt");
        std::fs::write(&transcript, "[00:00] SPEAKER 1: hi\n").unwrap();
        let pending_audio = dir.join("next.mp3");
        std::fs::write(&pending_audio, b"audio").unwrap();

        // A live run with one finished row and one waiting row.
        let seed = |app: &mut App| {
            app.queue.push(QueueItem {
                path: done_audio.clone(),
                audio: Some(done_audio.clone()),
                status: ItemStatus::Done,
                duration: Some(1.5),
                transcript: Some(transcript.clone()),
                stats: None,
                turns: Vec::new(),
                peaks: Arc::new(vec![0.1, 0.2, 0.3]),
            });
            app.queue.push(QueueItem {
                path: pending_audio.clone(),
                audio: Some(pending_audio.clone()),
                status: ItemStatus::Pending,
                duration: None,
                transcript: None,
                stats: None,
                turns: Vec::new(),
                peaks: Arc::new(Vec::new()),
            });
            app.status = Status::Running;
            app.selected = None;
            app.selected_view = None;
        };

        // Pro: the click opens the finished row even from the live run view.
        {
            let _config = ConfigEnv::new("finished-click-pro");
            let mut app = pro_app();
            seed(&mut app);

            let _ = app.update(Message::QueueDragStart(0));
            assert!(app.queue_drag.is_none());
            assert_eq!(app.selected, Some(0));
            assert!(app.selected_view.is_some());
            // Retention keys on the open finished view, not on the click.
            assert!(app.keeps_finished_view());

            // The row being transcribed is still clickable: it returns to the
            // live transcript from the pinned one.
            app.queue[1].status = ItemStatus::Running;
            app.current_index = Some(1);
            let _ = app.update(Message::QueueDragStart(1));
            assert!(app.selected.is_none());
            assert!(app.selected_view.is_none());

            // And the finished row can be opened again afterwards.
            let _ = app.update(Message::QueueDragStart(0));
            assert_eq!(app.selected, Some(0));
            assert!(app.keeps_finished_view());
            app.selected_view = None;
            assert!(!app.keeps_finished_view());
        }

        // Free: the same click is an upsell, not a view.
        {
            let _config = ConfigEnv::new("finished-click-free");
            let mut app = App::new();
            seed(&mut app);
            assert!(app.shows(license::Audience::FreeOnly));

            let _ = app.update(Message::QueueDragStart(0));
            assert!(app.queue_drag.is_none());
            assert!(app.selected.is_none());
            assert!(app.selected_view.is_none());
            assert_eq!(
                app.toast.as_ref().map(|(text, _)| text.as_str()),
                Some("Reviewing during active transcription is a Pro feature")
            );
        }
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

        let mut app = pro_app();
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
        open_test_item(&mut app, 0);

        // Populate both caches before mutating a line.
        assert_eq!(app.display_order(), sorted_display_order(app.shown_lines()));
        assert_eq!(
            app.speaker_order(),
            distinct_speaker_order(app.shown_lines())
        );

        let _ = app.update(Message::EditLine(0));
        {
            let row = current_row_mut(&mut app);
            row.speaker = "ZED".to_string();
            row.content = text_editor::Content::with_text("changed");
        }
        let _ = app.update(Message::DoneLineEdit);

        assert_eq!(app.display_order(), sorted_display_order(app.shown_lines()));
        // The order is the document's distinct names, sorted into the rename
        // dialog's natural order, so a name keeps its palette colour wherever
        // its line sits in the transcript.
        let mut expected = distinct_speaker_order(app.shown_lines());
        sort_speaker_names(&mut expected);
        assert_eq!(app.speaker_order(), expected);
        assert_eq!(
            app.speaker_order(),
            vec![
                "SPEAKER 1".to_string(),
                "SPEAKER 2".to_string(),
                "ZED".to_string()
            ]
        );

        let _ = std::fs::remove_dir_all(&media);
    }

    /// One transcript row per line, opened as a completed item so the follow
    /// logic takes the playback (centred) path rather than the live bottom one.
    fn follow_app(tag: &str) -> (App, PathBuf) {
        let media = temp_dir(tag);
        let audio = media.join("talk.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("talk.txt");
        std::fs::write(
            &transcript,
            "[00:00] SPEAKER 1: one\n[00:02] SPEAKER 2: two\n[00:04] SPEAKER 1: three\n[00:06] SPEAKER 2: four\n",
        )
        .unwrap();
        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(8.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.1; 16]),
        });
        open_test_item(&mut app, 0);
        app.transcript_viewport = 400.0;
        app.transcript_content = 1600.0;
        app.follow_playhead = true;
        (app, media)
    }

    #[test]
    fn follow_target_is_not_reissued_for_an_unchanged_view() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-target-config");
        let (mut app, media) = follow_app("follow-target-media");

        let order = app.display_order();
        assert_eq!(order.len(), 4);

        // The scrollable landed 10 px away from what we asked for (average row
        // heights are estimates, and `scroll_to` clamps/rounds). The reported
        // offset therefore never matches the desired target.
        app.transcript_offset = 810.0;

        // 2.5 rows * 400 px - 200 px viewport = 800. A one-row move eases, so
        // the offset is not returned for a jump; the glide is the scroll.
        assert_eq!(app.follow_scroll(Some(order[2])), None);
        assert_eq!(app.scroll_anim.map(|anim| anim.target), Some(800.0));
        // Treat the glide as landed before checking that the tick stays quiet.
        app.scroll_anim = None;
        // Every following tick used to re-issue, each time with a target that
        // had drifted a little: that is the jitter.
        for _ in 0..5 {
            assert_eq!(app.follow_scroll(Some(order[2])), None);
            // The offset does not move because the target is unreachable.
            app.transcript_offset = 810.0;
        }

        // Streamed growth of a couple of pixels does not move the estimate.
        app.transcript_content = 1602.0;
        assert_eq!(app.follow_scroll(Some(order[2])), None);

        // A different active line is a real move and still scrolls at once,
        // even inside the frame budget.
        let next = app.follow_scroll(Some(order[3])).unwrap();
        assert!((next - 800.0).abs() > 6.0, "{next}");

        // A user who scrolled away with Follow off gets no programmatic scroll.
        app.follow_playhead = false;
        app.transcript_at_bottom = false;
        assert_eq!(app.follow_scroll(Some(order[3])), None);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn follow_target_holds_back_small_drift() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-drift-config");
        let (mut app, media) = follow_app("follow-drift-media");
        let order = app.display_order();

        app.transcript_offset = 780.0;
        // One row out: eased rather than jumped, and the glide is the scroll.
        assert_eq!(app.follow_scroll(Some(order[2])), None);
        assert_eq!(app.scroll_anim.map(|anim| anim.target), Some(800.0));
        app.scroll_anim = None;

        // The scroll landed, so the pending offset is stale by the time the
        // next tick runs; a few pixels of drift must not queue another jump.
        app.transcript_offset = 802.0;
        app.transcript_content = 1610.0;
        assert_eq!(app.follow_scroll(Some(order[2])), None);

        let _ = std::fs::remove_dir_all(&media);
    }

    /// Transcript whose first line starts later than 0, so a playhead of 0
    /// matches no line and `active_line_index` falls back to the first row.
    fn delayed_follow_app(tag: &str) -> (App, PathBuf) {
        let media = temp_dir(tag);
        let audio = media.join("late.mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let transcript = media.join("late.txt");
        std::fs::write(
            &transcript,
            "[00:20] SPEAKER 1: one\n[00:24] SPEAKER 2: two\n[00:28] SPEAKER 1: three\n[00:32] SPEAKER 2: four\n",
        )
        .unwrap();
        let mut app = pro_app();
        app.queue.push(QueueItem {
            path: audio.clone(),
            audio: Some(audio),
            status: ItemStatus::Done,
            duration: Some(40.0),
            transcript: Some(transcript),
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(vec![0.1; 16]),
        });
        open_test_item(&mut app, 0);
        app.transcript_viewport = 400.0;
        app.transcript_content = 2000.0;
        app.follow_playhead = true;
        (app, media)
    }

    #[test]
    fn follow_never_scrolls_to_the_top() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-top-config");
        let (mut app, media) = delayed_follow_app("follow-top-media");

        // The playhead sits before the first line, so nothing matches it.
        app.playhead = 0.0;
        app.transcript_offset = 900.0;

        let first = app.display_order().first().copied().unwrap();
        // The highlight still falls back to the first row...
        assert_eq!(app.active_line_index(), Some(first));
        // ...but Follow must not: no line to follow, no scroll.
        assert_eq!(app.follow_line_index(), None);
        assert_eq!(app.next_follow_target(app.follow_line_index()), None);

        // Even handed the first row, centring it would clamp to 0. Follow
        // declines instead of jumping to the top.
        assert_eq!(app.next_follow_target(Some(first)), None);
        assert_eq!(app.transcript_offset, 900.0);

        // A playhead that does match a later line still scrolls.
        app.playhead = 28.0;
        app.follow_issue = None;
        let target = app.follow_scroll(app.follow_line_index()).unwrap();
        assert!(target > FOLLOW_DEADBAND, "{target}");

        // Content that fits the pane cannot scroll, even if the estimate of
        // the bottom lands on 0 after a transient re-layout shrink.
        app.follow_playhead = false;
        app.selected_view = None;
        app.status = Status::Running;
        app.transcript_at_bottom = true;
        app.transcript_viewport = 400.0;
        app.transcript_content = 300.0;
        assert_eq!(app.next_follow_target(None), None);

        let _ = std::fs::remove_dir_all(&media);
    }

    /// Collect the index path of every widget carrying `tag`.
    fn tag_paths(tree: &Tree, tag: Tag, path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if tree.tag == tag {
            out.push(path.clone());
        }
        for (index, child) in tree.children.iter().enumerate() {
            path.push(index);
            tag_paths(child, tag, path, out);
            path.pop();
        }
    }

    /// Where every scrollable sits in the view tree. iced keys widget state
    /// (including the scroll offset) off this position, so a changed path means
    /// a remount back to the top.
    fn scrollable_paths(app: &App) -> Vec<Vec<usize>> {
        // The state type is private in iced, so learn its tag from a probe.
        let probe: Element<'static, Message> = scrollable(horizontal_space()).into();
        let tag = Tree::new(probe.as_widget()).tag;

        let view = app.view();
        let tree = Tree::new(view.as_widget());
        let mut out = Vec::new();
        tag_paths(&tree, tag, &mut Vec::new(), &mut out);
        assert!(!out.is_empty(), "expected scrollables in the view");
        out
    }

    #[test]
    fn the_icon_line_box_matches_the_glyph_so_a_circle_can_centre_it() {
        use iced::advanced::graphics::text as gtext;
        use iced::advanced::text::{self, Paragraph as _};

        let paragraph = |line_height| {
            gtext::Paragraph::with_text(text::Text {
                content: "\u{f145}",
                bounds: Size::new(f32::INFINITY, 64.0),
                size: Pixels(ICON_SIZE),
                line_height,
                font: Font::with_name("bootstrap-icons"),
                horizontal_alignment: iced::alignment::Horizontal::Left,
                vertical_alignment: iced::alignment::Vertical::Top,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::None,
            })
        };

        // Tightened to the em box: the glyph's node is exactly the glyph, so a
        // circle that centres the node centres the ink.
        let tight = paragraph(icon_line_height(ICON_SIZE));
        assert_eq!(tight.min_bounds().height, ICON_SIZE);

        // iced's default is 1.3 em, and the spare space goes below the baseline,
        // which is what would sit the icon about 2 px high in the circle.
        let loose = paragraph(text::LineHeight::default());
        assert!(
            loose.min_bounds().height > ICON_SIZE + 2.0,
            "default line box {} should be materially taller than the glyph",
            loose.min_bounds().height
        );
    }

    #[test]
    fn the_glide_fraction_eases_and_lands_a_long_frame_in_one_step() {
        let tau = FOLLOW_GLIDE_TAU;
        // About a sixth of the remaining distance per 60 Hz frame.
        let frame = glide_fraction(Duration::from_micros(16_667), tau);
        assert!((frame - 0.169).abs() < 0.01, "{frame}");
        // One time constant covers 63 %.
        let constant = glide_fraction(tau, tau);
        assert!((constant - 0.632).abs() < 0.01, "{constant}");
        // Nothing moves in no time, and the curve is monotone.
        assert_eq!(glide_fraction(Duration::ZERO, tau), 0.0);
        assert!(
            glide_fraction(Duration::from_millis(8), tau)
                < glide_fraction(Duration::from_millis(20), tau)
        );
        assert!(
            glide_fraction(Duration::from_millis(20), tau)
                < glide_fraction(Duration::from_millis(60), tau)
        );
        // A gap the window could not have shown (occluded, stalled) lands in
        // one step instead of sweeping the pane across it.
        assert_eq!(glide_fraction(FOLLOW_GLIDE_MAX_GAP, tau), 1.0);
        assert_eq!(glide_fraction(Duration::from_secs(30), tau), 1.0);

        // A longer constant covers less of the distance in the same frame: that
        // is what stops a screenful arriving as fast as a one-row nudge.
        let slow = glide_fraction(Duration::from_millis(25), FOLLOW_JUMP_TAU);
        let quick = glide_fraction(Duration::from_millis(25), FOLLOW_GLIDE_TAU);
        assert!(slow < quick, "{slow} vs {quick}");
    }

    #[test]
    fn the_glide_advance_lands_inside_the_snap_threshold() {
        let tau = FOLLOW_GLIDE_TAU;
        // Well short of the target: a step, not a landing.
        let (next, landed) = glide_advance(100.0, 124.0, Duration::from_millis(25), tau);
        assert!(!landed);
        assert!(next > 100.0 && next < 124.0, "{next}");
        // Inside half a pixel it lands exactly.
        let (next, landed) = glide_advance(123.8, 124.0, Duration::from_millis(16), tau);
        assert!(landed);
        assert_eq!(next, 124.0);
        // A long gap lands in one step.
        let (next, landed) = glide_advance(0.0, 24.0, Duration::from_secs(5), tau);
        assert!(landed);
        assert_eq!(next, 24.0);
    }

    #[test]
    fn only_centred_moves_glide_and_deliberate_jumps_glide_further() {
        let row = 24.0;
        // The playhead path, one row, and exactly the routine ceiling: ease.
        assert_eq!(
            glide_shape(true, false, row, row, false, false),
            Glide::Ease {
                tau: FOLLOW_GLIDE_TAU
            }
        );
        assert_eq!(
            glide_shape(true, false, 2.0 * row, row, false, false),
            Glide::Ease {
                tau: FOLLOW_GLIDE_TAU
            }
        );
        // Only the centred path. The bottom pin follows a target that grows as
        // text streams in, so it must stay instant.
        assert_eq!(
            glide_shape(false, false, row, row, false, false),
            Glide::Snap
        );
        // Below the deadband nothing moves at all.
        assert_eq!(
            glide_shape(true, false, FOLLOW_DEADBAND - 0.5, row, false, false),
            Glide::Snap
        );
        // A routine advance past two rows that nobody asked for lands.
        assert_eq!(
            glide_shape(true, false, 2.0 * row + 1.0, row, false, false),
            Glide::Snap
        );
        // The first move after Follow is switched on always lands.
        assert_eq!(
            glide_shape(true, false, row, row, true, false),
            Glide::Snap
        );
        // Direction does not matter.
        assert_eq!(
            glide_shape(true, false, -row, row, false, false),
            Glide::Ease {
                tau: FOLLOW_GLIDE_TAU
            }
        );

        // A deliberate jump eases at any distance, with a constant that grows
        // with the distance so a screenful does not arrive like a nudge.
        let small = jump_tau(3.0 * row, row);
        let large = jump_tau(FOLLOW_JUMP_ROWS * row, row);
        assert!(small >= FOLLOW_GLIDE_TAU && small < large, "{small:?}");
        assert_eq!(large, FOLLOW_JUMP_TAU);
        assert_eq!(jump_tau(40.0 * row, row), FOLLOW_JUMP_TAU);
        assert_eq!(
            glide_shape(true, true, 20.0 * row, row, false, false),
            Glide::Ease {
                tau: FOLLOW_JUMP_TAU
            }
        );
        // A jump still in flight keeps the jump shape when the playhead moves
        // on mid-glide, so the routine ceiling cannot cut it short.
        assert_eq!(
            glide_shape(true, false, 4.0 * row, row, false, true),
            Glide::Ease {
                tau: jump_tau(4.0 * row, row)
            }
        );
        // Even a jump lands when it is inside the deadband or a dialog is up.
        assert_eq!(
            glide_shape(true, true, FOLLOW_DEADBAND - 0.5, row, false, false),
            Glide::Snap
        );
        assert_eq!(
            glide_shape(true, true, row, row, true, false),
            Glide::Snap
        );
    }

    #[test]
    fn a_hand_scroll_ends_the_glide_and_follow() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("hand-scroll-config");
        let (mut app, media) = follow_app("hand-scroll-media");

        app.transcript_content = 1200.0;
        app.transcript_viewport = 400.0;
        app.pane_row_height.set(Some(24.0));
        app.transcript_offset = 700.0;

        // A jump is running towards the clicked line.
        let _ = app.update(Message::Seek(5.0));
        let anim = app.scroll_anim.expect("a click should ease to the line");
        assert!(anim.jump);
        let expected = anim.expected;

        // iced reports a scrollbar drag, a track click and a wheel notch the same
        // way — an offset — so none can be told from the others, and none is
        // ever eased. A report that is not where the glide asked the pane to be
        // is the reader taking it: the glide stops and Follow turns off.
        assert_eq!(
            app.correct_follow_offset(1200.0, expected + 60.0, 400.0, 1200.0),
            None
        );
        assert!(app.scroll_anim.is_none(), "a hand scroll must stop the glide");
        assert!(
            !app.follow_playhead,
            "a hand scroll must turn Follow off rather than snap back"
        );

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_jump_the_user_asked_for_survives_with_follow_off() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("jump-no-follow-config");
        let (mut app, media) = follow_app("jump-no-follow-media");

        app.transcript_content = 1200.0;
        app.transcript_viewport = 400.0;
        app.transcript_offset = 700.0;
        app.follow_playhead = false;
        // Long enough ago that this seek is a single gesture, not a scrub.
        app.last_seek = Some(
            Instant::now()
                .checked_sub(FOLLOW_SEEK_QUIET + Duration::from_millis(1))
                .unwrap(),
        );

        let _ = app.update(Message::Seek(5.0));
        assert!(
            app.scroll_anim.is_some_and(|anim| anim.jump),
            "a timeline click with Follow off should still ease"
        );

        // The follow tick does not own this move: it must not clear it.
        let _ = app.update(Message::Tick);
        assert!(
            app.scroll_anim.is_some_and(|anim| anim.jump),
            "the follow tick cleared a jump it did not start"
        );

        // And the jump lands on its own, with no follow to finish it.
        let mut now = app.scroll_anim.unwrap().last;
        for _ in 0..120 {
            if app.scroll_anim.is_none() {
                break;
            }
            now += FOLLOW_GLIDE_FRAME;
            app.advance_glide(now);
        }
        assert!(app.scroll_anim.is_none(), "the jump did not land");

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_scrubbed_timeline_lands_at_once_and_a_click_eases() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("seek-glide-config");
        let (mut app, media) = follow_app("seek-glide-media");

        app.transcript_content = 1200.0;
        app.transcript_viewport = 400.0;
        app.pane_row_height.set(Some(24.0));

        // A single click is one deliberate jump: it eases, and at a distance
        // well past the routine ceiling.
        let _ = app.update(Message::Seek(5.0));
        let anim = app.scroll_anim.expect("a click should ease to the line");
        assert!(anim.jump);
        assert!(anim.tau >= FOLLOW_GLIDE_TAU);
        let target = app.follow_target(app.line_for_playhead(app.playhead).unwrap()).unwrap();
        assert_eq!(anim.target, target);

        // The very next seek of a scrub is the same gesture: land at once so the
        // pane does not trail the pointer.
        let _ = app.update(Message::Seek(6.0));
        assert!(
            app.scroll_anim.is_none(),
            "a scrub must not start a glide per event"
        );
        assert!(app.scroll_target.is_some());

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn a_one_row_advance_glides_and_lands() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("glide-config");
        let (mut app, media) = follow_app("glide-media");

        // Four 400 px rows in a 400 px pane: centring the second line is 400 px.
        // Start one 24 px row short of it.
        app.pane_row_height.set(Some(24.0));
        app.transcript_offset = 376.0;
        let active = app.display_order()[1];

        assert_eq!(
            app.follow_scroll(Some(active)),
            None,
            "a one-row advance should glide, not snap"
        );
        let anim = app.scroll_anim.expect("a glide started");
        assert_eq!(anim.target, 400.0);

        // Step it at the frame cap until it lands.
        let mut now = anim.last;
        let mut steps = 0;
        while app.scroll_anim.is_some() && steps < 60 {
            now += FOLLOW_GLIDE_FRAME;
            if app.advance_glide(now).is_some() {
                steps += 1;
            }
        }
        assert!(app.scroll_anim.is_none(), "the glide did not land");
        assert_eq!(app.follow_offset, Some(400.0));
        assert!(steps > 4, "the glide should take several frames, took {steps}");

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn long_moves_the_bottom_pin_and_the_first_move_after_enabling_snap() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("snap-config");
        let (mut app, media) = follow_app("snap-media");

        app.pane_row_height.set(Some(24.0));

        // Four rows short of the target: a jump, not a nudge.
        app.transcript_offset = 304.0;
        let active = app.display_order()[1];
        assert_eq!(app.follow_scroll(Some(active)), Some(400.0));
        assert!(app.scroll_anim.is_none(), "a four-row move must not glide");

        // The first move after Follow is switched on lands even at one row.
        app.scroll_target = None;
        app.follow_issue = None;
        app.transcript_offset = 376.0;
        app.snap_next_follow = true;
        let active = app.display_order()[1];
        assert_eq!(app.follow_scroll(Some(active)), Some(400.0));
        assert!(app.scroll_anim.is_none());
        assert!(!app.snap_next_follow, "the flag should be spent");

        // The bottom pin tracks a target that grows as text arrives, so even a
        // one-row move of it must land rather than ease.
        app.selected_view = None;
        app.follow_playhead = false;
        app.transcript_at_bottom = true;
        app.scroll_target = None;
        app.follow_issue = None;
        app.transcript_offset = 1176.0;
        assert_eq!(app.follow_scroll(None), Some(1200.0));
        assert!(app.scroll_anim.is_none());

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn transcript_row_height_is_latched_not_self_referential() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("pane-height-config");
        let (mut app, media) = follow_app("pane-height-media");

        // Ten rendered rows filling 400 px: the window measured 40 px per row.
        app.pane_window.set((0, 10, 0.0, 0.0));
        app.transcript_content = 400.0;
        assert_eq!(app.pane_line_height(24.0), 40.0);

        // The reported content contains the spacers built from this estimate,
        // so it drifts on its own while scrolling. A few pixels must not move
        // the estimate — that feedback is what crept the pane.
        for content in [410.0, 430.0, 460.0, 440.0] {
            app.transcript_content = content;
            assert_eq!(app.pane_line_height(24.0), 40.0, "content {content}");
        }

        // A real size change (bubble mode, zoom) recalibrates.
        app.transcript_content = 900.0;
        assert_eq!(app.pane_line_height(24.0), 90.0);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn transcript_window_holds_still_while_scrolling_inside_it() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("pane-window-config");
        let (mut app, media) = follow_app("pane-window-media");

        // Long enough to need a virtualised window with room to scroll in.
        let text: String = (0..200)
            .map(|i| format!("[{:02}:{:02}] SPEAKER 1: line {i}\n", i / 60, i % 60))
            .collect();
        if let Some(view) = app.selected_view.as_mut() {
            view.lines = parse_lines(&text);
            view.transcript = text.clone();
        }
        app.invalidate_derived();

        app.transcript_viewport = 400.0;
        app.transcript_content = 2000.0;
        app.transcript_offset = 800.0;
        let _ = app.transcript_element();
        let anchor = app.pane_start.get().expect("virtual window");

        // Small scrolls stay inside the window: moving the anchor would shift
        // every rendered row by (estimate - real height).
        for offset in [810.0, 830.0, 860.0, 900.0, 700.0] {
            app.transcript_offset = offset;
            let _ = app.transcript_element();
            assert_eq!(app.pane_start.get(), Some(anchor), "offset {offset}");
        }

        // A jump that leaves the window re-anchors.
        app.transcript_offset = 1800.0;
        let _ = app.transcript_element();
        assert_ne!(app.pane_start.get(), Some(anchor));

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn scrollable_path_is_stable_across_transient_state() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("shape-audit-config");
        let baseline = {
            let (app, media) = follow_app("shape-audit-base");
            let paths = scrollable_paths(&app);
            let _ = std::fs::remove_dir_all(&media);
            paths
        };

        let cases: Vec<(&str, Box<dyn Fn(&mut App)>)> = vec![
            (
                "header_menu",
                Box::new(|app| app.header_menu = Some(HeaderMenu::View)),
            ),
            ("find_open", Box::new(|app| app.find_open = true)),
            (
                "line_hover",
                Box::new(|app| app.transcript_line_hover = Some(1)),
            ),
            (
                "at_bottom_off",
                Box::new(|app| app.transcript_at_bottom = false),
            ),
            ("offset", Box::new(|app| app.transcript_offset = 640.0)),
            (
                "search",
                Box::new(|app| {
                    app.search_query = "e".to_string();
                    app.search_index = 1;
                }),
            ),
            ("console", Box::new(|app| app.console_open = true)),
            (
                "toast",
                Box::new(|app| app.toast = Some(("Saved".to_string(), Instant::now()))),
            ),
            ("drag_hover", Box::new(|app| app.drag_hover = true)),
            ("sms_mode", Box::new(|app| app.sms_mode = true)),
            (
                "join_lines",
                Box::new(|app| {
                    app.join_lines = true;
                    app.invalidate_derived();
                }),
            ),
            ("zoom", Box::new(|app| app.font_scale = 1.35)),
            (
                "live_view",
                Box::new(|app| {
                    app.selected_view = None;
                    app.selected = None;
                    app.status = Status::Running;
                    app.lines = parse_lines("[00:00] A: one\n[00:02] B: two\n");
                    app.transcript = "[00:00] A: one\n[00:02] B: two\n".to_string();
                    app.invalidate_derived();
                }),
            ),
            (
                "model_error",
                Box::new(|app| app.model_error = Some("offline".to_string())),
            ),
        ];

        let mut unstable = Vec::new();
        for (name, mutate) in cases {
            let (mut app, media) = follow_app("shape-audit-case");
            mutate(&mut app);
            let paths = scrollable_paths(&app);
            // Every scrollable that was there must still be at the same path.
            let kept = baseline.iter().all(|path| paths.contains(path));
            // A new scrollable is fine only when it is appended after the
            // transcript (the console panel), never by shifting it.
            let appended = name == "console" && paths.len() == baseline.len() + 1;
            if !kept || (paths.len() != baseline.len() && !appended) {
                unstable.push(name);
            }
            let _ = std::fs::remove_dir_all(&media);
        }
        assert!(
            unstable.is_empty(),
            "these states remount the transcript scrollable: {unstable:?}"
        );
    }

    #[test]
    fn toast_does_not_move_the_transcript_in_the_widget_tree() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("toast-shape-config");
        let (mut app, media) = follow_app("toast-shape-media");

        // Showing a toast must not change where the scrollable sits: any change
        // of path remounts it and drops the scroll offset back to 0.
        let without = scrollable_paths(&app);
        app.toast = Some(("Speaker: SPEAKER 2".to_string(), Instant::now()));
        assert_eq!(scrollable_paths(&app), without);

        // Dismissing it must not either.
        app.toast = None;
        assert_eq!(scrollable_paths(&app), without);

        // The file-drop scrim is the same kind of layer and must behave the
        // same way: a drag over the window must not remount the transcript.
        app.drag_hover = true;
        assert_eq!(scrollable_paths(&app), without);
        app.drag_hover = false;
        assert_eq!(scrollable_paths(&app), without);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn paused_follow_stays_put_after_one_scroll() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-paused-config");
        let (mut app, media) = follow_app("follow-paused-media");

        let order = app.display_order();
        app.follow_playhead = true;
        app.playhead = 4.0;
        // The scrollable settled 10 px away from the estimate and will never
        // converge (clamping and rounding apply). The old level-triggered tick
        // re-issued on every frame, which is a stutter that survives pausing.
        app.transcript_offset = 810.0;

        let active = app.follow_line_index();
        assert_eq!(active, Some(order[2]));
        // One row out eases rather than jumps; land it before checking that a
        // paused pane stays put.
        assert_eq!(app.follow_scroll(active), None);
        assert_eq!(app.scroll_anim.map(|anim| anim.target), Some(800.0));
        app.scroll_anim = None;

        // With the playhead, line, content and viewport unchanged, the tick
        // must emit nothing at all.
        for _ in 0..30 {
            assert_eq!(app.follow_scroll(active), None);
        }

        // A different active line is a new situation and scrolls at once.
        app.playhead = 6.0;
        let next_active = app.follow_line_index();
        assert_eq!(next_active, Some(order[3]));
        let next = app.follow_scroll(next_active).unwrap();
        assert!((next - 800.0).abs() > 6.0, "{next}");

        // And it goes quiet again while that playhead holds still.
        for _ in 0..30 {
            assert_eq!(app.follow_scroll(next_active), None);
        }

        // A viewport change is an input change too, and a paused pane whose
        // content still overflows follows the new geometry once.
        app.transcript_viewport = 300.0;
        assert!(app.follow_scroll(next_active).is_some());
        assert_eq!(app.follow_scroll(next_active), None);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn flip_during_follow_keeps_the_playhead_in_view() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("follow-flip-config");
        let (mut app, media) = follow_app("follow-flip-media");

        // Follow sits on the playhead target with the content unchanged.
        app.follow_playhead = true;
        app.transcript_offset = 800.0;
        app.follow_offset = Some(800.0);
        let active = app.follow_line_index();
        app.note_follow_issue(800.0, active);
        app.scroll_target = None;

        // Right-clicking a line raises a toast, which used to remount the
        // scrollable: the fresh state reports offset 0 for a frame.
        let corrective = app.correct_follow_offset(1600.0, 0.0, 400.0, 1600.0);
        assert_eq!(corrective, Some(800.0));
        // Follow is untouched and still pinned to the playhead, not the top.
        assert!(app.follow_playhead);
        assert_eq!(app.follow_offset, Some(800.0));
        assert_eq!(app.scroll_target, Some(800.0));
        assert_eq!(app.follow_issue.map(|issue| issue.target), Some(800.0));

        // A later re-wrap that clamps the offset keeps following from there
        // instead of fighting the scrollable.
        app.scroll_target = None;
        app.follow_offset = Some(1600.0);
        assert_eq!(
            app.correct_follow_offset(2000.0, 900.0, 400.0, 1300.0),
            None
        );
        assert_eq!(app.follow_offset, Some(900.0));
        assert!(app.follow_playhead);

        // A genuine user scroll with the content unchanged still disengages.
        app.follow_offset = Some(900.0);
        assert_eq!(
            app.correct_follow_offset(1300.0, 300.0, 400.0, 1300.0),
            None
        );
        assert!(!app.follow_playhead);
        assert_eq!(app.follow_offset, None);

        let _ = std::fs::remove_dir_all(&media);
    }

    #[test]
    fn live_streaming_targets_the_bottom_not_the_newest_line() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("stream-bottom-config");

        let mut app = pro_app();
        app.selected_view = None;
        app.status = Status::Running;
        app.follow_playhead = false;
        app.scroll_seek = false;
        app.transcript_viewport = 400.0;
        app.transcript_content = 1600.0;
        app.transcript_offset = 1200.0;
        app.transcript_at_bottom = true;

        // Already at the bottom: no scroll at all.
        assert_eq!(app.follow_scroll(None), None);

        // Streamed text grows the content: pin to the bottom.
        app.transcript_content = 1700.0;
        assert_eq!(app.follow_scroll(None), Some(1300.0));
        // The scroll landed; nothing else moved.
        app.transcript_offset = 1300.0;
        assert_eq!(app.follow_scroll(None), None);

        // With Follow on the live stream pins there too, even though an active
        // line is offered for centring.
        app.follow_playhead = true;
        app.transcript_content = 1750.0;
        assert_eq!(app.follow_scroll(Some(0)), Some(1350.0));

        // Scrolled away from the bottom with Follow off: leave it alone.
        app.follow_playhead = false;
        app.transcript_at_bottom = false;
        app.transcript_offset = 700.0;
        assert_eq!(app.follow_scroll(None), None);
    }

    /// Queue entry for ETA tests; the audio file need not exist.
    fn eta_item(dir: &Path, name: &str, status: ItemStatus, duration: Option<f64>) -> QueueItem {
        let path = dir.join(name);
        QueueItem {
            path: path.clone(),
            audio: Some(path),
            status,
            duration,
            transcript: None,
            stats: None,
            turns: Vec::new(),
            peaks: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn eta_has_no_number_until_the_phase_can_be_priced() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-absent");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.work_done = 10.0;
        app.work_total = 100.0;
        assert!(app.eta_prior.is_none());

        let view = app.estimate_remaining().expect("a phase, even without a number");
        assert_eq!(view.phase, EtaPhase::Transcribing);
        assert_eq!(view.remaining, None, "a rate was invented");

        // The readout names the phase instead of printing digits.
        app.eta_shown = Some(view);
        app.run_start = Some(Instant::now() - Duration::from_secs(42));
        let text = app.timer_text();
        assert!(text.contains("transcribing"), "{text}");
        assert!(!text.contains("left"), "{text}");
        assert!(!text.contains("finishing"), "{text}");
    }

    #[test]
    fn eta_uses_measured_throughput() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-throughput");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.queue.clear();
        app.current_index = None;
        app.work_done = 100.0;
        app.work_total = 400.0;
        app.eta_throughput = Some(2.0);
        // Isolate ASR: no tail on this one.
        app.prepare_prior = Some(0.0);
        let view = app.estimate_remaining().expect("estimate");
        assert_eq!(view.phase, EtaPhase::Transcribing);
        let eta = view.remaining.expect("number");
        assert!((eta - 150.0).abs() <= 1.0, "{eta}");

        // And it reaches the label with the phase in front of it.
        app.eta_shown = Some(view);
        app.run_start = Some(Instant::now() - Duration::from_secs(42));
        let text = app.timer_text();
        assert!(text.contains("transcribing"), "{text}");
        assert!(text.contains("left"), "{text}");
    }

    #[test]
    fn eta_counts_queued_files() {
        let _guard = env_lock();
        let config = ConfigEnv::new("eta-queue");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.queue.clear();
        app.queue
            .push(eta_item(&config.dir, "current.mp3", ItemStatus::Running, Some(400.0)));
        app.queue
            .push(eta_item(&config.dir, "next.mp3", ItemStatus::Pending, Some(200.0)));
        app.queue
            .push(eta_item(&config.dir, "last.mp3", ItemStatus::Pending, Some(300.0)));
        app.current_index = Some(0);
        app.work_done = 100.0;
        app.work_total = 400.0;
        app.eta_throughput = Some(2.0);
        app.prepare_prior = Some(0.0);
        app.diarize_prior = None;

        let eta = app
            .estimate_remaining()
            .expect("estimate")
            .remaining
            .expect("number");
        // 150 s left in this file, plus 500 s of queued audio at 0.58 wall
        // seconds per audio second (ASR + the built-in diarize share).
        let expected = 150.0 + 500.0 * (1.0 / 2.0 + 0.08);
        assert!((eta - expected).abs() <= 1.0, "{eta} vs {expected}");
    }

    #[test]
    fn the_asr_estimate_includes_the_tail_that_follows_it() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-tail-composition");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.queue.clear();
        app.current_index = None;
        app.work_done = 0.0;
        app.work_total = 600.0;
        app.eta_throughput = Some(2.0);
        app.prepare_prior = Some(0.01);

        // 300 s of ASR plus 6 s of attribution and writing: the tail is
        // sequential after the decode, so it adds rather than competing.
        let eta = app
            .estimate_remaining()
            .expect("estimate")
            .remaining
            .expect("number");
        assert!((eta - 306.0).abs() <= 1.0, "{eta}");
    }

    #[test]
    fn eta_prior_survives_a_run() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-prior");
        let mut app = pro_app();
        app.status = Status::Running;
        app.events.lock().unwrap().push_back(Event::Done {
            txt: String::new(),
            stats: backend::Stats {
                audio_seconds: 600.0,
                asr_seconds: 200.0,
                prepare_seconds: 60.0,
                ..Default::default()
            },
        });
        let _ = app.update(Message::Tick);

        assert_eq!(app.eta_prior, Some(3.0));
        assert_eq!(App::load_settings().eta_throughput, Some(3.0));
        // The tail is not learned: `prepare_seconds` cannot be told apart from
        // the pre-ASR clip gap, so the stored value is left alone.
        assert_eq!(app.prepare_prior, None);
        assert_eq!(App::load_settings().eta_prepare, None);
    }

    #[test]
    fn eta_includes_the_prepare_tail() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-prepare-tail");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.queue.clear();
        app.current_index = None;
        app.work_done = 4800.0;
        app.work_total = 4800.0;
        app.timeline_duration = Some(4800.0);
        app.diarize_fraction = 1.0;
        app.diarize_prior = Some(0.1);
        app.prepare_prior = Some(0.01);
        app.eta_throughput = Some(2.0);

        // Decoder and diarizer are spent; only the post-ASR tail is left.
        let eta = app
            .estimate_remaining()
            .expect("estimate")
            .remaining
            .expect("number");
        assert!((eta - 48.0).abs() <= 2.0, "{eta}");
    }

    #[test]
    fn a_small_estimate_is_a_number_not_a_handoff() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-small");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.run_start = Some(Instant::now() - Duration::from_secs(42));
        app.eta_shown = Some(EtaView {
            phase: EtaPhase::Transcribing,
            remaining: Some(4.9),
        });

        // Four-odd seconds left is a number. The word for the hand-off is not
        // allowed to stand in for a small count.
        let text = app.timer_text();
        assert!(text.starts_with("0:4"), "elapsed dropped: {text}");
        assert!(text.contains("5 s left"), "{text}");
        assert!(!text.contains("finishing"), "{text}");

        // The last phase before the write has a number too.
        app.stage_id = "transcription".to_string();
        app.eta_shown = Some(EtaView {
            phase: EtaPhase::Writing,
            remaining: Some(4.0),
        });
        let text = app.timer_text();
        assert!(text.contains("writing"), "{text}");
        assert!(text.contains("5 s left"), "{text}");
        assert!(!text.contains("finishing"), "{text}");
    }

    #[test]
    fn finishing_is_only_the_last_handoff() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-finishing");
        let mut app = pro_app();
        app.status = Status::Running;
        app.run_start = Some(Instant::now() - Duration::from_secs(42));
        app.stage = "Done".to_string();
        app.stage_id = "done".to_string();
        let view = app.estimate_remaining().expect("a view");
        assert_eq!(view.phase, EtaPhase::Finishing);
        assert_eq!(view.remaining, None);
        app.eta_shown = Some(view);
        let text = app.timer_text();
        assert!(text.contains("finishing"), "{text}");
        assert!(!text.contains("left"), "{text}");
    }

    #[test]
    fn eta_waits_for_diarization_after_asr() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-diarize-wait");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "diarization".to_string();
        app.queue.clear();
        app.current_index = None;
        app.work_done = 600.0;
        app.work_total = 600.0;
        app.timeline_duration = Some(600.0);
        app.diarize_fraction = 0.5;
        app.diarize_prior = Some(0.1);
        app.prepare_prior = Some(0.0);
        app.eta_throughput = Some(2.0);

        // ASR is done but half of a 600 s file is still to diarize, and the
        // phase says so.
        let view = app.estimate_remaining().expect("estimate");
        assert_eq!(view.phase, EtaPhase::Diarizing);
        let eta = view.remaining.expect("number");
        assert!((eta - 30.0).abs() <= 2.0, "{eta}");

        // Without a diarization rate there is nothing to price it with, so the
        // label carries no digits rather than a borrowed number.
        app.diarize_prior = None;
        let view = app.estimate_remaining().expect("estimate");
        assert_eq!(view.phase, EtaPhase::Diarizing);
        assert_eq!(view.remaining, None);
        app.eta_shown = Some(view);
        app.run_start = Some(Instant::now() - Duration::from_secs(42));
        let text = app.timer_text();
        assert!(text.contains("diarizing"), "{text}");
        assert!(!text.contains("left"), "{text}");
    }

    #[test]
    fn eta_ignores_finished_diarization() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-diarize-done");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.queue.clear();
        app.current_index = None;
        app.work_done = 100.0;
        app.work_total = 400.0;
        app.timeline_duration = Some(400.0);
        app.diarize_fraction = 1.0;
        app.diarize_prior = Some(0.1);
        app.prepare_prior = Some(0.0);
        app.eta_throughput = Some(2.0);

        let eta = app
            .estimate_remaining()
            .expect("estimate")
            .remaining
            .expect("number");
        assert!((eta - 150.0).abs() <= 1.0, "{eta}");
    }

    #[test]
    fn a_phase_change_reseeds_the_shown_estimate() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-phase-reseed");
        let mut app = pro_app();
        app.status = Status::Running;
        app.stage_id = "transcription".to_string();
        app.queue.clear();
        app.current_index = None;
        app.work_done = 100.0;
        app.work_total = 400.0;
        app.timeline_duration = Some(400.0);
        app.eta_throughput = Some(2.0);
        app.prepare_prior = Some(0.0);
        app.diarize_prior = Some(0.1);
        let _ = app.update(Message::Tick);
        assert_eq!(
            app.eta_shown.map(|view| view.phase),
            Some(EtaPhase::Transcribing)
        );

        // The decode is spent and the diarizer is half way: the new phase's
        // estimate seeds the readout instead of being blended into, and capped
        // by, the old one.
        app.work_done = 400.0;
        app.stage_id = "diarization".to_string();
        app.diarize_fraction = 0.5;
        let _ = app.update(Message::Tick);
        let shown = app.eta_shown.expect("estimate");
        assert_eq!(shown.phase, EtaPhase::Diarizing);
        assert!((shown.remaining.expect("number") - 20.0).abs() <= 2.0, "{shown:?}");
    }

    #[test]
    fn diarize_prior_is_learned_from_stats() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-diarize-prior");
        let mut app = pro_app();
        app.status = Status::Running;
        app.events.lock().unwrap().push_back(Event::Done {
            txt: String::new(),
            stats: backend::Stats {
                audio_seconds: 600.0,
                diarize_seconds: 60.0,
                ..Default::default()
            },
        });
        let _ = app.update(Message::Tick);

        assert_eq!(app.diarize_prior, Some(0.1));
        assert_eq!(App::load_settings().eta_diarize, Some(0.1));
    }

    #[test]
    fn a_run_that_did_not_diarize_does_not_learn_a_rate() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-diarize-skip");
        let mut app = pro_app();
        app.status = Status::Running;
        app.diarize_prior = Some(0.06);
        // One speaker: the pipeline returns immediately and still reports the
        // leftover interval. It must not drag the rate down.
        app.events.lock().unwrap().push_back(Event::Done {
            txt: String::new(),
            stats: backend::Stats {
                audio_seconds: 600.0,
                diarize_seconds: 0.01,
                num_speakers: Some(1),
                ..Default::default()
            },
        });
        let _ = app.update(Message::Tick);
        assert_eq!(app.diarize_prior, Some(0.06));

        // Nor a cached pass, whose reported interval is under the floor.
        app.events.lock().unwrap().push_back(Event::Done {
            txt: String::new(),
            stats: backend::Stats {
                audio_seconds: 600.0,
                diarize_seconds: 0.2,
                ..Default::default()
            },
        });
        let _ = app.update(Message::Tick);
        assert_eq!(app.diarize_prior, Some(0.06));
    }

    #[test]
    fn a_stored_rate_from_a_skipped_pass_loads_as_unknown() {
        let _guard = env_lock();
        let _config = ConfigEnv::new("eta-migrate");
        let path = App::settings_path().expect("settings path");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            &path,
            r#"{"eta_diarize":0.0018,"eta_prepare":0.000061}"#,
        )
        .unwrap();

        let app = pro_app();
        assert_eq!(app.diarize_prior, None, "garbage kept as a measurement");
        assert_eq!(app.prepare_prior, None);

        // And the estimate falls back to the built-in rates rather than zero.
        assert_eq!(app.diarize_prior.unwrap_or(DIARIZE_DEFAULT), DIARIZE_DEFAULT);
    }

    #[test]
    fn decoding_label_shows_percentage_instead_of_dots() {
        // A bare stage keeps the animated dots.
        assert_eq!(
            decoding_label("Decoding", "file.wav \u{2014} Decoding....."),
            None
        );
        assert_eq!(decoding_label("decoding", "Decoding"), None);
        // A percentage-carrying label is rendered verbatim.
        assert_eq!(
            decoding_label("Decoding 42%", "file.wav \u{2014} Decoding 42%").as_deref(),
            Some("file.wav \u{2014} Decoding 42%")
        );
        assert_eq!(
            decoding_label("decoding 7%", "Decoding 7%").as_deref(),
            Some("Decoding 7%")
        );
        // Other stages never take the decoding path.
        assert_eq!(decoding_label("transcribing", "Transcribing"), None);
        assert_eq!(decoding_label("diarizing", "Diarizing 42%"), None);
    }

    #[test]
    fn button_label_is_white_only_on_dark_backgrounds() {
        // Dark mode: neutral labels go pure white.
        assert_eq!(button_label(&AppTheme::Dark.iced()), Color::WHITE);
        // Light mode: unchanged, i.e. the scheme's primary text — never white
        // on the near-white panel.
        let light = AppTheme::Light.iced();
        assert_eq!(button_label(&light), Scheme::light().text);
        assert_ne!(button_label(&light), Color::WHITE);
        // A custom dark palette is detected from the background, not from a
        // hard-coded theme identity.
        let custom = iced::Theme::custom("test".to_string(), iced::theme::Palette {
            background: Color::from_rgb(0.05, 0.05, 0.05),
            ..iced::theme::Palette::LIGHT
        });
        assert_eq!(button_label(&custom), Color::WHITE);
    }
}
