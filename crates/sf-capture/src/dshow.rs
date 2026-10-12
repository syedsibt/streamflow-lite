//! DirectShow camera capture (Windows only).
//!
//! Opens Windows cameras through FFmpeg's `dshow` input format and reuses
//! the shared [`DemuxDecoder`] for decode → RGBA8 conversion, so only the
//! input-opening path is dshow-specific.
//!
//! # Status
//! Code-complete; **hardware-unverified** — no Windows camera exists on the
//! CI/dev machines. The open path follows FFmpeg's documented `dshow`
//! API (`av_find_input_format` + `avformat_open_input` with
//! `video_size`/`framerate` options); decode, scaling, and PTS handling are
//! the tested demuxer used by media files.
//!
//! [`DemuxDecoder`]: super::demux::DemuxDecoder

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Mutex;

use ffmpeg_next as ff;

use super::demux::ensure_init;
use super::live::{LiveVideo, LiveVideoStats};
use super::media;
use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// Log capture for [`list_devices`]: dshow reports devices via `av_log`,
/// so we install a temporary callback, trigger the listing, then restore.
static LOG_CAPTURE: Mutex<String> = Mutex::new(String::new());

/// The `va_list` parameter type of FFmpeg's log callback: `char*` on
/// Windows x64. (The non-Windows alias only exists so the Windows code
/// paths can be type-checked on Linux with `--cfg sf_wincheck`.)
#[cfg(windows)]
type VaList = *mut c_char;
#[cfg(not(windows))]
type VaList = *mut ff::ffi::__va_list_tag;

extern "C" {
    /// msvcrt's vsnprintf — always available on Windows.
    fn vsnprintf(s: *mut c_char, n: usize, fmt: *const c_char, ap: VaList) -> c_int;
}

unsafe extern "C" fn log_capture_cb(
    _ptr: *mut c_void,
    _level: c_int,
    fmt: *const c_char,
    args: VaList,
) {
    if fmt.is_null() {
        return;
    }
    // Expand the varargs: dshow device lines are `"Name" (video)` with the
    // name interpolated via %s. On x86_64 Windows, va_list is char*.
    let mut buf = vec![0 as c_char; 4096];
    let n = vsnprintf(buf.as_mut_ptr(), buf.len(), fmt, args);
    if n > 0 {
        let s = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        if let Ok(mut cap) = LOG_CAPTURE.lock() {
            cap.push_str(&s);
        }
    }
}

/// List DirectShow video-capture device names.
///
/// Opens the dshow input with `list_devices=1`, which makes FFmpeg print
/// the devices and fail; we capture the log and parse the quoted names.
pub fn list_devices() -> Result<Vec<String>, CaptureError> {
    ensure_init();
    let fmt_name = CString::new("dshow").unwrap();
    let fmt = unsafe { ff::ffi::av_find_input_format(fmt_name.as_ptr()) };
    if fmt.is_null() {
        return Err(CaptureError::OpenFailed(
            "FFmpeg was built without dshow support".into(),
        ));
    }

    // Install log capture.
    {
        let mut buf = LOG_CAPTURE.lock().unwrap();
        buf.clear();
    }
    unsafe { ff::ffi::av_log_set_callback(Some(log_capture_cb)) };

    let url = CString::new("dummy").unwrap();
    let mut dict: *mut ff::ffi::AVDictionary = ptr::null_mut();
    let key = CString::new("list_devices").unwrap();
    let val = CString::new("1").unwrap();
    unsafe { ff::ffi::av_dict_set(&mut dict, key.as_ptr(), val.as_ptr(), 0) };

    let mut ctx: *mut ff::ffi::AVFormatContext = ptr::null_mut();
    let ret = unsafe {
        ff::ffi::avformat_open_input(
            &mut ctx,
            url.as_ptr(),
            fmt as *mut ff::ffi::AVInputFormat,
            &mut dict,
        )
    };
    unsafe {
        ff::ffi::av_dict_free(&mut dict);
        if !ctx.is_null() {
            ff::ffi::avformat_close_input(&mut ctx);
        }
        // Restore default logging.
        ff::ffi::av_log_set_callback(None);
    }
    // `list_devices` always "fails" the open — that is expected.
    let _ = ret;

    let buf = LOG_CAPTURE.lock().unwrap().clone();
    Ok(parse_dshow_devices(&buf, "(video)"))
}

/// List DirectShow audio-capture device names (microphones, line-in, etc.).
pub fn list_audio_devices() -> Result<Vec<String>, CaptureError> {
    // Reuse the same enumeration: trigger list_devices and parse audio entries.
    // We call the internal list to avoid duplicating the FFmpeg dance.
    let buf = {
        // Run the device listing (same as list_devices but we parse audio).
        list_devices_raw()?
    };
    Ok(parse_dshow_devices(&buf, "(audio)"))
}

/// Run the dshow device listing and return the raw captured log.
fn list_devices_raw() -> Result<String, CaptureError> {
    ensure_init();
    let fmt_name = CString::new("dshow").unwrap();
    let fmt = unsafe { ff::ffi::av_find_input_format(fmt_name.as_ptr()) };
    if fmt.is_null() {
        return Err(CaptureError::OpenFailed(
            "FFmpeg was built without dshow support".into(),
        ));
    }
    {
        let mut buf = LOG_CAPTURE.lock().unwrap();
        buf.clear();
    }
    unsafe { ff::ffi::av_log_set_callback(Some(log_capture_cb)) };
    let url = CString::new("dummy").unwrap();
    let mut dict: *mut ff::ffi::AVDictionary = ptr::null_mut();
    let key = CString::new("list_devices").unwrap();
    let val = CString::new("1").unwrap();
    unsafe { ff::ffi::av_dict_set(&mut dict, key.as_ptr(), val.as_ptr(), 0) };
    let mut ctx: *mut ff::ffi::AVFormatContext = ptr::null_mut();
    let ret = unsafe {
        ff::ffi::avformat_open_input(
            &mut ctx,
            url.as_ptr(),
            fmt as *mut ff::ffi::AVInputFormat,
            &mut dict,
        )
    };
    unsafe {
        ff::ffi::av_dict_free(&mut dict);
        if !ctx.is_null() {
            ff::ffi::avformat_close_input(&mut ctx);
        }
        ff::ffi::av_log_set_callback(None);
    }
    let _ = ret;
    Ok(LOG_CAPTURE.lock().unwrap().clone())
}

/// Parse dshow device names from captured log output.
/// dshow logs: `[dshow @ ...] "Device Name" (video)` / `(audio)`.
fn parse_dshow_devices(buf: &str, kind_tag: &str) -> Vec<String> {
    let mut devices = Vec::new();
    for line in buf.lines() {
        if !line.contains(kind_tag) {
            continue;
        }
        // Take the first quoted string.
        let mut in_quotes = false;
        let mut name = String::new();
        for ch in line.chars() {
            if ch == '"' {
                if in_quotes {
                    break;
                }
                in_quotes = true;
                continue;
            }
            if in_quotes {
                name.push(ch);
            }
        }
        if !name.is_empty() && !devices.contains(&name) {
            devices.push(name);
        }
    }
    devices
}

/// DirectShow camera / capture-card [`FrameProducer`] (Windows).
///
/// A live source: [`next_frame`](FrameProducer::next_frame) returns the
/// newest frame captured since the last call, or `Ok(None)` when none
/// arrived yet; it never touches the device and never blocks.
///
/// Capture runs on a dedicated thread ([`LiveVideo`]): non-blocking device
/// reads, newest-frame-wins, capture timestamps mapped onto the pipeline
/// clock. See `live.rs` for why the previous blocking drain never returned
/// on a real dshow device.
pub struct DshowProducer {
    live: LiveVideo,
    info: StreamInfo,
    /// What the camera actually negotiated, e.g. `"mjpeg 1920×1080@60fps
    /// [via mjpeg]"` — shown in diagnostics.
    negotiated_format: String,
}

/// Max time to wait for a dshow device to open (per attempt).
const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

impl DshowProducer {
    /// Open `device` (a name from [`list_devices`]) at `width`×`height`,
    /// `fps`. Frames are converted to RGBA8 by the shared demuxer.
    ///
    /// Format preference (1080p smoothness): **MJPEG first** — raw YUY2 at
    /// 1080p saturates USB 2 bandwidth and drops fps; MJPEG keeps the
    /// camera at full rate. Falls back through raw formats to fully
    /// negotiated (some virtual cameras / capture cards reject explicit
    /// options).
    pub fn open(device: &str, width: u32, height: u32, fps: u32) -> Result<Self, CaptureError> {
        ensure_init();
        let size = format!("{width}x{height}");
        let rate = fps.to_string();
        let attempts: Vec<(&str, Vec<(&str, &str)>)> = vec![
            ("mjpeg", vec![("vcodec", "mjpeg"), ("video_size", &size), ("framerate", &rate)]),
            ("nv12", vec![("pixel_format", "nv12"), ("video_size", &size), ("framerate", &rate)]),
            ("yuv420p", vec![("pixel_format", "yuv420p"), ("video_size", &size), ("framerate", &rate)]),
            ("size+fps", vec![("video_size", &size), ("framerate", &rate)]),
            ("negotiated", vec![]),
        ];
        let url = format!("video={device}");
        let mut last_err = String::new();
        for (label, opts) in &attempts {
            match LiveVideo::open("dshow", Some("dshow"), &url, opts, OPEN_TIMEOUT) {
                Ok((live, codec)) => {
                    let info = live.stream_info();
                    let negotiated_format = format!(
                        "{} {}×{}@{}fps [via {label}]",
                        codec,
                        info.width,
                        info.height,
                        info.fps().round() as u32
                    );
                    return Ok(Self {
                        live,
                        info,
                        negotiated_format,
                    });
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(CaptureError::OpenFailed(format!(
            "cannot open camera '{device}' (unplugged, or in use by another app): {last_err}"
        )))
    }

    /// Kept for API compatibility: audio is always opened as a separate
    /// `AudioInputProducer` graph (a shared `video=X:audio=Y` graph let the
    /// audio packet flood starve video). `audio_device` is ignored here.
    pub fn open_with_audio(
        device: &str,
        _audio_device: Option<&str>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        Self::open(device, width, height, fps)
    }

    /// Human-readable description of what the camera actually negotiated.
    pub fn negotiated_format(&self) -> &str {
        &self.negotiated_format
    }

    /// Live capture statistics (fps, drops, capture latency, errors).
    pub fn live_stats(&self) -> LiveVideoStats {
        self.live.stats()
    }

    /// Open the first available DirectShow camera.
    pub fn open_default(width: u32, height: u32, fps: u32) -> Result<Self, CaptureError> {
        let devices = list_devices()?;
        let first = devices
            .into_iter()
            .next()
            .ok_or_else(|| CaptureError::DeviceNotFound("no DirectShow cameras found".into()))?;
        Self::open(&first, width, height, fps)
    }

    /// Video-only producer: audio comes from a separate `AudioInputProducer`.
    pub fn next_audio(&mut self) -> Result<Option<media::DecodedAudio>, CaptureError> {
        Ok(None)
    }
}

impl FrameProducer for DshowProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        let stats = self.live.stats();
        if stats.ended {
            return Err(CaptureError::ReadFailed(
                stats.last_error.unwrap_or_else(|| "camera disconnected".into()),
            ));
        }
        Ok(self.live.take_latest())
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}
