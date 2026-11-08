//! Microphone capture to a mono WAV.
//!
//! The capture stream is built and owned by a dedicated thread: cpal's stream
//! is not `Send` on every backend, and the UI never needs to touch it. The
//! thread reports one of two things — that capture started, or why it could
//! not — then blocks until the stop channel is signalled.
//!
//! Samples leave the audio callback through a bounded channel and are written
//! by a second thread, so the callback only converts, never touches the disk.
//! A recording that outlasts the channel (it should not; 48 kHz mono is about
//! 96 KB/s) drops chunks rather than blocking the device.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat};

/// Preferred capture rate. Devices that cannot do it keep their own rate, which
/// the WAV header records; the pipeline resamples either way.
pub const TARGET_RATE: u32 = 48_000;

/// How many converted chunks may queue before the callback starts dropping.
const CHANNEL_CHUNKS: usize = 256;

/// A recording in progress.
///
/// The stream opens as soon as [`Recorder::start`] returns, but nothing is
/// written until [`Recorder::begin_capture`] is called: the countdown before a
/// recording runs on the live meter, while the writer drops chunks. Elapsed
/// time and the final duration are measured from that same moment.
pub struct Recorder {
    stop: Sender<()>,
    meter: Arc<MeterShared>,
    /// True once [`Recorder::begin_capture`] has armed the writer.
    capture: Arc<AtomicBool>,
    /// When capture was armed; `None` until then.
    capture_started: Option<Instant>,
    path: PathBuf,
    worker: Option<JoinHandle<Result<(), String>>>,
}

/// A snapshot of the microphone meter for one view build.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReading {
    /// Smoothed level above the room's noise floor, 0.0..=1.0.
    pub level: f32,
    /// The loudest level recently seen, held briefly so a transient is visible.
    pub peak: f32,
    /// True while the input has recently reached full scale.
    pub clipping: bool,
}

/// The callback's output, read by the view.
///
/// The smoothing itself lives in the audio callback, next to the samples it
/// measures: the view is built on whatever events happen to arrive, so a
/// snapshot is published continuously and reading it never consumes it.
#[derive(Default)]
struct MeterShared {
    level: AtomicU32,
    peak: AtomicU32,
    clipping: AtomicBool,
}

impl MeterShared {
    fn publish(&self, reading: MeterReading) {
        self.level.store(reading.level.to_bits(), Ordering::Relaxed);
        self.peak.store(reading.peak.to_bits(), Ordering::Relaxed);
        self.clipping.store(reading.clipping, Ordering::Relaxed);
    }

    fn silence(&self) {
        self.level.store(0, Ordering::Relaxed);
        self.peak.store(0, Ordering::Relaxed);
        self.clipping.store(false, Ordering::Relaxed);
    }

    fn reading(&self) -> MeterReading {
        MeterReading {
            level: f32::from_bits(self.level.load(Ordering::Relaxed)),
            peak: f32::from_bits(self.peak.load(Ordering::Relaxed)),
            clipping: self.clipping.load(Ordering::Relaxed),
        }
    }
}

impl Recorder {
    /// Start capturing, returning once the device is actually running.
    ///
    /// `device_name` is the name shown in Settings; `None` uses the system
    /// default. `path` is the WAV to create.
    pub fn start(device_name: Option<String>, path: PathBuf) -> Result<Self, String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let meter = Arc::new(MeterShared::default());
        let capture_flag = Arc::new(AtomicBool::new(false));

        let worker_meter = meter.clone();
        let worker_capture = capture_flag.clone();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            capture(
                device_name,
                worker_path,
                stop_rx,
                worker_meter,
                worker_capture,
                ready_tx,
            )
        });

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                stop: stop_tx,
                meter,
                capture: capture_flag,
                capture_started: None,
                path,
                worker: Some(worker),
            }),
            Ok(Err(message)) => {
                let _ = worker.join();
                Err(message)
            }
            Err(_) => {
                let _ = stop_tx.send(());
                let _ = worker.join();
                Err("the microphone did not start".to_string())
            }
        }
    }

    /// Arm the writer. Chunks before this are discarded, so nothing from the
    /// countdown (or the capture stream's warm-up) reaches the WAV. Idempotent.
    pub fn begin_capture(&mut self) {
        if self.capture_started.is_none() {
            self.capture_started = Some(Instant::now());
            self.capture.store(true, Ordering::SeqCst);
        }
    }

    /// Seconds since capture was armed; zero while still counting down.
    pub fn elapsed(&self) -> f32 {
        self.capture_started
            .map(|started| started.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// The latest meter snapshot. Reading it does not reset the meter: the
    /// callback keeps moving while the view is idle or rebuilt.
    pub fn meter(&self) -> MeterReading {
        self.meter.reading()
    }

    /// Stop capture, finalize the WAV, and return its path and duration.
    ///
    /// A recorder stopped before capture began reports zero seconds rather than
    /// the time the stream was open.
    pub fn stop(mut self) -> Result<(PathBuf, f32), String> {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(message)) => return Err(message),
                Err(_) => return Err("the recorder stopped unexpectedly".to_string()),
            }
        }
        let seconds = self
            .capture_started
            .map(|started| started.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        if !self.path.is_file() {
            return Err("the recording could not be written".to_string());
        }
        Ok((self.path, seconds))
    }

    /// Stop the stream without keeping anything: used when the user cancels
    /// during the countdown, or the recording session is dropped.
    pub fn cancel(mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A recorder with no microphone behind it, for tests that drive the UI state
/// machine. `captured_seconds` arms it at that elapsed time; `None` leaves it
/// waiting for [`Recorder::begin_capture`].
#[cfg(test)]
pub(crate) fn test_recorder(path: PathBuf, captured_seconds: Option<f32>) -> Recorder {
    let (stop, _stop_rx) = std::sync::mpsc::channel();
    Recorder {
        stop,
        meter: Arc::new(MeterShared::default()),
        capture: Arc::new(AtomicBool::new(captured_seconds.is_some())),
        capture_started: captured_seconds
            .map(|seconds| Instant::now() - Duration::from_secs_f32(seconds)),
        path,
        worker: None,
    }
}

/// Names of the input devices, for the Settings picker.
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .input_devices()
        .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

/// The recording thread: build the stream, report, wait, finalize.
fn capture(
    device_name: Option<String>,
    path: PathBuf,
    stop: Receiver<()>,
    meter: Arc<MeterShared>,
    capture: Arc<AtomicBool>,
    ready: Sender<Result<(), String>>,
) -> Result<(), String> {
    let host = cpal::default_host();
    let device = match device_name.as_deref() {
        Some(name) => host
            .input_devices()
            .map_err(|error| format!("could not list microphones: {error}"))?
            .find(|device| device.name().ok().as_deref() == Some(name)),
        None => None,
    }
    .or_else(|| host.default_input_device())
    .ok_or_else(|| "no microphone was found".to_string())?;

    let chosen = prefer_rate(&device, TARGET_RATE)
        .or_else(|| {
            device
                .default_input_config()
                .ok()
                .filter(|config| format_rank(config.sample_format()) < u8::MAX)
        })
        .ok_or_else(|| {
            format!(
                "the microphone ({}) has no usable input configuration",
                device.name().unwrap_or_else(|_| "unknown".to_string())
            )
        })?;
    let format = chosen.sample_format();
    let channels = chosen.channels() as usize;
    let rate = chosen.sample_rate().0;
    hippo_transcribe_core::logging::info(format!(
        "microphone {}: {format:?}, {channels} channel(s), {rate} Hz",
        device.name().unwrap_or_else(|_| "default".to_string())
    ));
    let config: cpal::StreamConfig = chosen.into();

    let (tx, rx) = sync_channel::<Vec<i16>>(CHANNEL_CHUNKS);
    let writer_capture = capture.clone();
    let writer = std::thread::spawn(move || write_wav(path, rate, writer_capture, rx));

    let error_meter = meter.clone();
    let error_cb = move |error: cpal::StreamError| {
        error_meter.silence();
        hippo_transcribe_core::logging::error(format!("microphone stream error: {error}"));
    };

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, channels, rate, tx, meter, error_cb),
        SampleFormat::I16 => build::<i16>(&device, &config, channels, rate, tx, meter, error_cb),
        SampleFormat::U16 => build::<u16>(&device, &config, channels, rate, tx, meter, error_cb),
        SampleFormat::I32 => build::<i32>(&device, &config, channels, rate, tx, meter, error_cb),
        SampleFormat::U8 => build::<u8>(&device, &config, channels, rate, tx, meter, error_cb),
        SampleFormat::I8 => build::<i8>(&device, &config, channels, rate, tx, meter, error_cb),
        other => Err(format!("unsupported microphone sample format: {other:?}")),
    };

    let stream = match stream {
        Ok(stream) => stream,
        Err(message) => {
            // The writer ends as soon as the callback's sender is dropped.
            let _ = ready.send(Err(message));
            return Ok(());
        }
    };

    if let Err(error) = stream.play() {
        let _ = ready.send(Err(format!("could not start the microphone: {error}")));
        return Ok(());
    }

    let _ = ready.send(Ok(()));
    // Block until the UI stops us. The sender is dropped when `Recorder` ends.
    let _ = stop.recv();
    drop(stream);
    // Dropping the stream drops the callback and therefore the only sender.
    let _ = writer.join();
    Ok(())
}

/// Pick a 48 kHz configuration in a format the converter understands.
///
/// Range order is whatever ALSA reports, and the first 48 kHz range can be
/// `U8` (the analog HDA device does exactly that). Taking it blindly rejected a
/// perfectly usable microphone, so only formats [`build`] can convert are
/// considered, and the order prefers float then 16-bit.
fn prefer_rate(device: &cpal::Device, rate: u32) -> Option<cpal::SupportedStreamConfig> {
    let ranges = device.supported_input_configs().ok()?;
    // Prefer the channel count the platform mixer advertises by default. A
    // capture device often lists several counts for every rate, and the first
    // one is not necessarily the one the system is configured for.
    let default_channels = device
        .default_input_config()
        .ok()
        .map(|config| config.channels());
    let mut candidates: Vec<cpal::SupportedStreamConfigRange> = ranges
        .filter(|range| {
            range.min_sample_rate().0 <= rate
                && range.max_sample_rate().0 >= rate
                && format_rank(range.sample_format()) < u8::MAX
        })
        .collect();
    candidates.sort_by_key(|range| {
        (
            format_rank(range.sample_format()),
            u8::from(
                default_channels.is_some_and(|channels| channels != range.channels()),
            ),
            range.channels(),
        )
    });
    candidates
        .first()
        .map(|range| range.with_sample_rate(cpal::SampleRate(rate)))
}

/// Preference order for input formats: float, then 16-bit, then the rest the
/// converter handles.
fn format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::U16 => 2,
        SampleFormat::I32 => 3,
        SampleFormat::U8 => 4,
        SampleFormat::I8 => 5,
        _ => u8::MAX,
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    rate: u32,
    tx: SyncSender<Vec<i16>>,
    meter: Arc<MeterShared>,
    error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + Sample,
    f32: FromSample<T>,
{
    let channels = channels.max(1);
    let rate = rate.max(1);
    let mut engine = LevelEngine::default();
    // The same window the meter ignores must not be written either: a device
    // waking from suspend can start with tens of milliseconds of full-scale
    // samples, which would become the loudness pass's peak and a click in the
    // transcript. Dropping them costs 150 ms at the top of the take.
    let mut warmup_frames = (WARMUP_SECONDS * rate as f32).ceil() as usize;
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let frames = data.len() / channels;
                if frames == 0 {
                    return;
                }
                let seconds = frames as f32 / rate as f32;
                let mut mono = Vec::with_capacity(frames);
                let mut peak = 0.0f32;
                // Clipping is judged before the channels are mixed down: a
                // single overloaded channel is still an overload, even if the
                // mono sum lands below full scale.
                let mut clip_peak = 0.0f32;
                for frame in data.chunks(channels) {
                    let mut sum = 0.0f32;
                    for sample in frame {
                        let sample = f32::from_sample(*sample);
                        clip_peak = clip_peak.max(sample.abs());
                        sum += sample;
                    }
                    let value = (sum / channels as f32).clamp(-1.0, 1.0);
                    peak = peak.max(value.abs());
                    if warmup_frames > 0 {
                        warmup_frames -= 1;
                    } else {
                        mono.push((value * i16::MAX as f32) as i16);
                    }
                }
                meter.publish(engine.update(peak, clip_peak, seconds));
                if mono.is_empty() {
                    return;
                }
                let _ = tx.try_send(mono);
            },
            error,
            None,
        )
        .map_err(|error| format!("could not open the microphone: {error}"))
}

/// The meter's reference span: decibels above the room's noise floor that fill
/// the bar. Ordinary speech peaks 25–35 dB above a quiet room, so the top of
/// the bar is a loud voice, not an impossible one.
const METER_SPAN_DB: f32 = 40.0;

/// Rise and fall of the displayed envelope. Speech attacks quickly; the long
/// release keeps the bar alive between syllables instead of flickering.
const ATTACK_SECONDS: f32 = 0.02;
const RELEASE_SECONDS: f32 = 0.30;

/// One block of the noise-floor estimate, and how many blocks it remembers.
/// A quarter second is long enough to contain a pause between words, and eight
/// seconds means a room that gets noisier is picked up without a restart.
const FLOOR_BLOCK_SECONDS: f32 = 0.25;
const FLOOR_BLOCKS: usize = 32;

/// The stream's own start is not signal: devices can emit a click or a burst of
/// full-scale samples while the capture path settles. Ignore that window so it
/// neither latches "clipping" nor seeds the noise floor.
const WARMUP_SECONDS: f32 = 0.15;

/// The floor before anything has been heard. A single quiet block replaces one
/// slot, so the guess is corrected within a quarter second of a pause; it only
/// matters if recording starts mid-sentence.
const FLOOR_SEED_DB: f32 = -55.0;

/// Digital silence has no meaningful floor; keep the reference out of `-inf`.
const FLOOR_MIN_DB: f32 = -80.0;

/// How long a peak is held before the tick starts falling, and how fast it
/// falls afterwards.
const PEAK_HOLD_SECONDS: f32 = 1.5;
const PEAK_FALL_PER_SECOND: f32 = 0.6;

/// What counts as full scale, and how long the overload warning stays up.
const CLIP_PEAK: f32 = 0.98;
const CLIP_HOLD_SECONDS: f32 = 2.0;

/// The stateful meter: a noise-floor reference, a smoothed envelope, a held
/// peak, and a clip latch.
///
/// The floor is the quietest block of the last eight seconds. Speech cannot
/// lift it, so the scale stays put while the user talks; the room's own noise
/// does, so moving to a noisy room does not pin the bar at full.
struct LevelEngine {
    warmup: f32,
    floor: [f32; FLOOR_BLOCKS],
    slot: usize,
    block_seconds: f32,
    block_peak: f32,
    floor_db: f32,
    level: f32,
    hold: f32,
    hold_age: f32,
    clip_seconds: f32,
}

impl Default for LevelEngine {
    fn default() -> Self {
        Self {
            warmup: 0.0,
            floor: [FLOOR_SEED_DB; FLOOR_BLOCKS],
            slot: 0,
            block_seconds: 0.0,
            block_peak: 0.0,
            floor_db: FLOOR_SEED_DB,
            level: 0.0,
            hold: 0.0,
            hold_age: 0.0,
            clip_seconds: 0.0,
        }
    }
}

impl LevelEngine {
    /// Fold one callback's peak into the meter. `peak` is the mono peak that is
    /// being recorded; `clip_peak` is the raw per-channel peak used to catch an
    /// overload that the mixdown would hide. `seconds` is the callback's
    /// duration, which makes the smoothing independent of buffer size.
    fn update(&mut self, peak: f32, clip_peak: f32, seconds: f32) -> MeterReading {
        let seconds = seconds.clamp(0.0, 1.0);

        // The first buffers say more about the driver than about the room.
        if self.warmup < WARMUP_SECONDS {
            self.warmup += seconds;
            return MeterReading::default();
        }

        // The floor is a rolling minimum of block peaks, not a follower: a
        // minimum cannot be dragged upward by the speech it is measuring.
        self.block_peak = self.block_peak.max(peak.max(0.0));
        self.block_seconds += seconds;
        if self.block_seconds >= FLOOR_BLOCK_SECONDS {
            let block_db = decibels(self.block_peak).clamp(FLOOR_MIN_DB, 0.0);
            self.floor[self.slot] = block_db;
            self.slot = (self.slot + 1) % FLOOR_BLOCKS;
            self.floor_db = self.floor.iter().copied().fold(f32::INFINITY, f32::min);
            self.block_seconds = 0.0;
            self.block_peak = 0.0;
        }

        let target = ((decibels(peak) - self.floor_db) / METER_SPAN_DB).clamp(0.0, 1.0);
        let tau = if target > self.level {
            ATTACK_SECONDS
        } else {
            RELEASE_SECONDS
        };
        let smoothing = 1.0 - (-seconds / tau).exp();
        self.level = (self.level + (target - self.level) * smoothing).clamp(0.0, 1.0);

        if self.level >= self.hold {
            self.hold = self.level;
            self.hold_age = 0.0;
        } else {
            self.hold_age += seconds;
            if self.hold_age > PEAK_HOLD_SECONDS {
                self.hold = (self.hold - PEAK_FALL_PER_SECOND * seconds).max(self.level);
            }
        }

        if clip_peak >= CLIP_PEAK {
            self.clip_seconds = CLIP_HOLD_SECONDS;
        } else {
            self.clip_seconds = (self.clip_seconds - seconds).max(0.0);
        }

        MeterReading {
            level: self.level,
            peak: self.hold,
            clipping: self.clip_seconds > 0.0,
        }
    }
}

/// Peak amplitude as decibels relative to full scale. Zero is `-inf`, which the
/// caller clamps or lets fall to an empty bar.
fn decibels(peak: f32) -> f32 {
    if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        f32::NEG_INFINITY
    }
}

/// The recording thread's writer: a WAV whose sizes are patched on finish.
///
/// Chunks that arrive before capture is armed are dropped here rather than in
/// the callback, so the stream can keep its meter alive through the countdown.
fn write_wav(path: PathBuf, rate: u32, capture: Arc<AtomicBool>, rx: Receiver<Vec<i16>>) {
    let Ok(mut wav) = WavWriter::create(&path, rate) else {
        return;
    };
    while let Ok(chunk) = rx.recv() {
        if write_chunk(&mut wav, &capture, &chunk).is_err() {
            break;
        }
    }
    let _ = wav.finish();
}

/// Write one chunk if capture has begun. Split out so the arm filter can be
/// tested without a stream.
fn write_chunk(wav: &mut WavWriter, capture: &AtomicBool, chunk: &[i16]) -> std::io::Result<()> {
    if !capture.load(Ordering::SeqCst) {
        return Ok(());
    }
    wav.write(chunk)
}

/// A single-channel 16-bit WAV.
///
/// The header is written with zero sizes and rewritten when the file is
/// finished: the length is not known while recording, and a streamed WAV cannot
/// seek back in a callback-owned buffer.
pub struct WavWriter {
    file: BufWriter<File>,
    data: u32,
}

const HEADER_LEN: u32 = 44;

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> std::io::Result<Self> {
        let file = File::create(path)?;
        let mut writer = Self {
            file: BufWriter::new(file),
            data: 0,
        };
        writer.write_header(rate, 0)?;
        Ok(writer)
    }

    fn write_header(&mut self, rate: u32, data: u32) -> std::io::Result<()> {
        let channels = 1u16;
        let bits = 16u16;
        let block_align = channels * bits / 8;
        self.file.write_all(b"RIFF")?;
        self.file.write_all(&(HEADER_LEN - 8 + data).to_le_bytes())?;
        self.file.write_all(b"WAVE")?;
        self.file.write_all(b"fmt ")?;
        self.file.write_all(&16u32.to_le_bytes())?;
        self.file.write_all(&1u16.to_le_bytes())?;
        self.file.write_all(&channels.to_le_bytes())?;
        self.file.write_all(&rate.to_le_bytes())?;
        self.file.write_all(&(rate * block_align as u32).to_le_bytes())?;
        self.file.write_all(&block_align.to_le_bytes())?;
        self.file.write_all(&bits.to_le_bytes())?;
        self.file.write_all(b"data")?;
        self.file.write_all(&data.to_le_bytes())?;
        Ok(())
    }

    pub fn write(&mut self, samples: &[i16]) -> std::io::Result<()> {
        for sample in samples {
            self.file.write_all(&sample.to_le_bytes())?;
        }
        self.data = self.data.saturating_add((samples.len() * 2) as u32);
        Ok(())
    }

    /// Patch the two size fields and close the file.
    pub fn finish(mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let mut file = self.file.into_inner().map_err(|error| error.into_error())?;
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&(HEADER_LEN - 8 + self.data).to_le_bytes())?;
        file.seek(SeekFrom::Start(40))?;
        file.write_all(&self.data.to_le_bytes())?;
        file.flush()?;
        Ok(())
    }
}

/// A file name for a new recording, in local time when the platform can say it.
pub fn default_name() -> String {
    let now = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!(
        "recording-{:04}-{:02}-{:02}-{:02}{:02}{:02}.wav",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hippo-recording-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn db_to_peak(db: f32) -> f32 {
        10.0f32.powf(db / 20.0)
    }

    /// Feed `seconds` of a steady tone at `db` dBFS in 10 ms steps. Returns the
    /// last reading, which is what the view would have seen.
    fn feed(engine: &mut LevelEngine, db: f32, seconds: f32, dt: f32) -> MeterReading {
        let peak = db_to_peak(db);
        let steps = (seconds / dt).round() as usize;
        let mut last = MeterReading::default();
        for _ in 0..steps {
            last = engine.update(peak, peak, dt);
        }
        last
    }

    #[test]
    fn the_meter_ignores_the_stream_start() {
        let mut engine = LevelEngine::default();
        // A full-scale burst for the first tenth of a second is a device
        // settling, not an overload.
        for _ in 0..5 {
            assert_eq!(engine.update(1.0, 1.0, 0.02), MeterReading::default());
        }
        assert_eq!(engine.clip_seconds, 0.0);
        assert_eq!(engine.floor_db, FLOOR_SEED_DB);

        // Speech right afterwards reads normally and does not inherit a latch.
        let speech = feed(&mut engine, -20.0, 0.3, 0.01);
        assert!(!speech.clipping, "{speech:?}");
        assert!(speech.level > 0.5, "{speech:?}");
    }

    #[test]
    fn the_meter_moves_with_speech_above_the_room() {
        let mut engine = LevelEngine::default();
        let silent = engine.update(0.0, 0.0, 0.01);
        assert_eq!(silent.level, 0.0);

        let room = feed(&mut engine, -55.0, 1.0, 0.01);
        assert!(room.level < 0.05, "{room:?}");

        let speech = feed(&mut engine, -20.0, 0.3, 0.01);
        assert!(speech.level > 0.7, "{speech:?}");
        assert!(speech.level > room.level + 0.5);
    }

    #[test]
    fn the_meter_releases_slowly_and_keeps_a_peak_tick() {
        let mut engine = LevelEngine::default();
        let loud = feed(&mut engine, -20.0, 0.5, 0.01);

        // A tenth of a second of silence barely moves the bar: syllable gaps
        // must not make it flicker.
        let early = feed(&mut engine, -90.0, 0.10, 0.01);
        assert!(early.level > loud.level * 0.55, "{early:?} vs {loud:?}");
        assert!(early.peak > early.level + 0.1, "{early:?}");

        // It does fall away once the phrase is over, and so does the tick.
        let late = feed(&mut engine, -90.0, 3.0, 0.01);
        assert!(late.level < 0.05, "{late:?}");
        assert!(late.peak < 0.2, "{late:?}");
    }

    #[test]
    fn the_floor_rises_when_the_room_gets_noisier() {
        let mut engine = LevelEngine::default();
        feed(&mut engine, -65.0, 1.0, 0.01);
        assert!(engine.floor_db < -60.0, "{}", engine.floor_db);

        // Ten seconds of a persistent hum at -30 dBFS. The reference climbs to
        // meet it, so the room itself reads quiet instead of pinning the bar.
        let hum = feed(&mut engine, -30.0, 10.0, 0.01);
        assert!(engine.floor_db > -31.0, "{}", engine.floor_db);
        assert!(hum.level < 0.1, "{hum:?}");

        // Speech twenty decibels above the new room reads mid-scale, not off
        // the top, because the reference moved with the room.
        let speech = feed(&mut engine, -10.0, 0.3, 0.01);
        assert!((0.45..0.55).contains(&speech.level), "{speech:?}");
    }

    #[test]
    fn the_floor_drops_with_the_room() {
        let mut engine = LevelEngine::default();
        feed(&mut engine, -30.0, 9.0, 0.01);
        assert!(engine.floor_db > -31.0, "{}", engine.floor_db);

        // Downward movement needs one block, and a block that straddles the
        // change still carries the loud part, so allow half a second. Only
        // upward movement has to wait for the old blocks to age out.
        feed(&mut engine, -65.0, 0.6, 0.01);
        assert!(engine.floor_db < -60.0, "{}", engine.floor_db);
    }

    #[test]
    fn speech_alone_does_not_lift_the_floor() {
        let mut engine = LevelEngine::default();
        feed(&mut engine, -65.0, 1.0, 0.01);
        let settled = engine.floor_db;

        // Ten seconds of loud speech with the pauses an ordinary speaker takes.
        // The old fixed-offset meter would have climbed with the voice; a
        // rolling minimum stays on the room.
        for _ in 0..10 {
            feed(&mut engine, -20.0, 0.6, 0.01);
            feed(&mut engine, -65.0, 0.4, 0.01);
        }
        assert!(
            engine.floor_db < settled + 1.0,
            "{settled} -> {}",
            engine.floor_db
        );

        // The scale has not compressed: the next phrase still reaches the top.
        let speech = feed(&mut engine, -20.0, 0.3, 0.01);
        assert!(speech.level > 0.8, "{speech:?}");
    }

    #[test]
    fn clipping_is_flagged_even_when_the_bar_is_not_full() {
        let mut engine = LevelEngine::default();
        feed(&mut engine, -25.0, 9.0, 0.01);
        assert!(engine.floor_db > -26.0, "{}", engine.floor_db);

        // Full scale in a room that is already loud: only 25 dB above the
        // floor, so the normalized bar sits mid-scale while the input clips.
        let hot = engine.update(1.0, 1.0, 0.02);
        assert!(hot.clipping, "{hot:?}");
        assert!(hot.level < 0.5, "{hot:?}");

        // The warning is a latch, not a single-frame blip, and then it clears.
        let cooling = feed(&mut engine, -25.0, 1.0, 0.01);
        assert!(cooling.clipping, "{cooling:?}");
        let cool = feed(&mut engine, -25.0, 1.5, 0.01);
        assert!(!cool.clipping, "{cool:?}");
    }

    #[test]
    fn a_peak_below_full_scale_is_not_clipping() {
        let mut engine = LevelEngine::default();
        let reading = feed(&mut engine, -20.0, 1.0, 0.01);
        assert!(!reading.clipping, "{reading:?}");
    }

    #[test]
    fn format_rank_prefers_float_then_16_bit() {
        assert!(format_rank(SampleFormat::F32) < format_rank(SampleFormat::I16));
        assert!(format_rank(SampleFormat::I16) < format_rank(SampleFormat::U8));
        assert_eq!(format_rank(SampleFormat::I64), u8::MAX);
    }

    #[test]
    #[ignore = "needs a microphone"]
    fn records_a_second_from_the_default_device() {
        let dir = scratch("capture");
        let path = dir.join("take.wav");
        let recorder = Recorder::start(None, path.clone()).expect("start the microphone");
        std::thread::sleep(std::time::Duration::from_millis(700));
        let (path, seconds) = recorder.stop().expect("stop");
        assert!(seconds > 0.4);
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes > 44, "only the header was written: {bytes}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_written_wav_decodes_back() {
        let dir = scratch("wave");
        let path = dir.join("take.wav");
        let mut wav = WavWriter::create(&path, 48_000).unwrap();
        // 0.25 s of a quiet ramp.
        let samples: Vec<i16> = (0..12_000).map(|index| (index % 200) as i16).collect();
        wav.write(&samples).unwrap();
        wav.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        assert_eq!(rate, 48_000);

        let (decoded, _) = hippo_transcribe_core::audio::decode(&path).unwrap();
        assert!((decoded.len() as f32 / 16_000.0 - 0.25).abs() < 0.01);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_writer_drops_chunks_before_capture_begins() {
        let dir = scratch("writer-arm");
        let path = dir.join("take.wav");
        let capture = AtomicBool::new(false);
        let mut wav = WavWriter::create(&path, 48_000).unwrap();

        // The countdown's chunks must not reach the file.
        write_chunk(&mut wav, &capture, &[1i16; 4_800]).unwrap();
        capture.store(true, Ordering::SeqCst);
        write_chunk(&mut wav, &capture, &[2i16; 9_600]).unwrap();
        wav.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data, 9_600 * 2, "only the armed chunk was written");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unarmed_recorder_reports_no_elapsed_time() {
        let dir = scratch("recorder-arm");
        let path = dir.join("take.wav");
        std::fs::write(&path, b"placeholder").unwrap();

        let mut recorder = test_recorder(path.clone(), None);
        assert_eq!(recorder.elapsed(), 0.0);

        // Stopping before the countdown ends reports zero, not stream time.
        let (stopped, seconds) = recorder.stop().unwrap();
        assert_eq!(stopped, path);
        assert_eq!(seconds, 0.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn arming_a_recorder_starts_its_clock() {
        let dir = scratch("recorder-begin");
        let path = dir.join("take.wav");
        std::fs::write(&path, b"placeholder").unwrap();

        let mut recorder = test_recorder(path, None);
        recorder.begin_capture();
        std::thread::sleep(Duration::from_millis(15));
        assert!(recorder.elapsed() > 0.0);

        let (_, seconds) = recorder.stop().unwrap();
        assert!(seconds > 0.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_removes_the_unfinished_file() {
        let dir = scratch("recorder-cancel");
        let path = dir.join("take.wav");
        std::fs::write(&path, b"placeholder").unwrap();

        test_recorder(path.clone(), None).cancel();
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_header_sizes_are_patched() {
        let dir = scratch("header");
        let path = dir.join("take.wav");
        let mut wav = WavWriter::create(&path, 16_000).unwrap();
        wav.write(&[1, 2, 3, 4]).unwrap();
        wav.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data, 8);
        assert_eq!(riff, 36 + 8);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_name_ends_in_wav() {
        let name = default_name();
        assert!(name.starts_with("recording-"));
        assert!(name.ends_with(".wav"));
        assert_eq!(name.len(), "recording-2026-11-06-153000.wav".len());
    }
}
