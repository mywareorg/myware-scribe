//! Native-rate audio streaming for playback.
//!
//! The GUI needs a sample stream at the file's own rate and channel count, not
//! the 16 kHz mono the ASR pipeline wants. It cannot use `rodio::Decoder`: that
//! decoder feeds *every* packet to the audio codec, so the video packets of a
//! normal MP4 reach the AAC decoder, which returns `Unsupported` for some of
//! them and ends playback after a fraction of a second. This module owns the
//! track selection so only the chosen audio track is decoded, and exposes a
//! plain `Iterator<Item = f32>` that a player can wrap.

use std::fs::File;
use std::time::Duration;

use symphonia::core::audio::{AudioBufferRef, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units;

/// A decoded audio track at its native sample rate and channel count.
pub struct AudioStream {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    spec: SignalSpec,
    duration: Option<Duration>,
    samples: Vec<f32>,
    pos: usize,
    done: bool,
}

impl AudioStream {
    /// Open `path` and decode its first audio track.
    ///
    /// The file is opened directly rather than through a `Read` wrapper: the
    /// ISO-BMFF and Matroska readers require a known byte length before they
    /// will open a seekable stream.
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
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

        // Prefer a track that also reports a sample rate; a container can list
        // an unknown codec first.
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some())
            .or_else(|| {
                format
                    .tracks()
                    .iter()
                    .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            })
            .ok_or_else(|| "no audio track".to_string())?;

        let track_id = track.id;
        let params = track.codec_params.clone();
        let duration = params
            .time_base
            .zip(params.n_frames)
            .map(|(base, frames)| base.calc_time(frames))
            .map(Duration::from);

        let mut decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| format!("decoder: {e}"))?;

        // Decode the first audio packet so `channels`/`sample_rate` are known
        // before iteration starts; the UI needs them when the player is built.
        let (spec, buffer) = loop {
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::ResetRequired) => {
                    return Err("empty audio".to_string());
                }
                Err(e) => return Err(format!("read packet: {e}")),
            };
            if packet.track_id() != track_id {
                continue;
            }
            match decoder.decode(&packet) {
                Ok(decoded) if decoded.frames() > 0 => break (decoded.spec().to_owned(), decoded),
                Ok(_) => continue,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => return Err(format!("decode: {e}")),
            }
        };

        let samples = Self::interleave(&buffer, &spec);
        Ok(Self {
            format,
            decoder,
            track_id,
            spec,
            duration,
            samples,
            pos: 0,
            done: false,
        })
    }

    fn interleave(buffer: &AudioBufferRef<'_>, spec: &SignalSpec) -> Vec<f32> {
        let mut sample_buffer =
            SampleBuffer::<f32>::new(units::Duration::from(buffer.capacity() as u64), *spec);
        sample_buffer.copy_interleaved_ref(buffer.clone());
        sample_buffer.samples().to_vec()
    }

    pub fn sample_rate(&self) -> u32 {
        self.spec.rate
    }

    pub fn channels(&self) -> u16 {
        self.spec.channels.count() as u16
    }

    /// Container duration when the header carries one.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Decode the next audio packet into the sample buffer.
    fn fill(&mut self) -> bool {
        if self.done {
            return false;
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                // End of stream, and an unrecoverable read error, both end
                // playback rather than raising: a partially playable file is
                // still worth hearing.
                Err(_) => {
                    self.done = true;
                    return false;
                }
            };
            // Packets from other tracks (video, a second audio track) must not
            // reach this decoder.
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    if decoded.frames() == 0 {
                        continue;
                    }
                    decoded.spec().clone_into(&mut self.spec);
                    self.samples = Self::interleave(&decoded, &self.spec);
                    self.pos = 0;
                    return !self.samples.is_empty();
                }
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(_) => {
                    self.done = true;
                    return false;
                }
            }
        }
    }

    /// Seek to `seconds` on the audio timeline.
    pub fn seek(&mut self, seconds: f64) -> Result<(), String> {
        // Clamp to the known duration: the demuxers report an out-of-range
        // error rather than saturating, and a seek past the end is a no-op the
        // player should not surface.
        let mut target = seconds.max(0.0);
        if let Some(duration) = self.duration {
            // Stay just short of the end: the demuxers treat a seek to exactly
            // the last timestamp as out of range.
            target = target.min((duration.as_secs_f64() - 0.05).max(0.0));
        }
        let at = Duration::from_secs_f64(target);
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: at.into(),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| format!("seek: {e}"))?;
        // The demuxer moved without the decoder knowing; reset keeps them in
        // sync and drops the pre-seek buffer.
        self.decoder.reset();
        self.samples.clear();
        self.pos = 0;
        self.done = false;
        Ok(())
    }
}

impl Iterator for AudioStream {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if let Some(sample) = self.samples.get(self.pos) {
                self.pos += 1;
                return Some(*sample);
            }
            if !self.fill() {
                return None;
            }
        }
    }
}
