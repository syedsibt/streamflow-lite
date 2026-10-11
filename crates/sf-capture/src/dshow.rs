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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ffmpeg_next as ff;

use super::demux::{ensure_init, DemuxDecoder};
use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// Log capture for [`list_devices`]: dshow reports devices via `av_log`,
/// so we install a temporary callback, trigger the listing, then restore.
static LOG_CAPTURE: Mutex<String> = Mutex::new(String::new());

/// Deadline for blocking reads on a dshow device (microseconds since epoch).
/// The interrupt callback aborts `av_read_frame` when the deadline passes,
/// so a stalled device can never freeze the app — the read just fails and
/// `next_frame` returns `Ok(None)`.
struct ReadDeadline {
    deadline_us: AtomicU64,
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

unsafe extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let rd = &*(opaque as *const ReadDeadline);
    let dl = rd.deadline_us.load(Ordering::Relaxed);
    if dl != 0 && now_us() >= dl {
        1 // abort the blocking read
    } else {
        0
    }
}

extern "C" {
    /// msvcrt's vsnprintf — always available on Windows.
    fn vsnprintf(s: *mut c_char, n: usize, fmt: *const c_char, ap: *mut c_char) -> c_int;
}

unsafe extern "C" fn log_capture_cb(
    _ptr: *mut c_void,
    _level: c_int,
    fmt: *const c_char,
    args: *mut c_char,
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

/// DirectShow camera [`FrameProducer`] (Windows).
///
/// A live source: [`next_frame`](FrameProducer::next_frame) returns
/// `Ok(None)` when no frame is ready yet and never reports end-of-stream.
/// Reads are bounded by [`READ_TIMEOUT_US`]: if the device stalls, the
/// interrupt callback aborts the read instead of blocking forever.
pub struct DshowProducer {
    demux: DemuxDecoder,
    info: StreamInfo,
    deadline: Arc<ReadDeadline>,
    /// Human-readable description of what the camera actually negotiated
    /// (codec, resolution, fps) — e.g. `"mjpeg 1920×1080@60fps"`.
    /// Shown in diagnostics so users see what the camera gave, not just
    /// what was requested.
    negotiated_format: String,
    /// Minimum microseconds between delivered frames (decimation): the
    /// camera may run faster than our tick (e.g. 1080p60 in, 30fps tick).
    /// Stale frames are consumed-and-dropped so the dshow packet queue can
    /// never grow — a growing queue shows up as ever-increasing lag.
    min_frame_interval_us: u64,
    /// Timestamp of the last delivered frame (for decimation).
    last_delivered_us: u64,
}

/// Max time to wait for a frame from a dshow device before giving up
/// on that tick (25ms). The media tick runs every 33.3ms; a longer timeout
/// would stall it. When no frame is ready the consumer repeats the last
/// frame (latest-frame-wins) and the next tick tries again.
const READ_TIMEOUT_US: u64 = 25_000;

/// Timeout for drain reads after the first frame (3ms): packets already
/// queued in dshow return instantly; an empty queue aborts almost at once
/// so the drain never stalls the tick.
const DRAIN_TIMEOUT_US: u64 = 3_000;

/// Max frames to drain per tick. Bounds the decode work when the camera
/// outruns the tick (e.g. 60fps in, 30fps tick → ~2 per tick normally).
const MAX_DRAIN_FRAMES: u32 = 4;

/// Max time to wait for a dshow device to open (15 seconds per attempt).
const OPEN_TIMEOUT_US: u64 = 15_000_000;

impl DshowProducer {
    /// Open `device` (a name from [`list_devices`]) at `width`×`height`,
    /// `fps`. Frames are converted to RGBA8 by the shared demuxer.
    ///
    /// Tries the requested format first, then falls back to letting dshow
    /// negotiate (some virtual cameras reject explicit `video_size` /
    /// `pixel_format` options).
    ///
    /// Format preference (1080p smoothness): **MJPEG first** — raw YUY2 at
    /// 1080p saturates USB bandwidth and drops fps; MJPEG/NV12 keep the
    /// camera at full rate. Falls back through raw formats to fully
    /// negotiated.
    pub fn open(
        device: &str,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        ensure_init();
        // Decimate the camera to the requested fps: if the device delivers
        // faster (e.g. 1080p60 negotiated while we tick at 30fps), stale
        // frames are dropped instead of queueing up as lag. The gate is set
        // to 90% of the nominal interval so normal tick jitter (±ms) never
        // causes a false skip (which would halve the fps).
        let min_frame_interval_us = 1_000_000 * 9 / (fps.max(1) as u64 * 10);
        // Try strict format first, then progressively looser.
        // Each entry is (label, dshow options). The label is recorded in
        // `negotiated_format` for diagnostics.
        let attempts: Vec<(&str, Vec<(String, String)>)> = vec![
            (
                "mjpeg",
                vec![
                    ("vcodec".into(), "mjpeg".into()),
                    ("video_size".into(), format!("{width}x{height}")),
                    ("framerate".into(), fps.to_string()),
                    // Critical for live: disable FFmpeg's internal buffering,
                    // otherwise frames queue up and the preview lags by seconds
                    // or even minutes behind real time.
                    ("fflags".into(), "nobuffer".into()),
                ],
            ),
            (
                "yuv420p",
                vec![
                    ("video_size".into(), format!("{width}x{height}")),
                    ("framerate".into(), fps.to_string()),
                    ("pixel_format".into(), "yuv420p".into()),
                    ("fflags".into(), "nobuffer".into()),
                ],
            ),
            (
                "size+fps",
                vec![
                    ("video_size".into(), format!("{width}x{height}")),
                    ("framerate".into(), fps.to_string()),
                    ("fflags".into(), "nobuffer".into()),
                ],
            ),
            // Let dshow negotiate everything.
            ("negotiated", vec![("fflags".into(), "nobuffer".into())]),
        ];
        let mut last_err = String::new();
        for (label, opts) in &attempts {
            match Self::open_with_opts(device, label, opts, min_frame_interval_us) {
                Ok(p) => return Ok(p),
                Err(e) => last_err = e,
            }
        }
        Err(CaptureError::OpenFailed(format!(
            "dshow: cannot open camera '{device}' (no such device or it is in use): {last_err}"
        )))
    }

    fn open_with_opts(
        device: &str,
        label: &str,
        opts: &[(String, String)],
        min_frame_interval_us: u64,
    ) -> Result<Self, String> {
        let fmt_name = CString::new("dshow").unwrap();
        let fmt = unsafe { ff::ffi::av_find_input_format(fmt_name.as_ptr()) };
        if fmt.is_null() {
            return Err("FFmpeg was built without dshow support".into());
        }

        // `video=Device Name` selects the capture device.
        let url = CString::new(format!("video={device}"))
            .map_err(|_| "device name contains NUL".to_string())?;

        let mut dict: *mut ff::ffi::AVDictionary = ptr::null_mut();
        let mut set_opt = |k: &str, v: &str| unsafe {
            let key = CString::new(k).unwrap();
            let val = CString::new(v).unwrap();
            ff::ffi::av_dict_set(&mut dict, key.as_ptr(), val.as_ptr(), 0);
        };
        for (k, v) in opts {
            set_opt(k, v);
        }
        // Minimize startup probing latency for live capture: we already
        // know the format (or let dshow negotiate), so don't buffer
        // megabytes probing it.
        set_opt("probesize", "32");
        set_opt("analyzeduration", "0");

        // Allocate the context first so we can install the interrupt
        // callback BEFORE opening — a stalled device must not hang
        // `avformat_open_input` / `find_stream_info` forever.
        let deadline = Arc::new(ReadDeadline {
            deadline_us: AtomicU64::new(now_us() + OPEN_TIMEOUT_US),
        });
        let mut ctx: *mut ff::ffi::AVFormatContext =
            unsafe { ff::ffi::avformat_alloc_context() };
        if ctx.is_null() {
            unsafe { ff::ffi::av_dict_free(&mut dict) };
            return Err("avformat_alloc_context failed".into());
        }
        unsafe {
            (*ctx).interrupt_callback.callback = Some(interrupt_cb);
            (*ctx).interrupt_callback.opaque = Arc::as_ptr(&deadline) as *mut c_void;
        }
        let ret = unsafe {
            ff::ffi::avformat_open_input(
                &mut ctx,
                url.as_ptr(),
                fmt as *mut ff::ffi::AVInputFormat,
                &mut dict,
            )
        };
        unsafe { ff::ffi::av_dict_free(&mut dict) };
        if ret < 0 || ctx.is_null() {
            // `avformat_open_input` frees `ctx` on failure; do not touch it.
            return Err("avformat_open_input failed".into());
        }

        // Populate stream parameters (codec, dimensions, pixel format).
        // dshow is a live device; without this the stream params may be
        // incomplete and the decoder setup below can misbehave.
        // The open deadline is still armed, bounding this call too.
        let si_ret = unsafe { ff::ffi::avformat_find_stream_info(ctx, ptr::null_mut()) };
        // Open phase done — disarm the deadline; per-read timeouts are
        // armed in `next_frame`.
        deadline.deadline_us.store(0, Ordering::Relaxed);
        if si_ret < 0 {
            unsafe { ff::ffi::avformat_close_input(&mut ctx) };
            return Err("avformat_find_stream_info failed".into());
        }

        // The interrupt callback stays installed for the context's lifetime;
        // `deadline` (Arc) is owned by `Self`, so the opaque pointer stays
        // valid until drop.

        // SAFETY: `ctx` is a valid open input context owned by us now;
        // `Input::wrap` takes ownership and closes it on drop.
        let ictx = unsafe { ff::format::context::Input::wrap(ctx) };
        // Record what the camera actually negotiated (codec of the best
        // video stream), before the context moves into the demuxer.
        let negotiated_codec: String = ictx
            .streams()
            .best(ff::media::Type::Video)
            .and_then(|s| {
                ff::codec::context::Context::from_parameters(s.parameters())
                    .ok()?
                    .decoder()
                    .video()
                    .ok()?
                    .codec()
                    .map(|c| c.name().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());
        let mut demux = DemuxDecoder::from_input(ictx, 33_333)
            .map_err(|e| format!("demuxer: {e}"))?;
        // Live capture: never accumulate a frame backlog (prevents
        // unbounded memory growth when the device outpaces the poll loop).
        demux.set_live(true);
        let info = demux.stream_info(false);
        // Compact single-line form for the UI badge, e.g.
        // "mjpeg 1920×1080@60fps" — what the camera actually gave.
        let mut negotiated_format = format!(
            "{} {}×{}@{}fps",
            negotiated_codec,
            info.width,
            info.height,
            info.fps().round() as u32,
        );
        // Keep the attempt label for diagnostics (which request succeeded).
        negotiated_format.push_str(&format!(" [via {label}]"));
        Ok(Self {
            demux,
            info,
            deadline,
            negotiated_format,
            min_frame_interval_us,
            last_delivered_us: 0,
        })
    }

    /// Human-readable description of what the camera actually negotiated,
    /// e.g. `"mjpeg 1920×1080@60fps [via negotiated]"`.
    pub fn negotiated_format(&self) -> &str {
        &self.negotiated_format
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
}

impl FrameProducer for DshowProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        // Drain all currently available frames, keeping the newest. The
        // first read may block up to READ_TIMEOUT_US (device stall → "no
        // frame yet"); drain reads use DRAIN_TIMEOUT_US so queued packets
        // return instantly and an empty queue aborts at once. Draining is
        // what keeps a 60fps camera from building an ever-growing queue
        // behind our 30fps tick (the classic ever-increasing lag).
        let mut newest: Option<CapturedFrame> = None;
        let mut reads: u32 = 0;
        loop {
            let timeout_us = if reads == 0 {
                READ_TIMEOUT_US
            } else {
                DRAIN_TIMEOUT_US
            };
            // Bound the blocking read: if the device stalls, the interrupt
            // callback aborts it and we report "no frame yet" instead of
            // hanging the tick thread.
            self.deadline
                .deadline_us
                .store(now_us() + timeout_us, Ordering::Relaxed);
            let r = self.demux.next_video();
            self.deadline.deadline_us.store(0, Ordering::Relaxed);
            // Live source: the demuxer never yields EndOfStream for dshow;
            // map it defensively to "no frame yet" so a live source can
            // never terminate the consumer. A timed-out read surfaces as a
            // `ReadFailed` — also map that to "no frame yet".
            match r {
                Ok(Some(f)) => newest = Some(f),
                Ok(None) => break,
                Err(CaptureError::EndOfStream) => break,
                Err(CaptureError::ReadFailed(_)) => break,
                Err(e) => return Err(e),
            }
            reads += 1;
            if reads >= MAX_DRAIN_FRAMES {
                break;
            }
        }
        let Some(frame) = newest else {
            return Ok(None);
        };
        // Decimate to the target fps: drop the frame when we delivered one
        // too recently; the consumer repeats the last frame (no stutter,
        // no accumulation).
        let now = now_us();
        if now.saturating_sub(self.last_delivered_us) < self.min_frame_interval_us {
            return Ok(None);
        }
        self.last_delivered_us = now;
        Ok(Some(frame))
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}
