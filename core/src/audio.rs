//! Audio decode + resample to 16 kHz mono (replaces soundfile/torchaudio).

use std::path::Path;

use rubato::Resampler;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const SAMPLE_RATE: u32 = 16000;

/// Decode any supported audio file to mono f32 at 16 kHz.
pub fn decode(path: &Path) -> Result<(Vec<f32>, u32), String> {
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
    let mut mono: Vec<f32> = Vec::new();

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
                let mut sbuf = SampleBuffer::<f32>::new(frames as u64, spec);
                sbuf.copy_interleaved_ref(buf);
                let inter = sbuf.samples();
                if channels == 1 {
                    mono.extend_from_slice(&inter[..frames]);
                } else {
                    for frame in 0..frames {
                        let mut sum = 0.0f32;
                        for channel in 0..channels {
                            sum += inter[frame * channels + channel];
                        }
                        mono.push(sum / channels as f32);
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
        mono = resample(&mono, sample_rate, SAMPLE_RATE)?;
    }
    Ok((mono, SAMPLE_RATE))
}

fn resample(input: &[f32], in_rate: u32, out_rate: u32) -> Result<Vec<f32>, String> {
    let chunk = 1024;
    let mut resampler =
        rubato::FftFixedInOut::<f32>::new(in_rate as usize, out_rate as usize, chunk, 1)
            .map_err(|e| format!("resampler: {e}"))?;

    let mut output = Vec::with_capacity(input.len() * out_rate as usize / in_rate as usize + chunk);
    let mut pos = 0;
    while pos < input.len() {
        let need = resampler.input_frames_next();
        let mut block = vec![0.0f32; need];
        let avail = (input.len() - pos).min(need);
        block[..avail].copy_from_slice(&input[pos..pos + avail]);
        pos += avail;
        let res = resampler
            .process(&[block], None)
            .map_err(|e| format!("resample: {e}"))?;
        output.extend_from_slice(&res[0]);
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
                let mut sbuf = SampleBuffer::<f32>::new(frames as u64, spec);
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

pub fn duration(path: &Path) -> Result<f64, String> {
    if let Some(seconds) = duration_fast(path) {
        return Ok(seconds);
    }
    probe(path, 1).map(|(d, _)| d)
}
