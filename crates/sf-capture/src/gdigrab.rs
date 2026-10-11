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
//! * `DemuxDecoder::set_live(true)`: the video queue holds at most one
//!   frame; a 60 Hz desktop behind a 30 fps tick drops stale frames
//!   instead of lagging.
//! * `next_frame` drains to the newest frame with a 2 ms source-timestamp
//!   hysteresis (the OBS `ready_async_frame` rule), like the dshow camera.
//!
//! # Status
//! Code-complete; **hardware-unverified** — no Windows desktop exists on
//! the CI/dev machines. The open path follows FFmpeg's documented
//! `gdigrab` API; decode is the tested demuxer used by media files.

use std::ffi::CString;
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ffmpeg_next as ff;

use super::demux::{ensure_init, DemuxDecoder};
use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// A stalled grab must not hang the tick.
const READ_TIMEOUT_US: u64 = 25_000;
/// Draining queued packets must return instantly.
const DRAIN_TIMEOUT_US: u64 = 1_000;
/// Opening the desktop grab must fail fast, not hang.
const OPEN_TIMEOUT_US: u64 = 5_000_000;
/// Upper bound on drain iterations per `next_frame` call.
const MAX_DRAIN_FRAMES: u32 = 4;

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

struct ReadDeadline {
    deadline_us: AtomicU64,
}

unsafe extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let rd = unsafe { &*(opaque as *const ReadDeadline) };
    let dl = rd.deadline_us.load(Ordering::Relaxed);
    if dl != 0 && now_us() >= dl {
        1
    } else {
        0
    }
}

/// Live desktop screen producer (Windows gdigrab).
pub struct ScreenProducer {
    demux: DemuxDecoder,
    info: StreamInfo,
    deadline: Arc<ReadDeadline>,
    last_delivered_pts_us: Option<i64>,
}

// SAFETY: FFmpeg contexts have no thread affinity; the backend serializes
// all producer access through the shell `Mutex`.
unsafe impl Send for ScreenProducer {}

impl ScreenProducer {
    /// Start capturing the primary desktop at `fps`.
    pub fn open(fps: u32) -> Result<Self, CaptureError> {
        ensure_init();
        let fmt_name = CString::new("gdigrab").unwrap();
        let fmt = unsafe { ff::ffi::av_find_input_format(fmt_name.as_ptr()) };
        if fmt.is_null() {
            return Err(CaptureError::OpenFailed(
                "FFmpeg was built without gdigrab support".into(),
            ));
        }
        let url = CString::new("desktop").unwrap();

        let deadline = Arc::new(ReadDeadline {
            deadline_us: AtomicU64::new(now_us() + OPEN_TIMEOUT_US),
        });
        let mut ctx: *mut ff::ffi::AVFormatContext =
            unsafe { ff::ffi::avformat_alloc_context() };
        if ctx.is_null() {
            return Err(CaptureError::OpenFailed("avformat_alloc_context failed".into()));
        }
        unsafe {
            (*ctx).interrupt_callback.callback = Some(interrupt_cb);
            (*ctx).interrupt_callback.opaque = Arc::as_ptr(&deadline) as *mut c_void;
        }
        let mut dict: *mut ff::ffi::AVDictionary = ptr::null_mut();
        let mut set_opt = |k: &str, v: &str| unsafe {
            let key = CString::new(k).unwrap();
            let val = CString::new(v).unwrap();
            ff::ffi::av_dict_set(&mut dict, key.as_ptr(), val.as_ptr(), 0);
        };
        let fps = fps.clamp(1, 60).to_string();
        set_opt("framerate", &fps);
        // gdigrab has no internal buffering knob like dshow's nobuffer;
        // the live demuxer queue (newest-wins) provides the same guarantee.
        set_opt("probesize", "32");
        set_opt("analyzeduration", "0");
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
            return Err(CaptureError::OpenFailed(
                "cannot open desktop capture (gdigrab)".into(),
            ));
        }
        let si_ret = unsafe { ff::ffi::avformat_find_stream_info(ctx, ptr::null_mut()) };
        deadline.deadline_us.store(0, Ordering::Relaxed);
        if si_ret < 0 {
            unsafe { ff::ffi::avformat_close_input(&mut ctx) };
            return Err(CaptureError::OpenFailed(
                "avformat_find_stream_info failed".into(),
            ));
        }
        // SAFETY: `ctx` is a valid open input context owned by us now;
        // `Input::wrap` takes ownership and closes it on drop.
        let ictx = unsafe { ff::format::context::Input::wrap(ctx) };
        let mut demux = DemuxDecoder::from_input(ictx, 33_333)
            .map_err(|e| CaptureError::OpenFailed(format!("demuxer: {e}")))?;
        // Live capture: never accumulate a frame backlog.
        demux.set_live(true);
        let info = demux.stream_info(false);
        Ok(Self {
            demux,
            info,
            deadline,
            last_delivered_pts_us: None,
        })
    }
}

impl FrameProducer for ScreenProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        // Drain to the newest frame (same as the dshow camera): a 60 Hz
        // desktop behind a 30 fps tick must drop stale frames, not queue.
        let mut newest: Option<CapturedFrame> = None;
        let mut reads: u32 = 0;
        loop {
            let timeout_us = if reads == 0 {
                READ_TIMEOUT_US
            } else {
                DRAIN_TIMEOUT_US
            };
            self.deadline
                .deadline_us
                .store(now_us() + timeout_us, Ordering::Relaxed);
            let r = self.demux.next_video();
            self.deadline.deadline_us.store(0, Ordering::Relaxed);
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
        // OBS `ready_async_frame` hysteresis (2 ms).
        if let Some(last_pts) = self.last_delivered_pts_us {
            if frame.pts_us <= last_pts.saturating_add(2_000) {
                return Ok(None);
            }
        }
        self.last_delivered_pts_us = Some(frame.pts_us);
        Ok(Some(frame))
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}
