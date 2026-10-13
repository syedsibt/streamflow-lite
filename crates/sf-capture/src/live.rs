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
    /// Video packets the device delivered over the last second (before
    /// stale ones are dropped) — the camera's real output rate.
    pub device_fps: f32,
    /// Smoothed decode time per published frame, ms.
    pub decode_ms: f32,
    /// Smoothed RGBA conversion time per published frame, ms.
    pub convert_ms: f32,
    /// Packets the decoder rejected (corrupt MJPEG / mid-GOP H.264).
    pub decode_errors: u64,
    /// Failed capture passes (read/convert errors → short backoff).
    pub capture_errors: u64,
    /// Longest gap between two published frames in the last second, ms
    /// (what the UI sees as a "waiting for frames" stall).
    pub max_gap_ms: f32,
    /// Last read/decode error, if any (kept until the next good frame).
    pub last_error: Option<String>,
    /// The device ended (unplugged / graph stopped).
    pub ended: bool,
}

struct Shared {
    latest: Mutex<Option<CapturedFrame>>,
    stop: AtomicBool,
    /// False while nothing shows this source: packets are read and dropped
    /// (the device never backs up) but nothing is decoded or converted.
    active: AtomicBool,
    /// While inactive, keep reading (and dropping) packets. Needed for
    /// devices that queue frames (cameras); a grab-on-read input (desktop
    /// capture) sets it false and is not touched at all while unseen.
    drain_idle: AtomicBool,
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
            active: AtomicBool::new(true),
            drain_idle: AtomicBool::new(true),
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

    /// Decode only while something shows this source (see `Shared::active`).
    /// Re-activating delivers the next frame the device sends.
    pub fn set_active(&self, active: bool) {
        if !self.shared.active.swap(active, Ordering::Relaxed) && active {
            // Drop a stale frame from before the pause.
            if let Ok(mut l) = self.shared.latest.lock() {
                *l = None;
            }
        }
    }

    /// See `Shared::drain_idle`.
    pub fn set_drain_when_idle(&self, drain: bool) {
        self.shared.drain_idle.store(drain, Ordering::Relaxed);
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
                let dec = ffmpeg_next::codec::context::Context::from_parameters(s.parameters())
                    .ok()?
                    .decoder()
                    .video()
                    .ok()?;
                let name = dec.codec().map(|c| c.name().to_string())?;
                if name != "rawvideo" {
                    return Some(name);
                }
                // Raw: the pixel format is the useful part (yuyv422, nv12…).
                let pix: ffmpeg_next::ffi::AVPixelFormat = dec.format().into();
                let p = unsafe { ffmpeg_next::ffi::av_get_pix_fmt_name(pix) };
                if p.is_null() {
                    Some(name)
                } else {
                    Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
                }
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
    let mut requested: Vec<&str> = Vec::new();
    for (k, v) in options {
        if *k == "vcodec" {
            // Not an AVOption: the FFmpeg CLI's `-vcodec` before `-i` sets
            // AVFormatContext.video_codec_id, which dshow reads to choose a
            // compressed (MJPEG/H.264) camera mode. Passed as a dictionary
            // entry it was silently ignored — so "MJPEG 1080p30" was really
            // a request for *raw* 1080p30, which most USB cameras can't do.
            let name = CString::new(*v).map_err(|_| CaptureError::OpenFailed("bad codec".into()))?;
            let codec = unsafe { ff::ffi::avcodec_find_decoder_by_name(name.as_ptr()) };
            if codec.is_null() {
                unsafe { ff::ffi::avformat_free_context(ctx) };
                return Err(CaptureError::Unsupported(format!("no '{v}' decoder in this FFmpeg build")));
            }
            unsafe { (*ctx).video_codec_id = (*codec).id };
            continue;
        }
        requested.push(k);
        set(k, v);
    }
    let ret = unsafe { ff::ffi::avformat_open_input(&mut ctx, curl.as_ptr(), fmt as *mut _, &mut dict) };
    // Options still in the dictionary were not recognised by the input:
    // treat that as a failed attempt instead of silently opening with
    // something else.
    let mut unused: Vec<String> = Vec::new();
    unsafe {
        let mut e: *mut ff::ffi::AVDictionaryEntry = std::ptr::null_mut();
        let empty = CString::new("").expect("static");
        loop {
            e = ff::ffi::av_dict_get(dict, empty.as_ptr(), e, ff::ffi::AV_DICT_IGNORE_SUFFIX);
            if e.is_null() {
                break;
            }
            let k = std::ffi::CStr::from_ptr((*e).key).to_string_lossy().into_owned();
            if requested.contains(&k.as_str()) {
                unused.push(k);
            }
        }
        ff::ffi::av_dict_free(&mut dict);
    }
    if ret < 0 || ctx.is_null() {
        // `avformat_open_input` freed `ctx` on failure.
        return Err(CaptureError::OpenFailed(format!(
            "cannot open '{url}': {}",
            ff::Error::from(ret)
        )));
    }
    if !unused.is_empty() {
        unsafe { ff::ffi::avformat_close_input(&mut ctx) };
        return Err(CaptureError::OpenFailed(format!(
            "input does not support option(s): {}",
            unused.join(", ")
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
    let mut packet_times: VecDeque<i64> = VecDeque::with_capacity(256);
    let mut consecutive_errors = 0u32;
    let mut decode_ms = 0f32;
    let mut convert_ms = 0f32;
    while !sh.stop.load(Ordering::Relaxed) {
        let decode = sh.active.load(Ordering::Relaxed);
        if !decode && !sh.drain_idle.load(Ordering::Relaxed) {
            // Unseen grab-on-read input: no capture work at all.
            if let Ok(mut s) = sh.stats.lock() {
                s.capture_fps = 0.0;
                s.device_fps = 0.0;
                s.last_error = None;
            }
            publish_times.clear();
            packet_times.clear();
            std::thread::sleep(Duration::from_millis(30));
            continue;
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| demux.drain_live_mode(decode)))
            .unwrap_or_else(|_| Err(CaptureError::ReadFailed("capture backend panicked".into())));
        let now = crate::clock::now_us();
        match r {
            Ok(d) => {
                consecutive_errors = 0;
                for _ in 0..d.video_packets {
                    packet_times.push_back(now);
                }
                let mut published = None;
                if let Some(frame) = d.frame {
                    let latency = now - frame.captured_at_us;
                    let replaced = sh
                        .latest
                        .lock()
                        .map(|mut slot| slot.replace(frame).is_some())
                        .unwrap_or(false);
                    publish_times.push_back(now);
                    decode_ms += 0.1 * (d.decode_us as f32 / 1000.0 - decode_ms);
                    convert_ms += 0.1 * (d.convert_us as f32 / 1000.0 - convert_ms);
                    published = Some((latency, replaced));
                }
                while publish_times.front().is_some_and(|t| now - *t > 1_000_000) {
                    publish_times.pop_front();
                }
                while packet_times.front().is_some_and(|t| now - *t > 1_000_000) {
                    packet_times.pop_front();
                }
                if let Ok(mut s) = sh.stats.lock() {
                    // A pass that read video fine clears an old read error
                    // (otherwise an unseen source, which publishes nothing,
                    // kept showing a one-off error forever).
                    if d.video_packets > 0 && published.is_none() && !decode {
                        s.last_error = None;
                    }
                    s.frames_dropped += d.dropped as u64;
                    s.decode_errors += d.decode_errors as u64;
                    s.capture_fps = publish_times.len() as f32;
                    s.device_fps = packet_times.len() as f32;
                    s.decode_ms = decode_ms;
                    s.convert_ms = convert_ms;
                    let mut gap = 0i64;
                    let mut prev: Option<i64> = None;
                    for &t in publish_times.iter().chain(std::iter::once(&now)) {
                        if let Some(p) = prev {
                            gap = gap.max(t - p);
                        }
                        prev = Some(t);
                    }
                    s.max_gap_ms = if publish_times.is_empty() { 1000.0 } else { gap as f32 / 1000.0 };
                    if let Some((latency, replaced)) = published {
                        s.frames_captured += 1;
                        if replaced {
                            s.frames_dropped += 1;
                        }
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
                    s.capture_errors += 1;
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
                // Strict real-time pacing: no initial burst (FFmpeg 6.1's
                // default sends the first 0.5 s at once — frames "before
                // their time", which no physical device can do), and no
                // x264 lookahead.
                "-v", "error", "-re", "-readrate_initial_burst", "0",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30",
                "-c:v", codec, "-q:v", "5", "-g", "15", "-bf", "0",
                "-tune", "zerolatency",
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

    /// A paused source reads and drops packets (device keeps flowing) but
    /// decodes nothing; resuming delivers fresh frames again.
    #[test]
    fn paused_source_decodes_nothing_and_resumes() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let p = port(41);
        let mut tx = live_sender(p, "mjpeg");
        let live = open_live(p);
        let t0 = Instant::now();
        while live.take_latest().is_none() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        live.set_active(false);
        std::thread::sleep(Duration::from_millis(300));
        let _ = live.take_latest();
        let before = live.stats().frames_captured;
        std::thread::sleep(Duration::from_millis(1200));
        let st = live.stats();
        assert_eq!(st.frames_captured, before, "nothing decoded while paused: {st:?}");
        assert!(live.take_latest().is_none());
        assert!(st.device_fps > 10.0, "device packets still drained: {st:?}");
        live.set_active(true);
        let t1 = Instant::now();
        let mut got = false;
        while t1.elapsed() < Duration::from_secs(3) {
            if live.take_latest().is_some() {
                got = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got, "frames resume after re-activation");
        eprintln!("resumed after {:?}", t1.elapsed());
        drop(live);
        let _ = tx.kill();
        let _ = tx.wait();
    }

    /// The live tests each run a real-time ffmpeg sender; run them one at a
    /// time so the senders aren't starved by each other on small CI hosts.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn run_live_case(codec: &str, seed: u16) {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let p = port(seed);
        let mut tx = live_sender(p, codec);
        let live = open_live(p);
        let opened = Instant::now();
        // Warm up: frames flow continuously.
        let t0 = Instant::now();
        let mut got = 0;
        let mut first: Option<(Instant, i64)> = None;
        let mut last: Option<(Instant, i64)> = None;
        while t0.elapsed() < Duration::from_secs(2) {
            if let Some(f) = live.take_latest() {
                got += 1;
                let now = Instant::now();
                first.get_or_insert((now, f.pts_us));
                last = Some((now, f.pts_us));
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        assert!(got > 30, "{codec}: only {got} frames in 2 s");
        // How fast the *sender* really produces media (1.0 = real time). A
        // starved CI host can run the ffmpeg sender below real time; the
        // backlog check must compare against what was actually produced.
        let (f0, l0) = (first.unwrap(), last.unwrap());
        let rate = ((l0.1 - f0.1) as f64 / 1e6) / l0.0.duration_since(f0.0).as_secs_f64().max(0.1);

        // Stall the consumer for 3 s (a frozen UI / slow tick). Live-first:
        // the next frame must be the newest one, not the head of a 3 s
        // backlog.
        let before = live.take_latest().map(|f| (Instant::now(), f.pts_us));
        std::thread::sleep(Duration::from_secs(3));
        let (tf, f) = loop {
            if let Some(f) = live.take_latest() {
                break (Instant::now(), f);
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let age_ms = (crate::clock::now_us() - f.captured_at_us) / 1000;
        // The decisive invariant: frames after the stall continue at the
        // source's pace from `f` — no catch-up jump. A stale `f` (backlog)
        // would be followed by frames racing ahead of wall time.
        let mut g = (tf, f.pts_us);
        while tf.elapsed() < Duration::from_secs(1) {
            if let Some(n) = live.take_latest() {
                g = (Instant::now(), n.pts_us);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let wall_fg = g.0.duration_since(tf).as_secs_f64() * 1e6 * rate.min(1.0);
        let jump_us = (g.1 - f.pts_us) as f64 - wall_fg;
        let advanced = before.map(|(tb, b)| (f.pts_us - b, tf.duration_since(tb).as_millis()));
        assert!(
            jump_us < 300_000.0,
            "{codec}: frames raced {:.0} ms ahead after the stall — the first frame was stale (backlog); advanced {advanced:?}, rate {rate:.2}x, stats {:?}",
            jump_us / 1000.0,
            live.stats()
        );
        if let Some((adv, wall_ms)) = advanced {
            // Informational on hosts where the sender itself stutters.
            eprintln!("{codec}: stall {wall_ms} ms, media advanced {} ms, post-stall jump {:.0} ms", adv / 1000, jump_us / 1000.0);
        }
        // Capture age is only meaningful when the sender keeps real time
        // (a slower sender makes every frame "late" by construction).
        // Shared CI hosts (GitHub's Windows runners) schedule the sender and
        // the capture thread in bursts of a few hundred ms; the bound there
        // still catches any real backlog (which shows up as seconds).
        let max_age = if std::env::var_os("CI").is_some() { 600 } else { 150 };
        if rate > 0.97 {
            assert!(age_ms < max_age, "{codec}: frame is {age_ms} ms old, rate {rate:.2}x, stats {:?}", live.stats());
        } else {
            eprintln!("{codec}: sender ran at {rate:.2}x real time; age check skipped ({age_ms} ms)");
        }
        let st = live.stats();
        assert!(st.frames_dropped > 60, "{codec}: stale frames must be dropped, stats {st:?}");
        // Average publish rate over the whole run. (`capture_fps` is a live
        // 1 s window — it correctly reads 0 while a bursty CI host has the
        // sender descheduled, so it is not asserted at one instant.)
        let avg = st.frames_captured as f64 / opened.elapsed().as_secs_f64();
        eprintln!("{codec}: average capture {avg:.1} fps, window {} fps, device {} pkt/s", st.capture_fps, st.device_fps);
        assert!(avg > 20.0 * rate.min(1.0), "{codec}: average capture {avg:.1} fps, rate {rate:.2}x, stats {st:?}");
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

    /// Measurement (run explicitly): a 1080p60 MJPEG live source — the
    /// mode the user's camera negotiated — through the capture thread.
    /// `cargo test -p sf-capture --release bench_1080p60_mjpeg -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_1080p60_mjpeg() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let p = port(7);
        let mut tx = Command::new("ffmpeg")
            .args([
                "-v", "error", "-re", "-readrate_initial_burst", "0",
                "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=60",
                "-c:v", "mjpeg", "-q:v", "4", "-pix_fmt", "yuvj422p",
                "-f", "nut", &format!("tcp://127.0.0.1:{p}?listen=1&tcp_nodelay=1"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let live = open_live(p);
        for _ in 0..6 {
            std::thread::sleep(Duration::from_secs(1));
            let _ = live.take_latest();
            eprintln!("{:?}", live.stats());
        }
        drop(live);
        let _ = tx.kill();
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
