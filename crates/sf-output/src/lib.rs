//! `sf-output` — recording and streaming outputs for StreamFlow Lite.
//!
//! * [`Recorder`] muxes [`EncodedPacket`][sf_encode::EncodedPacket]s from
//!   `sf-encode` into MP4 (fragmented — crash-safe — or flat) / MKV files via
//!   libavformat.
//! * [`Streamer`] pushes packets to an RTMP target through a [`Transport`]
//!   with exponential-backoff reconnects. [`RealTransport`] uses the same
//!   libavformat FLV mux path as a real RTMP publish; [`FakeTransport`]
//!   scripts failures for tests.
//!
//! Stream keys are never logged or serialized: [`StreamTarget`] redacts them
//! in `Debug` (tested).

mod recorder;
mod streamer;

pub use recorder::Recorder;
pub use streamer::{
    redact_url, FakeTransport, RealTransport, ReconnectPolicy, StreamState, StreamTarget, Streamer,
    Transport, TransportError,
};

use thiserror::Error;

/// Write the output trailer.
///
/// Works around an ffmpeg-next quirk: FFmpeg's MP4 muxer returns the mfra
/// box *size* (a positive number, e.g. 67) from `av_write_trailer` on success
/// for fragmented MP4, but ffmpeg-next's `Output::write_trailer` treats any
/// non-zero return as an error. FFmpeg's own callers accept `ret >= 0`, so
/// this does the same via a direct sys call.
pub(crate) fn write_trailer(
    octx: &mut ffmpeg_next::format::context::Output,
) -> Result<(), OutputError> {
    // SAFETY: `octx` is a live output context; `as_mut_ptr` is the documented
    // accessor for exactly this kind of escape hatch.
    let ret = unsafe { ffmpeg_sys_next::av_write_trailer(octx.as_mut_ptr()) };
    if ret < 0 {
        Err(OutputError::Trailer(format!("av_write_trailer: {ret}")))
    } else {
        Ok(())
    }
}

/// Rescale a microsecond timestamp into a stream time base using FFmpeg's
/// own `av_rescale_q` (round-to-nearest, overflow-safe).
pub(crate) fn rescale_us_to_tb(ts_us: i64, tb: ffmpeg_next::Rational) -> i64 {
    // SAFETY: pure arithmetic on values, no pointers.
    unsafe {
        ffmpeg_sys_next::av_rescale_q(
            ts_us,
            ffmpeg_sys_next::AVRational {
                num: 1,
                den: 1_000_000,
            },
            ffmpeg_sys_next::AVRational {
                num: tb.0,
                den: tb.1,
            },
        )
    }
}

/// Container format for [`Recorder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerFormat {
    /// MP4 with `frag_keyframe+empty_moov+default_base_moof`: crash-safe,
    /// playable before the trailer is written. The default for recordings.
    Mp4Fragmented,
    /// Regular MP4 (moov at end; needs a clean `finish()`).
    Mp4Flat,
    /// Matroska.
    Mkv,
}

/// Statistics returned by [`Recorder::finish`].
#[derive(Debug, Clone, Copy)]
pub struct OutputStats {
    pub video_packets: u64,
    pub audio_packets: u64,
    /// `last_dts_us - first_pts_us` across all written packets.
    pub duration_us: u64,
}

/// Errors from recording / streaming outputs.
#[derive(Debug, Error)]
pub enum OutputError {
    /// The output context (file or URL) could not be opened.
    #[error("output init failed: {0}")]
    Init(String),
    /// Packets were written before any video stream was added.
    #[error("no video stream added")]
    NoVideoStream,
    /// Writing the stream header failed.
    #[error("header failed: {0}")]
    Header(String),
    /// Writing a packet failed.
    #[error("write failed: {0}")]
    Write(String),
    /// Writing the trailer failed.
    #[error("trailer failed: {0}")]
    Trailer(String),
}
