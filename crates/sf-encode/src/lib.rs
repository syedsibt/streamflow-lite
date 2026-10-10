//! `sf-encode` — FFmpeg-based video/audio encoders for StreamFlow Lite.
//!
//! [`VideoEncoder`] wraps an H.264 encoder chosen by [`select_video_encoder`]
//! (NVENC / QuickSync / AMF on Windows, libx264 software fallback, mpeg4 last
//! resort). [`AudioEncoder`] wraps the native FFmpeg AAC encoder.
//!
//! Encoded output is codec-agnostic [`EncodedPacket`]s carrying microsecond
//! timestamps, so muxers (`sf-output`) can use a 1/1_000_000 stream time base
//! without precision loss.

mod audio;
mod selection;
mod video;

pub use audio::AudioEncoder;
pub use selection::{
    ffmpeg_encoder_available, select_system_video_encoder, select_video_encoder, EncoderId,
};
pub use video::{VideoEncoder, VideoEncoderOpts};

use thiserror::Error;

/// A single encoded packet, in codec-agnostic form.
///
/// `stream_index`: 0 = video, 1 = audio.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    /// Presentation timestamp, microseconds, on the caller's clock.
    pub pts_us: i64,
    /// Decode timestamp, microseconds, on the caller's clock.
    pub dts_us: i64,
    pub is_keyframe: bool,
    pub stream_index: u8,
}

/// Errors from the encoding pipeline.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// FFmpeg global init failed.
    #[error("ffmpeg init failed: {0}")]
    Init(String),
    /// The requested encoder name is not in the FFmpeg registry.
    #[error("encoder not found: {0}")]
    EncoderNotFound(String),
    /// The encoder could not be opened with the given options.
    #[error("encoder open failed: {0}")]
    Open(String),
    /// A frame could not be sent or a packet could not be received.
    #[error("encode failed: {0}")]
    Encode(String),
    /// Caller-supplied input (frame size, sample count, …) was invalid.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl From<EncodeError> for String {
    fn from(e: EncodeError) -> Self {
        e.to_string()
    }
}

/// Allocate an `AVCodecContext` against a concrete codec.
///
/// This replicates `Context::new_with_codec` (which ffmpeg-next 6 removed):
/// `avcodec_alloc_context3(codec)` initializes codec defaults — including the
/// codec's private AVOptions area. Plain `Context::new()` (= alloc with a NULL
/// codec) leaves `codec_id` unset, and the libx264 wrapper then never sees
/// options like `preset` from the open dict, failing with x264's "broken
/// ffmpeg default settings detected".
pub(crate) unsafe fn context_with_codec(
    codec: &ffmpeg_next::Codec,
) -> Result<ffmpeg_next::codec::context::Context, EncodeError> {
    // SAFETY: `codec.as_ptr()` is a valid live AVCodec from the registry;
    // the returned context is owned (owner None → freed on drop).
    let ptr = ffmpeg_sys_next::avcodec_alloc_context3(codec.as_ptr());
    if ptr.is_null() {
        return Err(EncodeError::Open("avcodec_alloc_context3 failed".into()));
    }
    Ok(ffmpeg_next::codec::context::Context::wrap(ptr, None))
}
