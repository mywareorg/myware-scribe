//! Symphonia codec registry shared by the transcription decoder and playback.
//!
//! Symphonia 0.5 has no Opus decoder of its own, and the container readers do
//! report `CODEC_TYPE_OPUS` for Ogg and Matroska tracks. Registering the libopus
//! adapter on top of the default set is what makes those tracks playable.

use symphonia::core::codecs::CodecRegistry;

/// A registry with every decoder this build supports, including Opus.
///
/// Building it costs a handful of map inserts, so callers construct one per
/// open rather than sharing a static: decoding is a once-per-file operation.
pub fn registry() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut registry);
    registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
    registry
}
