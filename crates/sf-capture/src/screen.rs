//! X11 screen capture via FFmpeg's `x11grab` input device (Linux).
//!
//! # Status: code-complete, option-parsing tested, **requires X server —
//! unverified here**
//!
//! [`ScreenProducer`] opens `:DISPLAY+X,Y` through the `x11grab` demuxer
//! and decodes frames exactly like [`super::media::MediaFileProducer`].
//! The option/geometry parsing ([`ScreenCaptureOptions::parse`],
//! [`ScreenCaptureOptions::input_url`],
//! [`ScreenCaptureOptions::ffmpeg_options`]) is pure logic and unit-tested.
//! The actual grab needs a running X server — this headless VM has none,
//! so end-to-end capture is **unverified** (the code path is identical to
//! the verified file-decode path once the input context is open).

use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;

use ffmpeg_next as ff;

use super::demux::{ensure_init, DemuxDecoder};
use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// Options for an X11 screen capture.
///
/// Geometry string format: `WIDTHxHEIGHT+X,Y` (e.g. `"1920x1080+0,0"`),
/// optionally followed by `@FPS` (e.g. `"1280x720+100,50@30"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCaptureOptions {
    /// X display, e.g. `":0.0"`. Defaults from `$DISPLAY`.
    pub display: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl ScreenCaptureOptions {
    /// Parse `"WIDTHxHEIGHT+X,Y[@FPS]"`. Display comes from `$DISPLAY`
    /// (fallback `":0.0"`).
    pub fn parse(geometry: &str) -> Result<Self, CaptureError> {
        let (geom, fps) = match geometry.split_once('@') {
            Some((g, f)) => {
                let fps: u32 = f.parse().map_err(|_| {
                    CaptureError::Unsupported(format!("bad fps in screen geometry {geometry:?}"))
                })?;
                (g, fps)
            }
            None => (geometry, 30),
        };
        let (wh, xy) = geom.split_once('+').ok_or_else(|| {
            CaptureError::Unsupported(format!("bad screen geometry {geometry:?}, want WxH+X,Y"))
        })?;
        let (w, h) = wh.split_once('x').ok_or_else(|| {
            CaptureError::Unsupported(format!("bad screen geometry {geometry:?}, want WxH+X,Y"))
        })?;
        let (x, y) = xy.split_once(',').ok_or_else(|| {
            CaptureError::Unsupported(format!("bad screen geometry {geometry:?}, want WxH+X,Y"))
        })?;
        let parse = |s: &str, what: &str| {
            s.parse::<u32>().map_err(|_| {
                CaptureError::Unsupported(format!("bad {what} in screen geometry {geometry:?}"))
            })
        };
        let (width, height, x, y) = (
            parse(w, "width")?,
            parse(h, "height")?,
            parse(x, "x")?,
            parse(y, "y")?,
        );
        if width == 0 || height == 0 || fps == 0 {
            return Err(CaptureError::Unsupported(format!(
                "screen geometry {geometry:?} needs non-zero W/H/FPS"
            )));
        }
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0.0".to_string());
        Ok(Self {
            display,
            x,
            y,
            width,
            height,
            fps,
        })
    }

    /// The x11grab input URL, e.g. `":0.0+100,50"`.
    pub fn input_url(&self) -> String {
        format!("{}+{},{}", self.display, self.x, self.y)
    }

    /// Demuxer options for x11grab: capture size + framerate.
    /// Order is stable (video_size, framerate) for testability.
    pub fn ffmpeg_options(&self) -> Vec<(String, String)> {
        vec![
            (
                "video_size".to_string(),
                format!("{}x{}", self.width, self.height),
            ),
            ("framerate".to_string(), self.fps.to_string()),
        ]
    }
}

/// X11 screen source. Frames arrive at the requested framerate via the
/// x11grab demuxer; `next_frame` returns `Ok(None)` when no frame is ready
/// yet (non-blocking).
///
/// Requires a running X server and FFmpeg built with `--enable-x11grab`.
/// Unverified on this headless VM.
pub struct ScreenProducer {
    demux: DemuxDecoder,
    info: StreamInfo,
}

impl ScreenProducer {
    /// Start capturing with the given options.
    pub fn open(opts: &ScreenCaptureOptions) -> Result<Self, CaptureError> {
        ensure_init();
        let url = CString::new(opts.input_url())
            .map_err(|_| CaptureError::Unsupported("display string contains NUL".into()))?;
        let ictx = unsafe { open_x11grab(&url, opts) }?;
        let nominal_frame_us = 1_000_000 / opts.fps as u64;
        let demux = DemuxDecoder::from_input(ictx, nominal_frame_us)?;
        let info = demux.stream_info(false);
        Ok(Self { demux, info })
    }
}

impl FrameProducer for ScreenProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        self.demux.next_video()
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}

/// Open an x11grab input context via the raw FFmpeg API (ffmpeg-next has
/// no device-demuxer helper, so we call `avformat_open_input` with the
/// `x11grab` AVInputFormat directly).
///
/// # Safety
/// All pointer handling is contained here; on success ownership of the
/// `AVFormatContext` moves into `ff::format::context::Input`.
unsafe fn open_x11grab(
    url: &CString,
    opts: &ScreenCaptureOptions,
) -> Result<ff::format::context::Input, CaptureError> {
    let name = CString::new("x11grab").expect("static");
    let iformat = ff::sys::av_find_input_format(name.as_ptr() as *const c_char);
    if iformat.is_null() {
        return Err(CaptureError::Unsupported(
            "x11grab input device not available in this FFmpeg build".into(),
        ));
    }
    let mut dict = ff::Dictionary::new();
    for (k, v) in opts.ffmpeg_options() {
        dict.set(&k, &v);
    }
    let mut dict_ptr = dict.disown();
    let mut ps: *mut ff::sys::AVFormatContext = ptr::null_mut();
    let ret = ff::sys::avformat_open_input(
        &mut ps,
        url.as_ptr() as *const c_char,
        iformat,
        &mut dict_ptr,
    );
    // Reclaim any unconsumed options to avoid leaking the dictionary.
    let _ = ff::Dictionary::own(dict_ptr);
    if ret < 0 || ps.is_null() {
        return Err(CaptureError::OpenFailed(format!(
            "x11grab open failed for {} (is an X server running on {}?): {}",
            opts.input_url(),
            opts.display,
            ff::util::error::Error::from(ret)
        )));
    }
    let ret = ff::sys::avformat_find_stream_info(ps, ptr::null_mut());
    if ret < 0 {
        ff::sys::avformat_close_input(&mut ps);
        return Err(CaptureError::OpenFailed(format!(
            "x11grab stream info failed: {}",
            ff::util::error::Error::from(ret)
        )));
    }
    Ok(ff::format::context::Input::wrap(ps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_geometry_full_form() {
        let o = ScreenCaptureOptions::parse("1920x1080+100,50@30").unwrap();
        assert_eq!(o.width, 1920);
        assert_eq!(o.height, 1080);
        assert_eq!((o.x, o.y), (100, 50));
        assert_eq!(o.fps, 30);
    }

    #[test]
    fn parse_geometry_defaults_to_30fps() {
        let o = ScreenCaptureOptions::parse("1280x720+0,0").unwrap();
        assert_eq!(o.fps, 30);
        assert_eq!((o.width, o.height), (1280, 720));
    }

    #[test]
    fn parse_geometry_rejects_garbage() {
        for bad in [
            "",
            "1920x1080",
            "1920x1080+0",
            "0x720+0,0",
            "1920x0+0,0",
            "1920x1080+0,0@0",
            "1920x1080+0,0@fps",
            "1920*1080+0,0",
        ] {
            assert!(
                ScreenCaptureOptions::parse(bad).is_err(),
                "geometry {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn input_url_and_ffmpeg_options_shape() {
        let o = ScreenCaptureOptions {
            display: ":1.0".into(),
            x: 10,
            y: 20,
            width: 800,
            height: 600,
            fps: 25,
        };
        assert_eq!(o.input_url(), ":1.0+10,20");
        assert_eq!(
            o.ffmpeg_options(),
            vec![
                ("video_size".to_string(), "800x600".to_string()),
                ("framerate".to_string(), "25".to_string()),
            ]
        );
    }

    #[test]
    fn screen_open_without_x_server_fails_cleanly() {
        // No X server here: open must return a clean OpenFailed/Unsupported,
        // never panic. DISPLAY is unset/pointless in this environment.
        let opts = ScreenCaptureOptions {
            display: ":99.0".into(),
            x: 0,
            y: 0,
            width: 320,
            height: 240,
            fps: 10,
        };
        match ScreenProducer::open(&opts) {
            Err(CaptureError::OpenFailed(_)) | Err(CaptureError::Unsupported(_)) => {}
            Err(e) => panic!("expected clean open failure, got error: {e}"),
            Ok(_) => panic!("expected clean open failure, but open succeeded"),
        }
    }
}
