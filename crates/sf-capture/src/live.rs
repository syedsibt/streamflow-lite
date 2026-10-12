//! Dedicated capture thread for live video devices (cameras, capture
//! cards, desktop grab) — OBS's per-source capture thread.
//!
//! # Design (live-first, never a backlog)
//!
//! * The thread owns the device demuxer in **non-blocking** mode and polls
//!   it every [`POLL`] (2 ms). Each pass reads everything the device has
//!   queued, decodes only what is needed, and publishes **only the newest
//!   frame** into a one-slot mailbox. Anything older is dropped and counted.
//! * The media tick never touches the device: [`LiveVideo::take_latest`]
//!   is a mutex swap. A slow tick can therefore never make capture lag, and
//!   a stalled device can never stall the tick.
//! * Every published frame carries its capture time on the pipeline clock
//!   ([`crate::clock::now_us`], device timestamps mapped by
//!   [`crate::clock::ClockMapper`]), so downstream stages can measure the
//!   real end-to-end age of each frame.
//!
//! # Why not the old design
//!
//! The previous loop called `next_video_newest`, which kept reading until
//! "the interrupt deadline fired". dshow and gdigrab never consult the
//! interrupt callback, and ffmpeg-next's `packets()` iterator retries every
//! non-EOF error forever — so on a live device that loop never returned
//! (camera thread published nothing; the gdigrab call inside the media tick
//! froze the tick while holding the shell lock).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::demux::DemuxDecoder;
use crate::{CaptureError, CapturedFrame, StreamInfo};

/// Device poll interval when nothing was queued (bounds added latency).
const POLL: Duration = Duration::from_millis(2);

/// Live capture statistics (for the diagnostics panel).
#[derive(Debug, Clone, Default)]
pub struct LiveVideoStats {
    /// Frames published to the mailbox.
    pub frames_captured: u64,
    /// Frames dropped because a newer one arrived first (in the device
    /// queue or unread in the mailbox).
    pub frames_dropped: u64,
    /// Frames taken by the consumer.
    pub frames_taken: u64,
    /// Publish rate over the last second (≈ device fps).
    pub capture_fps: f32,
    /// Age of the newest frame when it was published (device → mailbox),
    /// microseconds: queueing + decode time inside capture.
    pub capture_latency_us: i64,
    /// Last read/decode error, if any (kept until the next good frame).
    pub last_error: Option<String>,
    /// The device ended (unplugged / graph stopped).
    pub ended: bool,
}

struct Shared {
    latest: Mutex<Option<CapturedFrame>>,
    stop: AtomicBool,
    stats: Mutex<LiveVideoStats>,
}

/// A running live video capture (see module docs). Dropping it stops the
/// thread (it exits within one poll interval; it is not joined, so a driver
/// that wedges inside a call can never hang the caller).
pub struct LiveVideo {
    shared: Arc<Shared>,
    info: StreamInfo,
}

impl LiveVideo {
    /// Start the capture thread for an opened live demuxer.
    pub(crate) fn spawn(label: &str, mut demux: DemuxDecoder) -> Result<Self, CaptureError> {
        demux.set_live(true);
        demux.set_nonblocking();
        let info = demux.stream_info(false);
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            stop: AtomicBool::new(false),
            stats: Mutex::new(LiveVideoStats::default()),
        });
        let sh = Arc::clone(&shared);
        // Abort any blocking protocol read as soon as stop is requested
        // (network-style inputs block; devices are non-blocking anyway).
        // SAFETY: the thread owns `sh` and drops the demuxer first.
        unsafe {
            demux.set_interrupt(stop_interrupt_cb, Arc::as_ptr(&sh) as *mut std::os::raw::c_void);
        }
        std::thread::Builder::new()
            .name(format!("sf-capture-{label}"))
            .spawn(move || capture_loop(demux, sh))
            .map_err(|e| CaptureError::OpenFailed(format!("cannot start capture thread: {e}")))?;
        Ok(Self { shared, info })
    }

    /// Newest captured frame since the last call (`None` if none arrived).
    /// Never blocks on the device.
    pub fn take_latest(&self) -> Option<CapturedFrame> {
        let f = self.shared.latest.lock().ok()?.take();
        if f.is_some() {
            if let Ok(mut s) = self.shared.stats.lock() {
                s.frames_taken += 1;
            }
        }
        f
    }

    pub fn stats(&self) -> LiveVideoStats {
        self.shared.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn stream_info(&self) -> StreamInfo {
        self.info
    }
}

impl LiveVideo {
    /// Open any FFmpeg live input (device or network) and start capturing.
    ///
    /// `format` is the input format name (`"dshow"`, `"gdigrab"`,
    /// `"mpegts"`…, or `None` to probe); `options` are demuxer/device
    /// options. The open itself is bounded by `open_timeout` through the
    /// interrupt callback (honoured by network protocols and by the
    /// generic open path); reads afterwards are non-blocking.
    pub fn open(
        label: &str,
        format: Option<&str>,
        url: &str,
        options: &[(&str, &str)],
        open_timeout: Duration,
    ) -> Result<(Self, String), CaptureError> {
        let ictx = open_input(format, url, options, open_timeout)?;
        let codec = ictx
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .and_then(|s| {
                ffmpeg_next::codec::context::Context::from_parameters(s.parameters())
                    .ok()?
                    .decoder()
                    .video()
                    .ok()?
                    .codec()
                    .map(|c| c.name().to_string())
            })
            .unwrap_or_else(|| "unknown".into());
        let demux = DemuxDecoder::from_input(ictx, 33_333)?;
        Ok((Self::spawn(label, demux)?, codec))
    }
}

struct OpenDeadline(std::sync::atomic::AtomicI64);

unsafe extern "C" fn open_interrupt_cb(opaque: *mut std::os::raw::c_void) -> std::os::raw::c_int {
    if opaque.is_null() {
        return 0;
    }
    let d = &*(opaque as *const OpenDeadline);
    (crate::clock::now_us() >= d.0.load(Ordering::Relaxed)) as std::os::raw::c_int
}

/// `avformat_open_input` + `avformat_find_stream_info` with a hard deadline
/// and low-latency probing. The interrupt callback is removed afterwards,
/// so no pointer outlives this call.
pub(crate) fn open_input(
    format: Option<&str>,
    url: &str,
    options: &[(&str, &str)],
    open_timeout: Duration,
) -> Result<ffmpeg_next::format::context::Input, CaptureError> {
    use ffmpeg_next as ff;
    use std::ffi::CString;
    crate::demux::ensure_init();
    let fmt = match format {
        Some(name) => {
            let n = CString::new(name).map_err(|_| CaptureError::OpenFailed("bad format".into()))?;
            let f = unsafe { ff::ffi::av_find_input_format(n.as_ptr()) };
            if f.is_null() {
                return Err(CaptureError::Unsupported(format!(
                    "this FFmpeg build has no '{name}' input"
                )));
            }
            f
        }
        None => std::ptr::null(),
    };
    let curl = CString::new(url).map_err(|_| CaptureError::OpenFailed("device name contains NUL".into()))?;
    let deadline = Box::new(OpenDeadline(std::sync::atomic::AtomicI64::new(
        crate::clock::now_us() + open_timeout.as_micros() as i64,
    )));
    let mut ctx = unsafe { ff::ffi::avformat_alloc_context() };
    if ctx.is_null() {
        return Err(CaptureError::OpenFailed("avformat_alloc_context failed".into()));
    }
    unsafe {
        (*ctx).interrupt_callback.callback = Some(open_interrupt_cb);
        (*ctx).interrupt_callback.opaque = &*deadline as *const OpenDeadline as *mut _;
    }
    let mut dict: *mut ff::ffi::AVDictionary = std::ptr::null_mut();
    let mut set = |k: &str, v: &str| {
        if let (Ok(k), Ok(v)) = (CString::new(k), CString::new(v)) {
            unsafe { ff::ffi::av_dict_set(&mut dict, k.as_ptr(), v.as_ptr(), 0) };
        }
    };
    // Live: tiny probe, no demuxer-side buffering.
    set("probesize", "32");
    set("analyzeduration", "0");
    set("fflags", "nobuffer");
    for (k, v) in options {
        set(k, v);
    }
    let ret = unsafe { ff::ffi::avformat_open_input(&mut ctx, curl.as_ptr(), fmt as *mut _, &mut dict) };
    unsafe { ff::ffi::av_dict_free(&mut dict) };
    if ret < 0 || ctx.is_null() {
        // `avformat_open_input` freed `ctx` on failure.
        return Err(CaptureError::OpenFailed(format!(
            "cannot open '{url}': {}",
            ff::Error::from(ret)
        )));
    }
    let si = unsafe { ff::ffi::avformat_find_stream_info(ctx, std::ptr::null_mut()) };
    unsafe {
        (*ctx).interrupt_callback.callback = None;
        (*ctx).interrupt_callback.opaque = std::ptr::null_mut();
    }
    drop(deadline);
    if si < 0 {
        unsafe { ff::ffi::avformat_close_input(&mut ctx) };
        return Err(CaptureError::OpenFailed("could not read stream info from the device".into()));
    }
    // SAFETY: `ctx` is a valid, open input context we own.
    Ok(unsafe { ff::format::context::Input::wrap(ctx) })
}

impl Drop for LiveVideo {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn stop_interrupt_cb(opaque: *mut std::os::raw::c_void) -> std::os::raw::c_int {
    if opaque.is_null() {
        return 0;
    }
    let sh = &*(opaque as *const Shared);
    sh.stop.load(Ordering::Relaxed) as std::os::raw::c_int
}

fn capture_loop(mut demux: DemuxDecoder, sh: Arc<Shared>) {
    run_capture(&mut demux, &sh);
    // The interrupt callback points into `sh`: drop the context first.
    drop(demux);
    drop(sh);
}

fn run_capture(demux: &mut DemuxDecoder, sh: &Arc<Shared>) {
    let mut publish_times: VecDeque<i64> = VecDeque::with_capacity(128);
    let mut consecutive_errors = 0u32;
    while !sh.stop.load(Ordering::Relaxed) {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| demux.drain_live()))
            .unwrap_or_else(|_| Err(CaptureError::ReadFailed("capture backend panicked".into())));
        match r {
            Ok(d) => {
                consecutive_errors = 0;
                if d.dropped > 0 {
                    if let Ok(mut s) = sh.stats.lock() {
                        s.frames_dropped += d.dropped as u64;
                    }
                }
                if let Some(frame) = d.frame {
                    let now = crate::clock::now_us();
                    let latency = now - frame.captured_at_us;
                    let replaced = sh
                        .latest
                        .lock()
                        .map(|mut slot| slot.replace(frame).is_some())
                        .unwrap_or(false);
                    publish_times.push_back(now);
                    while publish_times.front().map_or(false, |t| now - *t > 1_000_000) {
                        publish_times.pop_front();
                    }
                    if let Ok(mut s) = sh.stats.lock() {
                        s.frames_captured += 1;
                        if replaced {
                            s.frames_dropped += 1;
                        }
                        s.capture_fps = publish_times.len() as f32;
                        s.capture_latency_us = latency;
                        s.last_error = None;
                    }
                }
                if d.eof {
                    if let Ok(mut s) = sh.stats.lock() {
                        s.ended = true;
                        s.last_error = Some("device stopped delivering (unplugged?)".into());
                    }
                    break;
                }
                if d.packets == 0 {
                    std::thread::sleep(POLL);
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if let Ok(mut s) = sh.stats.lock() {
                    s.last_error = Some(e.to_string());
                }
                let ms = (5u64 << consecutive_errors.min(5)).min(200);
                std::thread::sleep(Duration::from_millis(ms));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    /// A real live source: the ffmpeg CLI sends a 30 fps MJPEG MPEG-TS over
    /// UDP in real time (`-re`), exactly like a device that keeps producing
    /// whether or not anyone reads.
    fn live_sender(port: u16, codec: &str) -> Child {
        // NUT over TCP (the sender listens; we connect like opening a device).
        Command::new("ffmpeg")
            .args([
                "-v", "error", "-re",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30",
                "-c:v", codec, "-q:v", "5", "-g", "15", "-bf", "0",
                "-f", "nut", &format!("tcp://127.0.0.1:{port}?listen=1&tcp_nodelay=1"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("ffmpeg CLI must be installed")
    }

    fn port(seed: u16) -> u16 {
        40_000 + (std::process::id() % 1000) as u16 * 10 + seed
    }

    fn open_live(port: u16) -> LiveVideo {
        // Let the sender start (process spawn + first PAT/PMT).
        std::thread::sleep(Duration::from_millis(600));
        // tcp_nodelay: without it Windows' delayed-ACK/Nagle interplay holds
        // small frames back ~200 ms in the *test transport*.
        let url = format!("tcp://127.0.0.1:{port}?tcp_nodelay=1");
        let (live, _codec) = LiveVideo::open(
            "test",
            Some("nut"),
            &url,
            &[("probesize", "5000000"), ("analyzeduration", "1000000")],
            Duration::from_secs(5),
        )
        .expect("open live udp input");
        live
    }

    fn run_live_case(codec: &str, seed: u16) {
        let p = port(seed);
        let mut tx = live_sender(p, codec);
        let live = open_live(p);
        // Warm up: frames flow continuously.
        let t0 = Instant::now();
        let mut got = 0;
        while t0.elapsed() < Duration::from_secs(2) {
            if live.take_latest().is_some() {
                got += 1;
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        assert!(got > 30, "{codec}: only {got} frames in 2 s");

        // Stall the consumer for 3 s (a frozen UI / slow tick). Live-first:
        // the next frame must be fresh, not the first of a 3 s backlog.
        let before = live.take_latest().map(|f| f.pts_us);
        std::thread::sleep(Duration::from_secs(3));
        let f = loop {
            if let Some(f) = live.take_latest() {
                break f;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        if let Some(b) = before {
            let advanced = f.pts_us - b;
            assert!(
                advanced > 2_700_000,
                "{codec}: after a 3 s stall the frame is only {advanced} us newer (backlog?) stats {:?}",
                live.stats()
            );
        }
        let age_ms = (crate::clock::now_us() - f.captured_at_us) / 1000;
        assert!(age_ms < 150, "{codec}: frame is {age_ms} ms old, stats {:?}", live.stats());
        let st = live.stats();
        assert!(st.frames_dropped > 60, "{codec}: stale frames must be dropped, stats {st:?}");
        assert!(st.capture_fps > 20.0, "{codec}: capture fps {}", st.capture_fps);
        drop(live);
        let _ = tx.kill();
        let _ = tx.wait();
    }

    #[test]
    fn live_mjpeg_never_builds_a_backlog() {
        run_live_case("mjpeg", 1);
    }

    #[test]
    fn live_h264_decodes_references_but_shows_newest() {
        // Inter-coded camera (H.264): every packet is decoded to keep
        // references valid, only the newest picture is converted.
        run_live_case("libx264", 2);
    }

    #[test]
    fn drop_never_blocks() {
        let p = port(3);
        let mut tx = live_sender(p, "mjpeg");
        let live = open_live(p);
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        drop(live);
        assert!(t.elapsed() < Duration::from_millis(50));
        let _ = tx.kill();
        let _ = tx.wait();
    }
}
