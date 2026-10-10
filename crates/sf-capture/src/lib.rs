//! `sf-capture` — frame capture sources for the StreamFlow Lite engine.
//!
//! Every source implements [`FrameProducer`], which yields timestamped
//! RGBA8 frames. The engine consumes frames through this trait only, so
//! adding a new source never touches engine code.
//!
//! # Sources
//!
//! | Module | Source | Status |
//! |---|---|---|
//! | [`media`] | Media file (mp4/mkv/…) via FFmpeg | **Verified here** (decode test) |
//! | [`camera`] | V4L2 cameras (`/dev/video*`) | Code-complete, logic-tested, **hardware-unverified** (no camera on this VM) |
//! | [`screen`] | X11 screen grab via FFmpeg `x11grab` | Code-complete, option-parsing tested, **requires X server — unverified here** |
//! | `mf_camera` (Windows) | MediaFoundation camera | Scaffold: plausible public shape, `todo!()` body |
//! | `dxgi_capture` (Windows) | DXGI desktop duplication | Scaffold: plausible public shape, `todo!()` body |
//!
//! # Drop policy
//!
//! A producer yields frames at the *source* rate. It never blocks waiting
//! for the consumer: [`FrameProducer::next_frame`] returns `Ok(None)` when
//! no frame is ready yet (live sources) or the decoded frame as fast as
//! possible (file sources). **The consumer is responsible for dropping late
//! frames** — e.g. the engine keeps only the newest frame per input when it
//! falls behind, so a slow consumer never causes unbounded memory growth
//! inside a producer.
//!
//! # Audio
//!
//! Video and audio travel separately: [`FrameProducer`] yields video only.
//! [`media::MediaFileProducer`] additionally exposes
//! [`media::MediaFileProducer::next_audio`], which returns audio **resampled
//! to 48000 Hz stereo `f32`** — the normalization the audio mixer expects.

use thiserror::Error;

/// One decoded video frame, RGBA8, tightly packed (`width * 4` bytes/row).
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// RGBA8 pixel data, row-major, top-left first. `len == width*height*4`.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Presentation timestamp in microseconds (monotonic per source).
    pub pts_us: i64,
    /// Frame duration in microseconds (e.g. 33333 for 30 fps).
    pub duration_us: u64,
}

impl CapturedFrame {
    /// Bytes per row (`width * 4`).
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// Pixel at (x, y) as [R, G, B, A]. Panics on out-of-bounds.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }
}

/// Static description of a stream, known right after open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub has_audio: bool,
}

impl StreamInfo {
    /// Frames per second as f64 (0.0 when unknown / `fps_den == 0`).
    pub fn fps(&self) -> f64 {
        if self.fps_den == 0 {
            0.0
        } else {
            self.fps_num as f64 / self.fps_den as f64
        }
    }

    /// Nominal frame duration in microseconds (0 when fps unknown).
    pub fn frame_duration_us(&self) -> u64 {
        if self.fps_num == 0 {
            0
        } else {
            1_000_000u64 * self.fps_den as u64 / self.fps_num as u64
        }
    }
}

/// Errors a capture source can report.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// No such device/source (bad index, missing `/dev/videoN`, …).
    #[error("capture device not found: {0}")]
    DeviceNotFound(String),
    /// The source exists but could not be opened/negotiated.
    #[error("capture open failed: {0}")]
    OpenFailed(String),
    /// A read/decode failed after a successful open.
    #[error("capture read failed: {0}")]
    ReadFailed(String),
    /// Source exhausted (end of file). Not an error condition for the
    /// consumer — it should stop polling or loop the file.
    #[error("end of stream")]
    EndOfStream,
    /// The source or a requested option is not supported on this platform.
    #[error("unsupported capture: {0}")]
    Unsupported(String),
}

/// A source of timestamped RGBA8 video frames.
///
/// # Contract
///
/// * `next_frame` is **non-blocking**: it returns `Ok(None)` when no frame
///   is ready *yet* (live sources between frames), `Ok(Some(frame))` when a
///   frame is available, and `Err(CaptureError::EndOfStream)` when the
///   source is exhausted (files). Any other error is a real failure.
/// * Frames arrive in PTS order with `pts_us` strictly increasing.
/// * `stream_info` is valid immediately after construction.
/// * The producer yields at source rate; **the consumer drops late
///   frames** (see crate docs). Producers never buffer unboundedly.
pub trait FrameProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError>;
    fn stream_info(&self) -> StreamInfo;
}

/// Test helper: yields `count` synthetic RGBA frames with exact PTS spacing.
///
/// Frame `i` has `pts_us = i * spacing_us` and a deterministic gradient
/// pattern (R varies with x, G with y, B with the frame index) so tests can
/// verify both timing and pixel content without any hardware.
#[derive(Debug)]
pub struct MockProducer {
    info: StreamInfo,
    count: usize,
    spacing_us: u64,
    next: usize,
}

impl MockProducer {
    /// `width`×`height` frames, `count` of them, spaced `spacing_us` apart.
    /// `fps` is derived as `1_000_000 / spacing_us`.
    pub fn new(width: u32, height: u32, count: usize, spacing_us: u64) -> Self {
        let fps_num = if spacing_us == 0 {
            0
        } else {
            1_000_000 / spacing_us as u32
        };
        Self {
            info: StreamInfo {
                width,
                height,
                fps_num,
                fps_den: 1,
                has_audio: false,
            },
            count,
            spacing_us,
            next: 0,
        }
    }

    /// How many frames have been yielded so far.
    pub fn produced(&self) -> usize {
        self.next
    }
}

impl FrameProducer for MockProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.next >= self.count {
            return Err(CaptureError::EndOfStream);
        }
        let i = self.next;
        self.next += 1;
        let (w, h) = (self.info.width as usize, self.info.height as usize);
        let mut data = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                data[o] = ((x * 255) / w.max(1)) as u8;
                data[o + 1] = ((y * 255) / h.max(1)) as u8;
                data[o + 2] = ((i * 37) % 256) as u8;
                data[o + 3] = 255;
            }
        }
        Ok(Some(CapturedFrame {
            data,
            width: self.info.width,
            height: self.info.height,
            pts_us: i as i64 * self.spacing_us as i64,
            duration_us: self.spacing_us,
        }))
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}

#[cfg(target_os = "linux")]
pub mod camera;
pub mod media;
pub mod screen;

mod demux;

#[cfg(windows)]
pub mod dxgi_capture;
#[cfg(windows)]
pub mod mf_camera;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_producer_yields_exact_count_then_end_of_stream() {
        let mut p = MockProducer::new(16, 8, 5, 33_333);
        for _ in 0..5 {
            assert!(p.next_frame().unwrap().is_some());
        }
        assert!(matches!(p.next_frame(), Err(CaptureError::EndOfStream)));
        assert_eq!(p.produced(), 5);
    }

    #[test]
    fn mock_producer_pts_spacing_is_exact_and_increasing() {
        let spacing = 33_333u64;
        let mut p = MockProducer::new(16, 8, 10, spacing);
        let mut prev = -1i64;
        for i in 0..10 {
            let f = p.next_frame().unwrap().unwrap();
            assert_eq!(f.pts_us, i as i64 * spacing as i64);
            assert_eq!(f.duration_us, spacing);
            assert!(f.pts_us > prev);
            prev = f.pts_us;
            assert_eq!((f.width, f.height), (16, 8));
            assert_eq!(f.data.len(), 16 * 8 * 4);
            assert_eq!(f.stride(), 16 * 4);
        }
    }

    #[test]
    fn mock_producer_pixel_pattern_is_deterministic() {
        let mut p = MockProducer::new(4, 4, 2, 1000);
        let f0 = p.next_frame().unwrap().unwrap();
        let f1 = p.next_frame().unwrap().unwrap();
        // Top-left pixel: R=0,G=0, B differs per frame, A=255.
        assert_eq!(f0.pixel(0, 0), [0, 0, 0, 255]);
        assert_eq!(f1.pixel(0, 0), [0, 0, 37, 255]);
        // Bottom-right: R,G near 255.
        let br = f0.pixel(3, 3);
        assert_eq!((br[0], br[1], br[3]), (191, 191, 255));
    }

    #[test]
    fn stream_info_fps_and_duration_math() {
        let s = StreamInfo {
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            has_audio: true,
        };
        assert!((s.fps() - 30.0).abs() < 1e-9);
        assert_eq!(s.frame_duration_us(), 33_333);
        let unknown = StreamInfo {
            width: 0,
            height: 0,
            fps_num: 0,
            fps_den: 0,
            has_audio: false,
        };
        assert_eq!(unknown.fps(), 0.0);
        assert_eq!(unknown.frame_duration_us(), 0);
    }

    #[test]
    fn capture_error_display_is_human_readable() {
        assert_eq!(format!("{}", CaptureError::EndOfStream), "end of stream");
        assert!(
            format!("{}", CaptureError::DeviceNotFound("/dev/video0".into()))
                .contains("/dev/video0")
        );
    }
}
