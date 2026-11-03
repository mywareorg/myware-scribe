//! Audio decode + resample to 16 kHz mono (replaces soundfile/torchaudio).

use std::path::Path;
use std::time::{Duration, Instant};

use rubato::Resampler;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const SAMPLE_RATE: u32 = 16000;

/// Minimum spacing between two progress callbacks.
const REPORT_INTERVAL: Duration = Duration::from_millis(100);

/// Decode any supported audio file to mono f32 at 16 kHz.
pub fn decode(path: &Path) -> Result<(Vec<f32>, u32), String> {
    decode_with_progress(path, &mut |_| {})
}

/// Decode to mono f32 at 16 kHz, reporting progress in `0.0..=1.0`.
///
/// The callback fires at most once per [`REPORT_INTERVAL`] and once with `1.0`
/// when the file is fully decoded. Packet decoding covers `0.0..=0.8` and
/// resampling the rest. When the container carries no frame count the packet
/// phase stays silent; resampling progress is still reported.
pub fn decode_with_progress(
    path: &Path,
    progress: &mut dyn FnMut(f32),
) -> Result<(Vec<f32>, u32), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe {}: {e}", path.display()))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no audio track".to_string())?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut sample_rate = track.codec_params.sample_rate.unwrap_or(0);
    // Container frame count drives decode progress; absent for some streams.
    let total_frames = track.codec_params.n_frames;
    // Pre-allocate from container metadata: avoiding repeated growth copies is
    // a measurable win on hour-long files.
    let mut mono: Vec<f32> = Vec::with_capacity(track.codec_params.n_frames.unwrap_or(0) as usize);
    // Reuse one conversion buffer across packets instead of allocating per
    // packet (a one-hour MP3 is ~140k packets).
    let mut sample_buffer: Option<SampleBuffer<f32>> = None;
    let mut last_report: Option<Instant> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("read packet: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                if sample_rate == 0 {
                    sample_rate = spec.rate;
                }
                let frames = buf.frames();
                let channels = spec.channels.count();
                let needed = frames * channels;
                let mut sbuf = match sample_buffer.take() {
                    Some(buffer) if buffer.capacity() >= needed => buffer,
                    _ => SampleBuffer::<f32>::new(frames as u64, spec),
                };
                sbuf.copy_interleaved_ref(buf);
                let inter = sbuf.samples();
                if channels == 1 {
                    mono.extend_from_slice(&inter[..frames]);
                } else {
                    mono.reserve(frames);
                    for frame in 0..frames {
                        let mut sum = 0.0f32;
                        for channel in 0..channels {
                            sum += inter[frame * channels + channel];
                        }
                        mono.push(sum / channels as f32);
                    }
                }
                sample_buffer = Some(sbuf);
                if let Some(total) = total_frames.filter(|total| *total > 0) {
                    if last_report.is_none_or(|at| at.elapsed() >= REPORT_INTERVAL) {
                        let fraction = (mono.len() as f32 / total as f32).clamp(0.0, 1.0);
                        progress(0.8 * fraction);
                        last_report = Some(Instant::now());
                    }
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }

    if sample_rate == 0 || mono.is_empty() {
        return Err("empty audio".to_string());
    }

    if sample_rate != SAMPLE_RATE {
        mono = resample(&mono, sample_rate, SAMPLE_RATE, progress)?;
    }
    progress(1.0);
    Ok((mono, SAMPLE_RATE))
}

fn resample(
    input: &[f32],
    in_rate: u32,
    out_rate: u32,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<f32>, String> {
    // A larger chunk cuts FFT setup overhead; 4096 frames is ~93 ms at 44.1 kHz.
    let chunk = 4096;
    let mut resampler =
        rubato::FftFixedInOut::<f32>::new(in_rate as usize, out_rate as usize, chunk, 1)
            .map_err(|e| format!("resampler: {e}"))?;

    let mut output = Vec::with_capacity(input.len() * out_rate as usize / in_rate as usize + chunk);
    // Reuse one input block for the whole file.
    let mut block = vec![0.0f32; resampler.input_frames_next()];
    let mut pos = 0;
    let mut last_report = Instant::now();
    while pos < input.len() {
        let need = resampler.input_frames_next();
        if block.len() < need {
            block.resize(need, 0.0);
        }
        block[..need].fill(0.0);
        let avail = (input.len() - pos).min(need);
        block[..avail].copy_from_slice(&input[pos..pos + avail]);
        pos += avail;
        let res = resampler
            .process(&[&block[..need]], None)
            .map_err(|e| format!("resample: {e}"))?;
        output.extend_from_slice(&res[0]);
        if last_report.elapsed() >= REPORT_INTERVAL {
            let fraction = pos as f32 / input.len() as f32;
            progress(0.8 + 0.2 * fraction.clamp(0.0, 1.0));
            last_report = Instant::now();
        }
    }
    Ok(output)
}

/// Downsampled RMS waveform at the file's native sample rate (no resampling).
///
/// This is much faster than [`probe`] because it skips the 16 kHz conversion
/// and never stores the full sample buffer. Used for the UI timeline.
pub fn waveform(path: &Path, buckets: usize) -> Result<(f64, Vec<f32>), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe {}: {e}", path.display()))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no audio track".to_string())?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut sample_rate = track.codec_params.sample_rate.unwrap_or(0);
    let mut total_frames: u64 = 0;
    let mut chunk_frames: usize = 0;
    let mut chunk_sq: f64 = 0.0;
    let mut chunks: Vec<f32> = Vec::new();
    let mut sample_buffer: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("read packet: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                if sample_rate == 0 {
                    sample_rate = spec.rate;
                }
                let frames = buf.frames();
                let channels = spec.channels.count();
                let needed = frames * channels;
                let mut sbuf = match sample_buffer.take() {
                    Some(buffer) if buffer.capacity() >= needed => buffer,
                    _ => SampleBuffer::<f32>::new(frames as u64, spec),
                };
                sbuf.copy_interleaved_ref(buf);
                let inter = sbuf.samples();
                // One RMS bucket per ~10 ms of audio.
                let target = (sample_rate as usize / 100).max(1);
                for frame in 0..frames {
                    let value = if channels == 1 {
                        inter[frame]
                    } else {
                        let mut sum = 0.0f32;
                        for channel in 0..channels {
                            sum += inter[frame * channels + channel];
                        }
                        sum / channels as f32
                    };
                    chunk_sq += (value as f64) * (value as f64);
                    chunk_frames += 1;
                    total_frames += 1;
                    if chunk_frames >= target {
                        chunks.push((chunk_sq / chunk_frames as f64).sqrt() as f32);
                        chunk_sq = 0.0;
                        chunk_frames = 0;
                    }
                }
                sample_buffer = Some(sbuf);
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }

    if sample_rate == 0 || total_frames == 0 {
        return Err("empty audio".to_string());
    }
    if chunk_frames > 0 {
        chunks.push((chunk_sq / chunk_frames as f64).sqrt() as f32);
    }

    let peaks = bucketize_rms(&chunks, buckets);
    Ok((total_frames as f64 / sample_rate as f64, peaks))
}

fn bucketize_rms(chunks: &[f32], buckets: usize) -> Vec<f32> {
    let buckets = buckets.max(1);
    let mut out = vec![0.0f32; buckets];
    if chunks.is_empty() {
        return out;
    }
    if chunks.len() >= buckets {
        let per = chunks.len() as f64 / buckets as f64;
        for (i, value) in out.iter_mut().enumerate() {
            let start = (i as f64 * per) as usize;
            let end = (((i + 1) as f64 * per) as usize).min(chunks.len());
            if end > start {
                let sum: f64 = chunks[start..end]
                    .iter()
                    .map(|v| (*v as f64) * (*v as f64))
                    .sum();
                *value = (sum / (end - start) as f64).sqrt() as f32;
            }
        }
    } else {
        for (i, value) in out.iter_mut().enumerate() {
            let index = (i * chunks.len()) / buckets;
            *value = chunks[index.min(chunks.len() - 1)];
        }
    }
    out
}

/// Downsampled RMS waveform (one value per bucket) from mono samples.
pub fn rms_peaks(samples: &[f32], buckets: usize) -> Vec<f32> {
    let buckets = buckets.max(1);
    let mut rms = vec![0.0f32; buckets];
    if !samples.is_empty() {
        let per = (samples.len() as f64 / buckets as f64).max(1.0);
        for (i, value) in rms.iter_mut().enumerate() {
            let start = (i as f64 * per) as usize;
            let end = (((i + 1) as f64 * per) as usize).min(samples.len());
            if end > start {
                let mut sum = 0.0f64;
                for &s in &samples[start..end] {
                    sum += (s as f64) * (s as f64);
                }
                *value = (sum / (end - start) as f64).sqrt() as f32;
            }
        }
    }
    rms
}

/// Duration (seconds) and a downsampled RMS waveform (one value per bucket).
pub fn probe(path: &Path, buckets: usize) -> Result<(f64, Vec<f32>), String> {
    let (samples, rate) = decode(path)?;
    let duration = samples.len() as f64 / rate as f64;
    Ok((duration, rms_peaks(&samples, buckets)))
}

/// Fast duration from container metadata only (no packet decoding).
pub fn duration_fast(path: &Path) -> Option<f64> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let track = probed
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?;

    let params = &track.codec_params;
    let frames = params.n_frames?;

    if let Some(time_base) = params.time_base {
        let seconds = frames as f64 * time_base.numer as f64 / time_base.denom as f64;
        if seconds > 0.0 {
            return Some(seconds);
        }
    }

    let rate = params.sample_rate?;
    if rate == 0 {
        return None;
    }
    Some(frames as f64 / rate as f64)
}

/// Duration from container metadata only. Callers must not fall back to a full
/// decode: the pipeline reports the real duration once it has decoded the file,
/// and decoding twice per file was the main avoidable cost.
pub fn duration(path: &Path) -> Result<f64, String> {
    duration_fast(path).ok_or_else(|| "duration is not in the container metadata".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a tiny PCM WAV so progress tests need no binary fixture.
    fn write_test_wav(path: &Path, rate: u32, seconds: f32) {
        let frames = (rate as f32 * seconds) as u32;
        let mut data = Vec::with_capacity(frames as usize * 2);
        for frame in 0..frames {
            let t = frame as f32 / rate as f32;
            let sample = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            data.extend_from_slice(&((sample * i16::MAX as f32) as i16).to_le_bytes());
        }
        let mut wav = Vec::with_capacity(44 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).expect("write wav fixture");
    }

    fn temp_wav(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hippo-audio-{name}-{}.wav", std::process::id()))
    }

    #[test]
    fn decode_with_progress_is_monotonic_and_ends_at_one() {
        let path = temp_wav("progress");
        // 8 kHz forces the resample phase, so both callback ranges are used.
        write_test_wav(&path, 8000, 0.3);

        let mut fractions: Vec<f32> = Vec::new();
        let decoded = decode_with_progress(&path, &mut |fraction| fractions.push(fraction));
        let _ = std::fs::remove_file(&path);

        let (samples, rate) = decoded.expect("decode test wav");
        assert_eq!(rate, SAMPLE_RATE);
        assert!(!samples.is_empty());
        assert!(!fractions.is_empty(), "progress callback never fired");
        assert!(
            fractions.iter().all(|f| (0.0..=1.0).contains(f)),
            "fraction out of range: {fractions:?}"
        );
        assert!(
            fractions.windows(2).all(|pair| pair[1] >= pair[0]),
            "fractions decreased: {fractions:?}"
        );
        assert_eq!(fractions.last().copied(), Some(1.0));
    }
}
