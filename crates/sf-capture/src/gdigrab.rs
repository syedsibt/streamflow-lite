//! Windows screen capture via FFmpeg's `gdigrab` input device.
//!
//! # OBS parity
//! OBS captures the desktop through `win-capture` (`monitor-capture` /
//! `window-capture`): DXGI desktop duplication with a dedicated capture
//! thread, newest-frame-wins delivery to the video thread. We reach the
//! same desktop through FFmpeg's `gdigrab` (`-f gdigrab -i desktop`) and
//! decode with the shared [`DemuxDecoder`], marked live so the queue is
//! newest-wins — the same no-accumulation guarantee.
//!
//! # Realtime rules
//! * Capture runs on its own thread with non-blocking reads (gdigrab
//!   would otherwise `av_usleep` until the next frame time *inside the
//!   media tick*); only the newest frame is kept.
//!
//! # Status
//! Code-complete; **hardware-unverified** — no Windows desktop exists on
//! the CI/dev machines. The open path follows FFmpeg's documented
//! `gdigrab` API; decode is the tested demuxer used by media files.

use std::time::Duration;

use super::live::{LiveVideo, LiveVideoStats};
use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// Opening the desktop grab must fail fast, not hang.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Live desktop screen producer (Windows gdigrab) on its own capture
/// thread — the grab (BitBlt + BMP decode) never runs inside the media
/// tick, and the tick never waits for it.
pub struct ScreenProducer {
    live: LiveVideo,
    info: StreamInfo,
}

impl ScreenProducer {
    /// Start capturing the primary desktop at `fps`.
    pub fn open(fps: u32) -> Result<Self, CaptureError> {
        let fps = fps.clamp(1, 60).to_string();
        let (live, _codec) = LiveVideo::open(
            "screen",
            Some("gdigrab"),
            "desktop",
            &[("framerate", fps.as_str()), ("draw_mouse", "1")],
            OPEN_TIMEOUT,
        )?;
        // The desktop is grabbed on each read (no device queue): while no
        // scene shows it, don't grab at all.
        live.set_drain_when_idle(false);
        let info = live.stream_info();
        Ok(Self { live, info })
    }

    /// Live capture statistics.
    /// Decode only while shown (idle sources read and drop packets).
    pub fn set_active(&self, active: bool) {
        self.live.set_active(active);
    }

    pub fn live_stats(&self) -> LiveVideoStats {
        self.live.stats()
    }
}

impl FrameProducer for ScreenProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        Ok(self.live.take_latest())
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}
