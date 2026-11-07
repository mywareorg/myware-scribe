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
use std::sync::atomic::{AtomicU32, Ordering};
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
pub struct Recorder {
    stop: Sender<()>,
    level: Arc<AtomicU32>,
    started: Instant,
    path: PathBuf,
    worker: Option<JoinHandle<Result<(), String>>>,
}

impl Recorder {
    /// Start capturing, returning once the device is actually running.
    ///
    /// `device_name` is the name shown in Settings; `None` uses the system
    /// default. `path` is the WAV to create.
    pub fn start(device_name: Option<String>, path: PathBuf) -> Result<Self, String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let level = Arc::new(AtomicU32::new(0));

        let worker_level = level.clone();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            capture(device_name, worker_path, stop_rx, worker_level, ready_tx)
        });

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                stop: stop_tx,
                level,
                started: Instant::now(),
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

    pub fn elapsed(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// Peak level since the last call, 0.0..=1.0.
    pub fn take_level(&self) -> f32 {
        f32::from_bits(self.level.swap(0, Ordering::Relaxed))
    }

    /// Stop capture, finalize the WAV, and return its path and duration.
    pub fn stop(mut self) -> Result<(PathBuf, f32), String> {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(message)) => return Err(message),
                Err(_) => return Err("the recorder stopped unexpectedly".to_string()),
            }
        }
        let seconds = self.started.elapsed().as_secs_f32();
        if !self.path.is_file() {
            return Err("the recording could not be written".to_string());
        }
        Ok((self.path, seconds))
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
    level: Arc<AtomicU32>,
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
        .or_else(|| device.default_input_config().ok())
        .ok_or_else(|| "the microphone has no usable input configuration".to_string())?;
    let format = chosen.sample_format();
    let channels = chosen.channels() as usize;
    let rate = chosen.sample_rate().0;
    let config: cpal::StreamConfig = chosen.into();

    let (tx, rx) = sync_channel::<Vec<i16>>(CHANNEL_CHUNKS);
    let writer = std::thread::spawn(move || write_wav(path, rate, rx));

    let level_cb = level.clone();
    let error_level = level.clone();
    let error_cb = move |error: cpal::StreamError| {
        error_level.store(0, Ordering::Relaxed);
        hippo_transcribe_core::logging::error(format!("microphone stream error: {error}"));
    };

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, channels, tx, level_cb, error_cb),
        SampleFormat::I16 => build::<i16>(&device, &config, channels, tx, level_cb, error_cb),
        SampleFormat::U16 => build::<u16>(&device, &config, channels, tx, level_cb, error_cb),
        SampleFormat::I32 => build::<i32>(&device, &config, channels, tx, level_cb, error_cb),
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

fn prefer_rate(device: &cpal::Device, rate: u32) -> Option<cpal::SupportedStreamConfig> {
    let ranges = device.supported_input_configs().ok()?;
    for range in ranges {
        if range.min_sample_rate().0 <= rate && range.max_sample_rate().0 >= rate {
            return Some(range.with_sample_rate(cpal::SampleRate(rate)));
        }
    }
    None
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    tx: SyncSender<Vec<i16>>,
    level: Arc<AtomicU32>,
    error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + Sample,
    f32: FromSample<T>,
{
    let channels = channels.max(1);
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut mono = Vec::with_capacity(data.len() / channels);
                let mut peak = 0.0f32;
                for frame in data.chunks(channels) {
                    let mut sum = 0.0f32;
                    for sample in frame {
                        sum += f32::from_sample(*sample);
                    }
                    let value = (sum / channels as f32).clamp(-1.0, 1.0);
                    peak = peak.max(value.abs());
                    mono.push((value * i16::MAX as f32) as i16);
                }
                if mono.is_empty() {
                    return;
                }
                level.fetch_max(peak.to_bits(), Ordering::Relaxed);
                let _ = tx.try_send(mono);
            },
            error,
            None,
        )
        .map_err(|error| format!("could not open the microphone: {error}"))
}

/// The recording thread's writer: a WAV whose sizes are patched on finish.
fn write_wav(path: PathBuf, rate: u32, rx: Receiver<Vec<i16>>) {
    let Ok(mut wav) = WavWriter::create(&path, rate) else {
        return;
    };
    while let Ok(chunk) = rx.recv() {
        if wav.write(&chunk).is_err() {
            break;
        }
    }
    let _ = wav.finish();
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
