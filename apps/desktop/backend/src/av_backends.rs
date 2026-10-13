//! Real capture/encode/output/audio backend wiring for the desktop shell.
//!
//! This module owns everything that touches `sf-capture`, `sf-encode`,
//! `sf-output`, and `sf-audio`, keeping `production.rs` focused on the
//! state machines. All logic lives on [`Shell`](super::Shell)
//! (headless-testable, no tauri dependency).
//!
//! # Live pipeline (one tick = one output frame)
//!
//! ```text
//!  capture threads ──newest frame──► [1] source cache (1 pull / source / tick)
//!  mic threads ──timed PCM──► audio bus           │
//!                                │                 ▼
//!                                │        [2] grade + composite (GPU)
//!                                │                 │  program frame (one copy)
//!                                ▼                 ├──► monitor (downscaled)
//!                     [3] aligned mix ──────►  [4] ONE encoder pair
//!                                                   ├──► recorder (rebased)
//!                                                   └──► RTMP send thread
//! ```
//!
//! * **Time-based media clock.** Every tick carries its scheduled time on
//!   the pipeline clock (`sf_capture::clock`). Video PTS = that time (frame
//!   slot), audio block length = wall time since the last tick. A late or
//!   skipped tick therefore never shifts the stream timeline — the old
//!   frame-counted PTS made the stream fall behind real time under load,
//!   which is what grew into a one-minute delay.
//! * **One pull per source per tick.** Program, preview monitor, Color
//!   Studio and every scene read the same cached frame; nobody steals
//!   frames from anybody, and nothing renders a second time on a UI poll.
//! * **One encoder** feeds both the recording and the stream (no duplicate
//!   encode when both run). A sink that joins late gets a forced IDR and
//!   its own timestamp origin.
//! * **A/V alignment.** Live audio is played at the age of the video frame
//!   rendered in the same tick (+ the manual offset); see `sf_audio::bus`.
//!
//! # Stream-key hygiene
//!
//! The key lives in `StreamSettings.key` (never serialized, redacted
//! `Debug`). Every error path that touches the RTMP URL goes through
//! [`sanitize_stream_error`], which redacts the key even if the underlying
//! FFmpeg error echoes the URL.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sf_audio::AudioFrame;
use sf_capture::{
    media::MediaFileProducer, CapturedFrame, CaptureError, FrameProducer, StreamInfo,
};
use sf_encode::{AudioEncoder, EncodedPacket, EncoderId, VideoEncoder, VideoEncoderOpts};
use sf_engine::Frame;
use sf_output::{
    redact_url, ContainerFormat, RealTransport, ReconnectPolicy, Recorder, StreamTarget, Streamer,
};

use super::production::{
    transition_progress, EnhancerView, RecordFormat, RecordRuntime, StreamLifecycle, VoiceStatsView,
};
use super::{Shell, ShellError};
use crate::production::SourceKind;

/// Maps a Video panel encoder name to [`EncoderId`]. `"auto"` is handled by
/// the caller (registry probing); unknown names yield `None`.
fn encoder_id_from_name(name: &str) -> Option<EncoderId> {
    match name {
        "h264_nvenc" => Some(EncoderId::H264Nvenc),
        "h264_qsv" => Some(EncoderId::H264Qsv),
        "h264_amf" => Some(EncoderId::H264Amf),
        "libx264" => Some(EncoderId::H264Libx264),
        _ => None,
    }
}

/// Hard timeout for DirectShow device opens. FFmpeg's interrupt callback
/// is NOT consulted while DirectShow builds its COM filter graph, so a
/// wedged driver (or a device held exclusive by another app) can block
/// `avformat_open_input` forever. Opening on a dedicated thread bounds the
/// damage: on timeout the thread is abandoned and the user gets a clean
/// error instead of a dead app.
#[cfg(any(windows, sf_wincheck))]
const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(8);

/// Runs a device `open` on a dedicated thread with a hard timeout.
/// Also converts an FFmpeg panic into a clean error.
#[cfg(any(windows, sf_wincheck))]
fn open_device<T, F>(label: &str, open: F) -> Result<T, ShellError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, CaptureError> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(format!("sf-open-{label}"))
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(open));
            let _ = tx.send(r);
        })
        .map_err(|e| ShellError::Capture(format!("{label}: cannot spawn open thread: {e}")))?;
    match rx.recv_timeout(DEVICE_OPEN_TIMEOUT) {
        Ok(Ok(Ok(prod))) => Ok(prod),
        Ok(Ok(Err(e))) => Err(ShellError::Capture(format!("{label}: {e}"))),
        Ok(Err(_)) => Err(ShellError::Capture(format!(
            "{label}: the capture backend crashed while opening the device"
        ))),
        Err(_) => Err(ShellError::Capture(format!(
            "{label}: timed out — the device is not responding (it may be in use by another app, or its driver is wedged)"
        ))),
    }
}

/// Moves a live audio producer onto its own capture thread
/// (`sf_audio::LiveInput`). Chunks carry their capture time, so the bus
/// can align them to video. The tick only drains a FIFO.
#[cfg(any(windows, sf_wincheck))]
fn spawn_live_audio(
    source_id: &str,
    mut prod: sf_capture::audio_in::AudioInputProducer,
) -> Result<sf_audio::LiveInput, ShellError> {
    sf_audio::LiveInput::spawn_timed(source_id, move || match prod.next_audio() {
        Ok(Some(c)) => Ok(Some(sf_audio::TimedChunk {
            samples: c.samples,
            captured_at_us: c.captured_at_us,
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    })
    .map_err(|e| ShellError::Capture(format!("cannot start audio capture thread: {e}")))
}

// ---------------------------------------------------------------------------
// Source frame cache
// ---------------------------------------------------------------------------

/// The newest frame of one video source, shared by every consumer of the
/// tick (program, preview monitor, Color Studio, other scenes).
pub(crate) struct SourceFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) data: Arc<Vec<u8>>,
    /// Capture time on the pipeline clock.
    pub(crate) captured_at_us: i64,
    /// Increments on every new frame (resize cache key).
    pub(crate) seq: u64,
    /// Smoothed age of the displayed frame at tick time, µs.
    pub(crate) age_ema_us: f64,
    /// Tick times at which a new frame arrived (last second) → fps.
    arrivals: VecDeque<i64>,
    /// Last capture error (device lost …), shown in diagnostics.
    pub(crate) last_error: Option<String>,
}

impl SourceFrame {
    pub(crate) fn fps(&self) -> f32 {
        self.arrivals.len() as f32
    }
}

/// Video-age smoothing factor per tick (≈ 0.7 s time constant at 30 fps).
const AGE_EMA_ALPHA: f64 = 0.05;

// ---------------------------------------------------------------------------
// Output engine: one encoder pair, fanned out to recorder + stream
// ---------------------------------------------------------------------------

/// Per-sink timestamp rebasing: each output starts at 0 on its own first
/// keyframe, and a resumed recording continues seamlessly.
#[derive(Default)]
struct Rebase {
    /// `out = in − base`; `None` → waiting for the next keyframe.
    base_us: Option<i64>,
    /// Where the next video frame should land after a (re)start.
    next_out_us: i64,
}

impl Rebase {
    /// Rebased copy of `p`, or `None` while waiting for a keyframe.
    fn map(&mut self, p: &EncodedPacket, frame_us: i64) -> Option<EncodedPacket> {
        if self.base_us.is_none() {
            if p.stream_index == 0 && p.is_keyframe {
                self.base_us = Some(p.pts_us - self.next_out_us);
            } else {
                return None;
            }
        }
        let base = self.base_us?;
        let pts = p.pts_us - base;
        let dts = p.dts_us - base;
        if pts < 0 || dts < 0 {
            return None; // audio from before this sink's first frame
        }
        if p.stream_index == 0 {
            self.next_out_us = pts + frame_us;
        }
        Some(EncodedPacket {
            data: p.data.clone(),
            pts_us: pts,
            dts_us: dts,
            is_keyframe: p.is_keyframe,
            stream_index: p.stream_index,
        })
    }

    /// Wait for a keyframe again, continuing the timeline (pause/resume).
    fn restart_continuous(&mut self) {
        self.base_us = None;
    }

    /// Wait for a keyframe again, restarting at 0 (new RTMP connection).
    fn restart_at_zero(&mut self) {
        self.base_us = None;
        self.next_out_us = 0;
    }
}

pub(crate) struct RecordSink {
    recorder: Recorder,
    rebase: Rebase,
    /// Paused on the previous tick (resume → keyframe + continuous rebase).
    was_paused: bool,
    #[allow(dead_code)]
    path: String,
}

/// An active streaming sink: async RTMP send worker + drop accounting.
///
/// The [`SendWorker`](crate::send_worker::SendWorker) owns the RTMP
/// [`Streamer`] on its own thread (OBS's dedicated send thread): the tick
/// hands packets over with non-blocking `try_send`, so a stalled network
/// can never stall rendering.
pub(crate) struct StreamSink {
    send: crate::send_worker::SendWorker,
    rebase: Rebase,
    /// `(dts_us, kind)` of packets handed to the send thread but not yet
    /// acked — drives the buffered-span drop policy and the latency meter.
    pending: VecDeque<(i64, PacketKind)>,
    /// Newest dts handed over (rebased µs).
    newest_dts_us: i64,
    bytes_sent: u64,
    worker_status: crate::send_worker::WorkerStatus,
}

/// The shared encoder pair plus its sinks. Exists while at least one sink
/// (recording or stream) is active.
pub(crate) struct OutputEngine {
    video_enc: VideoEncoder,
    audio_enc: AudioEncoder,
    pub(crate) record: Option<RecordSink>,
    pub(crate) stream: Option<StreamSink>,
    /// Frames actually encoded (diagnostics: encoder fps).
    encoded_times: VecDeque<i64>,
    /// Ticks whose frame mapped to an already-encoded slot.
    pub(crate) skipped_slots: u64,
    pub(crate) last_audio_encode_us: u64,
}

// SAFETY: FFmpeg encoder/muxer contexts are `!Send` only by conservatism —
// they carry no thread affinity, and the engine is only ever touched
// through the shell's `Mutex`.
unsafe impl Send for OutputEngine {}

impl OutputEngine {
    pub(crate) fn encoder_name(&self) -> &'static str {
        self.video_enc.encoder_name()
    }

    pub(crate) fn encoder_is_hardware(&self) -> bool {
        self.video_enc.encoder_id().is_hardware()
    }

    pub(crate) fn last_encode_us(&self) -> u64 {
        self.video_enc.last_encode_us()
    }

    pub(crate) fn encoder_fps(&self) -> f32 {
        self.encoded_times.len() as f32
    }

    pub(crate) fn has_sinks(&self) -> bool {
        self.record.is_some() || self.stream.is_some()
    }
}

impl StreamSink {
    pub(crate) fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    /// Video handed to the send thread but not yet on the network, µs.
    pub(crate) fn buffered_video_us(&self) -> i64 {
        buffered_video_us(&self.pending, self.newest_dts_us)
    }

    pub(crate) fn is_live(&self) -> bool {
        self.worker_status == crate::send_worker::WorkerStatus::Live
    }
}

/// Packet class for the drop policy: video delta frames may be shed under
/// congestion; audio and keyframes never are (OBS `drop_frames`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PacketKind {
    VideoKey,
    VideoDelta,
    Audio,
}

/// Buffered video span over a pending-packet deque: newest dts minus the
/// oldest unacked *video* dts (audio doesn't count toward the video buffer
/// metric, mirroring OBS's `first_video` tracking).
pub(crate) fn buffered_video_us(
    pending: &VecDeque<(i64, PacketKind)>,
    newest_dts: i64,
) -> i64 {
    let oldest = pending
        .iter()
        .filter(|(_, k)| matches!(k, PacketKind::VideoKey | PacketKind::VideoDelta))
        .map(|(d, _)| *d)
        .next();
    oldest.map(|o| (newest_dts - o).max(0)).unwrap_or(0)
}

/// OBS `check_to_drop_frames` decision: shed this packet when congested?
/// Only non-keyframe video is ever shed — audio and keyframes always pass.
pub(crate) fn drop_decision(kind: PacketKind, buffered_us: i64) -> bool {
    kind == PacketKind::VideoDelta && buffered_us > crate::send_worker::DROP_VIDEO_BUFFER_US
}

// ---------------------------------------------------------------------------
// Producers
// ---------------------------------------------------------------------------

/// Producers hold FFmpeg decoder/scaler contexts, which are raw pointers
/// and therefore `!Send`. Every producer is only ever touched through the
/// shell's `Mutex`, so this wrapper is sound.
pub(crate) enum ProducerKind {
    Media(MediaFileProducer),
    #[cfg(any(windows, sf_wincheck))]
    Camera(sf_capture::dshow::DshowProducer),
    #[cfg(any(windows, sf_wincheck))]
    Screen(sf_capture::gdigrab::ScreenProducer),
}

// SAFETY: see above — all access is serialized by the shell's `Mutex`.
unsafe impl Send for ProducerKind {}

pub(crate) struct SendProducer(pub(crate) ProducerKind);

// SAFETY: see above — all access is serialized by the shell's `Mutex`.
unsafe impl Send for SendProducer {}

impl std::ops::Deref for SendProducer {
    type Target = ProducerKind;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SendProducer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FrameProducer for ProducerKind {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        match self {
            ProducerKind::Media(p) => p.next_frame(),
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Camera(p) => p.next_frame(),
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Screen(p) => p.next_frame(),
        }
    }

    fn stream_info(&self) -> StreamInfo {
        match self {
            ProducerKind::Media(p) => p.stream_info(),
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Camera(p) => p.stream_info(),
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Screen(p) => p.stream_info(),
        }
    }
}

impl ProducerKind {
    /// Next normalized audio chunk, if the producer carries audio itself
    /// (media files). Live cameras deliver audio through a separate
    /// `LiveInput` under the same source id.
    fn next_audio(&mut self) -> Result<Option<sf_capture::media::DecodedAudio>, CaptureError> {
        match self {
            ProducerKind::Media(p) => p.next_audio(),
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Camera(_) | ProducerKind::Screen(_) => Ok(None),
        }
    }

    /// Live devices decode only while shown (no-op for files).
    fn set_active(&self, _active: bool) {
        match self {
            ProducerKind::Media(_) => {}
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Camera(p) => p.set_active(_active),
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Screen(p) => p.set_active(_active),
        }
    }

    fn is_live_device(&self) -> bool {
        !matches!(self, ProducerKind::Media(_))
    }

    /// Live capture-thread stats (devices only).
    pub(crate) fn live_stats(&self) -> Option<sf_capture::LiveVideoStats> {
        match self {
            ProducerKind::Media(_) => None,
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Camera(p) => Some(p.live_stats()),
            #[cfg(any(windows, sf_wincheck))]
            ProducerKind::Screen(p) => Some(p.live_stats()),
        }
    }
}

/// Nearest-neighbor RGBA resize (CPU fallback when the GPU resize fails).
pub(crate) fn resize_rgba_nearest(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw.max(1), sh.max(1), dw.max(1), dh.max(1));
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    if src.len() < sw as usize * sh as usize * 4 {
        return out;
    }
    for y in 0..dh {
        let sy = (y as u64 * sh as u64 / dh as u64) as u32;
        for x in 0..dw {
            let sx = (x as u64 * sw as u64 / dw as u64) as u32;
            let s = ((sy * sw + sx) * 4) as usize;
            let d = ((y * dw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

/// Build a stream error message that can never leak the key, even if the
/// underlying FFmpeg error echoes the URL.
fn sanitize_stream_error(url: &str, key: &str, what: &str, err: &dyn std::fmt::Display) -> String {
    let mut msg = format!("{what} ({}): {err}", redact_url(url));
    if !key.is_empty() {
        msg = msg.replace(key, "[redacted]");
    }
    msg
}

impl Shell {
    // ------------------------------------------------------------------
    // Media producers
    // ------------------------------------------------------------------

    /// Opens a media-file producer for a source. Called from `source_add`
    /// before the source is pushed, so a bad file fails cleanly.
    pub(crate) fn open_media_producer(
        &mut self,
        source_id: &str,
        path: &str,
    ) -> Result<(), ShellError> {
        if path.trim().is_empty() {
            return Err(ShellError::Capture(
                "no media file selected — pick a file with Browse…".to_string(),
            ));
        }
        let prod = MediaFileProducer::open(path)
            .map_err(|e| ShellError::Capture(format!("cannot open media '{path}': {e}")))?;
        self.media_producers.insert(
            source_id.to_string(),
            SendProducer(ProducerKind::Media(prod)),
        );
        Ok(())
    }

    /// Opens a DirectShow camera producer for a source (Windows only).
    /// Video and audio open as SEPARATE dshow graphs (like OBS's separate
    /// video/audio sources), each with its own non-blocking capture thread.
    #[cfg(any(windows, sf_wincheck))]
    pub(crate) fn open_camera_producer(
        &mut self,
        source_id: &str,
        device: Option<&str>,
        audio_device: Option<&str>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), ShellError> {
        let device_owned = device.map(|d| d.to_string());
        let audio_owned = audio_device.map(|d| d.to_string());
        let prod: sf_capture::dshow::DshowProducer = open_device("camera", move || {
            match device_owned.as_deref() {
                Some(d) => sf_capture::dshow::DshowProducer::open(d, width, height, fps),
                None => sf_capture::dshow::DshowProducer::open_default(width, height, fps),
            }
        })?;
        self.camera_formats
            .insert(source_id.to_string(), prod.negotiated_format().to_string());
        self.media_producers.insert(
            source_id.to_string(),
            SendProducer(ProducerKind::Camera(prod)),
        );
        if let Some(a) = audio_owned {
            if !a.is_empty() {
                let audio_prod: sf_capture::audio_in::AudioInputProducer =
                    open_device("camera audio", move || {
                        sf_capture::audio_in::AudioInputProducer::open(Some(a.as_str()))
                    })?;
                let live = spawn_live_audio(source_id, audio_prod)?;
                self.audio_inputs.insert(source_id.to_string(), live);
            }
        }
        Ok(())
    }

    /// Opens a desktop screen producer (Windows gdigrab).
    #[cfg(any(windows, sf_wincheck))]
    pub(crate) fn open_screen_producer(&mut self, source_id: &str) -> Result<(), ShellError> {
        let prod: sf_capture::gdigrab::ScreenProducer =
            open_device("screen capture", || sf_capture::gdigrab::ScreenProducer::open(30))?;
        self.media_producers.insert(
            source_id.to_string(),
            SendProducer(ProducerKind::Screen(prod)),
        );
        Ok(())
    }

    /// Negotiated camera format for a source, e.g.
    /// `"mjpeg 1920x1080 @30fps (via mjpeg)"`.
    pub fn camera_format(&self, source_id: &str) -> Option<String> {
        self.camera_formats.get(source_id).cloned()
    }

    /// Audio device label for a source: the mic name for audio inputs,
    /// the camera's audio device when set.
    pub fn source_audio_device(&self, source_id: &str) -> Option<String> {
        let src = self
            .scenes
            .iter()
            .flat_map(|sc| sc.sources.iter())
            .find(|s| s.id == source_id)?;
        match &src.kind {
            SourceKind::AudioInput { device } => device.clone(),
            SourceKind::Camera { audio_device, .. } => audio_device.clone(),
            _ => None,
        }
    }

    /// Opens a DirectShow audio-input producer (microphone/line-in,
    /// Windows only) on its own capture thread.
    #[cfg(any(windows, sf_wincheck))]
    pub(crate) fn open_audio_producer(
        &mut self,
        source_id: &str,
        device: Option<&str>,
    ) -> Result<(), ShellError> {
        let device_owned = device.map(|d| d.to_string());
        let prod: sf_capture::audio_in::AudioInputProducer =
            open_device("audio input", move || {
                sf_capture::audio_in::AudioInputProducer::open(device_owned.as_deref())
            })?;
        let live = spawn_live_audio(source_id, prod)?;
        self.audio_inputs.insert(source_id.to_string(), live);
        Ok(())
    }

    // ------------------------------------------------------------------
    // [1] Source frame cache
    // ------------------------------------------------------------------

    /// Capture stage of the tick: pull every video producer exactly once
    /// (newest frame — the capture threads already dropped anything older)
    /// and update per-source age / fps.
    /// Captures shown on Preview, Program, a running transition or the
    /// Color page keep decoding; every other live device is paused (reads
    /// and drops packets, no decode/convert/GPU work).
    fn update_source_activity(&mut self) {
        let mut scenes: Vec<&str> = vec![self.program_scene_id.as_str(), self.preview_scene_id.as_str()];
        if let Some(t) = &self.transition {
            scenes.push(t.from_scene.as_str());
            scenes.push(t.to_scene.as_str());
        }
        let watched_input = self.studio_watch.as_ref().map(|(id, _, _)| id.clone());
        let shown: std::collections::HashSet<String> = self
            .scenes
            .iter()
            .flat_map(|sc| {
                let on_air = scenes.contains(&sc.id.as_str());
                sc.sources.iter().filter(move |s| s.visible && on_air)
            })
            .chain(
                self.scenes
                    .iter()
                    .flat_map(|sc| sc.sources.iter())
                    .filter(|s| s.input_id.is_some() && s.input_id == watched_input),
            )
            .map(|s| self.frame_owner(&s.id).to_string())
            .collect();
        for (id, p) in &self.media_producers {
            if !p.0.is_live_device() {
                continue;
            }
            let active = shown.contains(id);
            let was = !self.paused_sources.contains(id);
            if active != was {
                p.0.set_active(active);
                if active {
                    self.paused_sources.remove(id);
                } else {
                    self.paused_sources.insert(id.clone());
                }
            }
        }
        self.paused_sources.retain(|id| self.media_producers.contains_key(id));
    }

    pub(crate) fn refresh_source_frames(&mut self, tick_us: i64) {
        self.update_source_activity();
        let ids: Vec<String> = self
            .media_producers
            .keys()
            .filter(|id| !self.paused_sources.contains(*id))
            .cloned()
            .collect();
        for id in ids {
            let Some(prod) = self.media_producers.get_mut(&id) else {
                continue;
            };
            let result = prod.next_frame();
            match result {
                Ok(Some(f)) => {
                    let entry = self.source_frames.entry(id.clone()).or_insert_with(|| {
                        SourceFrame {
                            width: f.width,
                            height: f.height,
                            data: Arc::new(Vec::new()),
                            captured_at_us: f.captured_at_us,
                            seq: 0,
                            age_ema_us: (tick_us - f.captured_at_us).max(0) as f64,
                            arrivals: VecDeque::new(),
                            last_error: None,
                        }
                    });
                    entry.width = f.width;
                    entry.height = f.height;
                    entry.captured_at_us = f.captured_at_us;
                    entry.data = Arc::new(f.data);
                    entry.seq += 1;
                    entry.arrivals.push_back(tick_us);
                    entry.last_error = None;
                }
                Ok(None) => {}
                Err(CaptureError::EndOfStream) => {
                    // Media file ended: freeze on the last frame; stop
                    // polling the demuxer.
                    self.media_producers.remove(&id);
                }
                Err(e) => {
                    if let Some(entry) = self.source_frames.get_mut(&id) {
                        entry.last_error = Some(e.to_string());
                    }
                }
            }
            if let Some(entry) = self.source_frames.get_mut(&id) {
                while entry
                    .arrivals
                    .front()
                    .is_some_and(|&t| tick_us - t > 1_000_000)
                {
                    entry.arrivals.pop_front();
                }
                let age = (tick_us - entry.captured_at_us).max(0) as f64;
                entry.age_ema_us += AGE_EMA_ALPHA * (age - entry.age_ema_us);
            }
        }
    }

    /// The current frame of a video source at `w`×`h`: served from the
    /// per-tick cache (resized once per new frame per size). Never pulls
    /// the producer — so a UI poll or a second scene can't steal frames.
    pub(crate) fn pull_media_frame(
        &mut self,
        source_id: &str,
        w: u32,
        h: u32,
    ) -> Option<Arc<Vec<u8>>> {
        let source_id = self.frame_owner(source_id).to_string();
        let source_id = source_id.as_str();
        let f = self.source_frames.get(source_id)?;
        if f.data.is_empty() {
            return None;
        }
        if f.width == w && f.height == h {
            return Some(Arc::clone(&f.data));
        }
        let key = (source_id.to_string(), w, h);
        if let Some((seq, px)) = self.resized_frames.get(&key) {
            if *seq == f.seq {
                return Some(Arc::clone(px));
            }
        }
        let (sw, sh, seq, data) = (f.width, f.height, f.seq, Arc::clone(&f.data));
        let px = match self.engine.gpu_resize_rgba(&data, sw, sh, w, h) {
            Ok(px) => px,
            Err(_) => resize_rgba_nearest(&data, sw, sh, w, h),
        };
        let px = Arc::new(px);
        // Bounded: during a resize drag a frozen source would otherwise
        // keep one copy per intermediate size.
        if self.resized_frames.len() >= 16 {
            self.resized_frames.retain(|(sid, _, _), _| sid != source_id);
        }
        self.resized_frames.insert(key, (seq, Arc::clone(&px)));
        Some(px)
    }

    /// The source whose capture `id` shows (itself unless it is a shared
    /// scene item).
    pub(crate) fn frame_owner<'a>(&'a self, id: &'a str) -> &'a str {
        self.producer_alias.get(id).map(String::as_str).unwrap_or(id)
    }

    /// Native size of a source's latest frame.
    pub(crate) fn source_native_size(&self, id: &str) -> Option<(u32, u32)> {
        let f = self.source_frames.get(self.frame_owner(id))?;
        (!f.data.is_empty()).then_some((f.width, f.height))
    }

    /// An existing source (any scene) already capturing the same device /
    /// file as `kind`, to share instead of opening it again.
    pub(crate) fn shared_producer_for(&self, kind: &SourceKind) -> Option<String> {
        let same = |k: &SourceKind| match (k, kind) {
            (SourceKind::Camera { device: a, .. }, SourceKind::Camera { device: b, .. }) => a == b,
            (SourceKind::Screen, SourceKind::Screen) => true,
            _ => false,
        };
        self.scenes
            .iter()
            .flat_map(|s| s.sources.iter())
            .find(|s| same(&s.kind))
            .map(|s| self.frame_owner(&s.id).to_string())
            .filter(|owner| self.media_producers.contains_key(owner) || self.source_frames.contains_key(owner))
    }

    /// Smoothed age of the program's main live picture (first visible
    /// camera, else screen, else media) — the A/V alignment reference.
    pub(crate) fn program_video_age_us(&self) -> Option<i64> {
        let scene = self
            .scenes
            .iter()
            .find(|s| s.id == self.program_scene_id)?;
        let rank = |k: &SourceKind| match k {
            SourceKind::Camera { .. } => Some(0),
            SourceKind::Screen => Some(1),
            SourceKind::Media { .. } => Some(2),
            _ => None,
        };
        scene
            .sources
            .iter()
            .filter(|s| s.visible)
            .filter_map(|s| Some((rank(&s.kind)?, s)))
            .filter(|(_, s)| self.source_frames.contains_key(self.frame_owner(&s.id)))
            .min_by_key(|(r, s)| (*r, s.order))
            .and_then(|(_, s)| self.source_frames.get(self.frame_owner(&s.id)))
            .map(|f| f.age_ema_us as i64)
    }

    /// Requested play age for a live audio source (see `sf_audio::bus`):
    /// its own camera's picture age (camera audio) or the program
    /// picture's age (external mic), plus the manual offset.
    fn audio_target_age_us(&self, audio_id: &str) -> Option<i64> {
        let offset = self.av_sync.offset_ms as i64 * 1000;
        if !self.av_sync.auto {
            return Some(offset.max(0));
        }
        let video_age = self
            .source_frames
            .get(audio_id)
            .map(|f| f.age_ema_us as i64)
            .or_else(|| self.program_video_age_us())?;
        Some((video_age + offset).max(0))
    }

    /// Pause/resume a media source's playback.
    pub fn media_set_paused(&mut self, source_id: &str, paused: bool) -> Result<bool, ShellError> {
        let prod = self
            .media_producers
            .get_mut(source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        #[allow(unreachable_patterns)]
        match &mut prod.0 {
            ProducerKind::Media(p) => {
                p.set_paused(paused);
                Ok(p.is_paused())
            }
            _ => Err(ShellError::Capture(
                "pause is only available for media file sources".into(),
            )),
        }
    }

    /// Enable/disable local speaker monitoring of the master mix.
    pub fn set_monitor_enabled(&mut self, enabled: bool) -> Result<(), ShellError> {
        if enabled && self.audio_monitor.is_none() {
            match crate::audio_monitor::AudioMonitor::open() {
                Ok(m) => self.audio_monitor = Some(m),
                Err(e) => {
                    return Err(ShellError::Capture(format!(
                        "cannot start audio monitor: {e}"
                    )))
                }
            }
        } else if !enabled {
            self.audio_monitor = None;
        }
        Ok(())
    }

    /// True while local speaker monitoring is active.
    pub fn monitor_enabled(&self) -> bool {
        self.audio_monitor.is_some()
    }

    /// Current enhancer settings for the UI.
    pub fn enhancer_settings(&self) -> EnhancerView {
        let s = &self.enhancer_settings;
        EnhancerView {
            enabled: s.enabled,
            gate_enabled: s.gate_enabled,
            gate_threshold_db: s.gate_threshold_db,
            eq_low_db: s.eq_low_db,
            eq_mid_db: s.eq_mid_db,
            eq_high_db: s.eq_high_db,
            compressor_enabled: s.compressor_enabled,
            ai_denoise: s.ai_denoise,
            ai_strength: s.ai_strength,
        }
    }

    /// Applies enhancer settings from the UI (clamped to sane ranges).
    pub fn set_enhancer_settings(&mut self, v: EnhancerView) {
        let num = |x: f32, lo: f32, hi: f32, dflt: f32| {
            if x.is_finite() {
                x.clamp(lo, hi)
            } else {
                dflt
            }
        };
        let settings = sf_audio::EnhancerSettings {
            enabled: v.enabled,
            gate_enabled: v.gate_enabled,
            gate_threshold_db: num(v.gate_threshold_db, -60.0, 0.0, -40.0),
            eq_low_db: num(v.eq_low_db, -12.0, 12.0, 0.0),
            eq_mid_db: num(v.eq_mid_db, -12.0, 12.0, 0.0),
            eq_high_db: num(v.eq_high_db, -12.0, 12.0, 0.0),
            compressor_enabled: v.compressor_enabled,
            ai_denoise: v.ai_denoise,
            ai_strength: num(v.ai_strength, 0.0, 1.0, 1.0),
        };
        for e in self.voice_enhancers.values_mut() {
            e.set_settings(settings.clone());
        }
        self.enhancer_settings = settings;
    }

    /// A/V sync settings (auto alignment + manual offset).
    pub fn av_sync_get(&self) -> crate::production::AvSyncView {
        self.av_sync.clone()
    }

    /// Sets A/V sync: `auto` aligns audio to the picture's measured age;
    /// `offset_ms` (−100…+100, + = later audio) is added on top.
    pub fn av_sync_set(&mut self, auto: bool, offset_ms: i32) -> crate::production::AvSyncView {
        self.av_sync = crate::production::AvSyncView {
            auto,
            offset_ms: offset_ms.clamp(-100, 100),
        };
        self.av_sync.clone()
    }

    /// Live per-voice-source telemetry for the UI (enhancer + bus + device).
    pub fn voice_stats(&self) -> std::collections::HashMap<String, VoiceStatsView> {
        self.audio_inputs
            .iter()
            .map(|(id, inp)| {
                let e = self
                    .voice_enhancers
                    .get(id)
                    .map(|e| e.stats())
                    .unwrap_or_default();
                let b = self.audio_bus.stats(id);
                let device = match inp.status() {
                    sf_audio::LiveStatus::Starting => "starting".to_string(),
                    sf_audio::LiveStatus::Running => "running".to_string(),
                    sf_audio::LiveStatus::Error(m) => format!("error: {m}"),
                    sf_audio::LiveStatus::Stopped => "stopped".to_string(),
                };
                (
                    id.clone(),
                    VoiceStatsView {
                        vad: e.vad,
                        gain_reduction_db: e.gain_reduction_db,
                        gate_closed: e.gate_closed,
                        latency_ms: e.latency_ms,
                        process_us: e.process_us,
                        buffered_ms: b.buffered_ms,
                        underruns: b.underruns,
                        dropped_frames: b.dropped_frames + inp.dropped_frames(),
                        device,
                        audio_age_ms: b.audio_age_ms,
                        target_age_ms: b.target_age_ms,
                        corrections: b.corrections,
                    },
                )
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // [3] Audio: pull + align + enhance + mix + meters
    // ------------------------------------------------------------------

    /// Mixes exactly `frames` sample frames (48 kHz stereo) for the tick at
    /// `tick_us`. `frames` comes from the wall-clock [`sf_audio::AudioTimeline`],
    /// so the audio timeline always covers real elapsed time.
    ///
    /// 1. Live inputs: drain their capture threads (with capture times).
    /// 2. Media files: decode until the bus holds one block + prebuffer.
    /// 3. Live sources are pulled *aligned* to the picture's age; voice
    ///    sources go through their enhancer pre-fader; the mixer sums.
    pub(crate) fn pump_audio(&mut self, frames: usize, tick_us: i64) -> Vec<f32> {
        let media_ids: Vec<String> = self
            .media_producers
            .iter()
            .filter(|(id, p)| p.stream_info().has_audio && !self.audio_inputs.contains_key(*id))
            .map(|(id, _)| id.clone())
            .collect();
        let live_ids: Vec<String> = self.audio_inputs.keys().cloned().collect();
        let ids: Vec<String> = media_ids.iter().chain(live_ids.iter()).cloned().collect();

        for id in &ids {
            if !self.audio_mixer.source_ids().contains(id) {
                let gain = self.mixer.channels.get(id).map(|c| c.gain).unwrap_or(1.0);
                self.audio_mixer.add_source(id, gain);
            }
        }
        for id in self.audio_mixer.source_ids() {
            if !ids.contains(&id) {
                self.audio_mixer.remove_source(&id);
            }
        }
        for id in &ids {
            if let Some(strip) = self.mixer.channels.get(id) {
                self.audio_mixer.set_gain(id, strip.gain);
                self.audio_mixer.set_mute(id, strip.mute);
            }
        }
        self.audio_mixer.set_master_gain(self.mixer.master.gain);
        self.audio_mixer.set_master_mute(self.mixer.master.mute);
        self.audio_bus.retain(&ids);
        self.voice_enhancers.retain(|k, _| live_ids.contains(k));

        if ids.is_empty() || frames == 0 {
            return vec![0.0; frames * 2];
        }

        let mut tmp: Vec<f32> = Vec::new();
        for (id, input) in &self.audio_inputs {
            tmp.clear();
            let at = input.drain_into_timed(&mut tmp);
            if !tmp.is_empty() {
                self.audio_bus.push_timed(id, &tmp, at);
            }
        }
        let target = self.audio_bus.refill_target_frames(frames);
        for id in &media_ids {
            let Some(prod) = self.media_producers.get_mut(id) else {
                continue;
            };
            let mut guard = 0;
            while self.audio_bus.buffered_frames(id) < target && guard < 64 {
                guard += 1;
                match prod.next_audio() {
                    Ok(Some(c)) if c.channels == 2 && c.sample_rate == 48_000 => {
                        self.audio_bus.push(id, &c.samples)
                    }
                    Ok(Some(_)) => {}
                    _ => break,
                }
            }
        }

        let mut blocks: Vec<(String, Vec<f32>)> = Vec::with_capacity(ids.len());
        for id in &ids {
            let mut block = Vec::with_capacity(frames * 2);
            if live_ids.contains(id) {
                let target_age = self.audio_target_age_us(id);
                self.audio_bus
                    .pull_aligned(id, frames, &mut block, tick_us, target_age);
                let settings = &self.enhancer_settings;
                let enh = self.voice_enhancers.entry(id.clone()).or_insert_with(|| {
                    let mut e = sf_audio::Enhancer::new(48_000);
                    e.set_settings(settings.clone());
                    e
                });
                enh.process(&mut block);
            } else {
                self.audio_bus.pull(id, frames, &mut block);
            }
            if let Some(strip) = self.mixer.channels.get(id) {
                crate::production::apply_trim_pan(&mut block, strip.trim_db, strip.pan);
            }
            blocks.push((id.clone(), block));
        }
        // Solo bus for the local monitor (AFL: post-fader, ignores mute).
        let soloed: Vec<(&String, f32)> = self
            .mixer
            .channels
            .iter()
            .filter(|(_, c)| c.solo)
            .map(|(id, c)| (id, c.gain))
            .collect();
        self.monitor_solo = if soloed.is_empty() {
            None
        } else {
            let mut bus = vec![0f32; frames * 2];
            for (id, b) in &blocks {
                if let Some((_, g)) = soloed.iter().find(|(sid, _)| *sid == id) {
                    for (o, x) in bus.iter_mut().zip(b.iter()) {
                        *o += x * g;
                    }
                }
            }
            for o in &mut bus {
                *o = o.clamp(-1.0, 1.0);
            }
            Some(bus)
        };
        let frames_in: Vec<(&str, AudioFrame)> = blocks
            .iter()
            .map(|(id, b)| (id.as_str(), AudioFrame::new(b, 2, 48_000)))
            .collect();
        let mixed = self.audio_mixer.mix(&frames_in);
        for m in &mixed.per_source {
            self.meter_values.insert(m.id.clone(), m.peak);
            let e = self.meter_hold.entry(m.id.clone()).or_insert((0.0, 0.0));
            e.0 = e.0.max(m.peak);
            e.1 = m.rms;
        }
        self.master_meter_hold = (self.master_meter_hold.0.max(mixed.peak), mixed.rms);
        self.meters_live = true;
        mixed.samples
    }

    // ------------------------------------------------------------------
    // [2] Program frame
    // ------------------------------------------------------------------

    /// Raw RGBA program frame: live crossfade during an auto transition,
    /// otherwise the program scene composite.
    pub(crate) fn program_frame_raw(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Frame, ShellError> {
        let active = self.transition.as_ref().map(|t| {
            let elapsed = t.started.elapsed().as_millis() as u64;
            (
                t.from_scene.clone(),
                t.to_scene.clone(),
                transition_progress(elapsed, t.duration_ms),
                t.kind,
            )
        });
        match active {
            Some((from, to, progress, kind)) => {
                let a = self.render_scene_frame(&from, width, height)?;
                let b = self.render_scene_frame(&to, width, height)?;
                Ok(crate::production::transition_blend(kind, &a, &b, progress))
            }
            None => {
                let program = self.program_scene_id.clone();
                self.render_scene_frame(&program, width, height)
            }
        }
    }

    // ------------------------------------------------------------------
    // [4] Output engine
    // ------------------------------------------------------------------

    fn video_encoder_opts(&self) -> VideoEncoderOpts {
        let vs = &self.video_settings;
        VideoEncoderOpts {
            width: vs.output_width,
            height: vs.output_height,
            fps_num: vs.output_fps,
            fps_den: 1,
            bitrate_bps: vs.bitrate_kbps * 1000,
            // 2-second keyframe interval: YouTube/Facebook/TikTok ingest
            // requirement (≤ 2–4 s) and fast viewer join.
            gop: vs.output_fps * 2,
            preset: "veryfast".to_string(),
        }
    }

    /// Opens the video encoder honoring the Video panel's encoder override:
    /// `"auto"` probes NVENC → QSV → AMF → x264 → mpeg4 (falls back when an
    /// encoder exists in the registry but fails to open on this machine).
    fn open_video_encoder(&mut self, opts: VideoEncoderOpts) -> Result<VideoEncoder, ShellError> {
        let name = self.video_settings.encoder.clone();
        self.encoder_note = None;
        if name != "auto" {
            if let Some(id) = encoder_id_from_name(&name) {
                match VideoEncoder::open_with_encoder(id, opts.clone()) {
                    Ok(enc) => return Ok(enc),
                    Err(e) => {
                        // e.g. NVENC selected on a machine without an
                        // NVIDIA GPU: fall back, and say so in diagnostics.
                        self.encoder_note = Some(format!(
                            "{name} unavailable ({e}); using automatic selection"
                        ));
                    }
                }
            }
        }
        VideoEncoder::open(opts).map_err(|e| ShellError::Capture(format!("video encoder: {e}")))
    }

    /// Creates the shared encoder pair if no sink is running yet.
    fn ensure_output_engine(&mut self) -> Result<(), String> {
        if self.output.is_some() {
            return Ok(());
        }
        let opts = self.video_encoder_opts();
        let video_enc = self.open_video_encoder(opts).map_err(|e| e.to_string())?;
        let audio_enc = AudioEncoder::open().map_err(|e| format!("audio encoder: {e}"))?;
        self.output = Some(OutputEngine {
            video_enc,
            audio_enc,
            record: None,
            stream: None,
            encoded_times: VecDeque::new(),
            skipped_slots: 0,
            last_audio_encode_us: 0,
        });
        Ok(())
    }

    /// Drops the encoder pair once no sink uses it.
    fn release_output_engine_if_idle(&mut self) {
        if self.output.as_ref().is_some_and(|o| !o.has_sinks()) {
            self.output = None;
        }
    }

    /// Rebuilds the shared video encoder with the current output settings
    /// (live fps change). The successor is opened first, so a failure
    /// leaves the working encoder untouched. Its first frame is an IDR and
    /// PTS stay on the same wall clock.
    pub(crate) fn reinit_video_encoders(&mut self) -> Result<(), ShellError> {
        if self.output.is_some() {
            let opts = self.video_encoder_opts();
            let enc = self.open_video_encoder(opts)?;
            if let Some(o) = self.output.as_mut() {
                o.video_enc = enc;
            }
        }
        Ok(())
    }

    /// Starts recording the program output to `record_settings.path`
    /// (fragmented MP4 / MKV). Shares the encoder with a running stream.
    pub fn record_start(&mut self) -> Result<(), ShellError> {
        let can_start = self.record_runtime.is_none() || self.record_error.is_some();
        if !can_start {
            return Err(ShellError::Record(
                "recording is already running or paused".to_string(),
            ));
        }
        let path = self.record_settings.path.clone();
        if path.is_empty() {
            let m = "no recording output path set".to_string();
            self.record_error = Some(m.clone());
            self.emit_record_state();
            return Err(ShellError::Record(m));
        }
        let fmt = match self.record_settings.format {
            RecordFormat::Mp4 => ContainerFormat::Mp4Fragmented,
            RecordFormat::Mkv => ContainerFormat::Mkv,
        };
        let build = |s: &mut Shell| -> Result<(), String> {
            s.ensure_output_engine()?;
            let o = s.output.as_mut().expect("engine exists");
            let mut recorder =
                Recorder::create(&path, fmt).map_err(|e| format!("recorder: {e}"))?;
            recorder
                .add_video_stream(&o.video_enc.parameters())
                .map_err(|e| format!("recorder: {e}"))?;
            // Always an audio track (silent until a source plays): a mic
            // added mid-recording must still be heard.
            recorder
                .add_audio_stream(&o.audio_enc.parameters(), o.audio_enc.sample_rate())
                .map_err(|e| format!("recorder: {e}"))?;
            o.video_enc.force_keyframe();
            o.record = Some(RecordSink {
                recorder,
                rebase: Rebase::default(),
                was_paused: false,
                path: path.clone(),
            });
            Ok(())
        };
        match build(self) {
            Ok(()) => {
                self.record_runtime = Some(RecordRuntime {
                    started: Instant::now(),
                    accumulated: Duration::ZERO,
                    paused: false,
                });
                self.record_error = None;
                self.emit_record_state();
                Ok(())
            }
            Err(m) => {
                if let Some(o) = self.output.as_mut() {
                    o.record = None;
                }
                self.release_output_engine_if_idle();
                self.record_runtime = None;
                self.record_error = Some(m.clone());
                self.emit_record_state();
                Err(ShellError::Record(m))
            }
        }
    }

    /// Stops recording and finalizes the file. The encoder keeps running
    /// if the stream still uses it.
    pub fn record_stop(&mut self) -> Result<(), ShellError> {
        let sink = self.output.as_mut().and_then(|o| o.record.take());
        if let Some(mut rec) = sink {
            let frame_us = self.frame_interval_us() as i64;
            let mut failed: Option<String> = None;
            // Last sink: flush the encoders into the file (zero-latency
            // settings leave nothing buffered in video; AAC may hold one
            // frame). Otherwise the stream keeps the encoder.
            if self.output.as_ref().is_some_and(|o| o.stream.is_none()) {
                let o = self.output.as_mut().expect("checked");
                let mut pkts = o.video_enc.flush().unwrap_or_default();
                pkts.extend(o.audio_enc.flush().unwrap_or_default());
                for p in &pkts {
                    if let Some(p) = rec.rebase.map(p, frame_us) {
                        let r = if p.stream_index == 0 {
                            rec.recorder.write_video(&p)
                        } else {
                            rec.recorder.write_audio(&p)
                        };
                        if let Err(e) = r {
                            failed = Some(format!("write: {e}"));
                            break;
                        }
                    }
                }
            }
            if failed.is_none() {
                if let Err(e) = rec.recorder.finish() {
                    failed = Some(format!("finalize: {e}"));
                }
            }
            if let Some(m) = failed {
                self.record_error = Some(m);
            }
        }
        self.release_output_engine_if_idle();
        self.record_runtime = None;
        self.emit_record_state();
        Ok(())
    }

    /// Starts streaming: spawns the async RTMP send worker (connects on
    /// its own thread). Shares the encoder with a running recording.
    pub fn stream_start(&mut self) -> Result<(), ShellError> {
        self.stream_start_with_policy(ReconnectPolicy::default())
    }

    /// [`stream_start`](Self::stream_start) with an explicit reconnect policy.
    pub fn stream_start_with_policy(
        &mut self,
        policy: ReconnectPolicy,
    ) -> Result<(), ShellError> {
        match self.stream_lifecycle {
            StreamLifecycle::Idle | StreamLifecycle::Error { .. } => {}
            _ => {
                return Err(ShellError::Stream(
                    "stream is already running or starting".to_string(),
                ))
            }
        }
        // A failed previous session may still hold its sink.
        if let Some(old) = self.output.as_mut().and_then(|o| o.stream.take()) {
            old.send.shutdown_detached();
        }
        let target = match crate::platforms::resolve_target(&self.stream_settings) {
            Ok(t) => t,
            Err(m) => {
                self.stream_lifecycle = StreamLifecycle::Error { message: m.clone() };
                self.emit_stream_state();
                self.release_output_engine_if_idle();
                return Err(ShellError::Stream(m));
            }
        };
        let full_url = target.full_url;
        let key = self.stream_settings.key.clone();
        let build = |s: &mut Shell| -> Result<(), String> {
            s.ensure_output_engine()?;
            let o = s.output.as_mut().expect("engine exists");
            let audio_params = Some(o.audio_enc.parameters());
            let st = StreamTarget::Rtmp(full_url.clone());
            let transport = RealTransport::new(st.clone(), o.video_enc.parameters(), audio_params);
            let streamer = Streamer::new(st, transport, policy);
            let send =
                crate::send_worker::SendWorker::spawn(full_url.clone(), key.clone(), streamer);
            o.stream = Some(StreamSink {
                send,
                rebase: Rebase::default(),
                pending: VecDeque::new(),
                newest_dts_us: 0,
                bytes_sent: 0,
                worker_status: crate::send_worker::WorkerStatus::Connecting,
            });
            Ok(())
        };
        match build(self) {
            Ok(()) => {
                self.stream_lifecycle = StreamLifecycle::Connecting;
                self.stream_started_at = Some(Instant::now());
                self.emit_stream_state();
                Ok(())
            }
            Err(m) => {
                self.release_output_engine_if_idle();
                let msg = sanitize_stream_error(&full_url, &key, "stream setup failed", &m);
                self.stream_lifecycle = StreamLifecycle::Error {
                    message: msg.clone(),
                };
                self.emit_stream_state();
                Err(ShellError::Stream(msg))
            }
        }
    }

    /// Stops streaming (any state → `Idle`). Never blocks the caller on
    /// the network: the worker closes the connection on its own thread.
    pub fn stream_stop(&mut self) -> Result<(), ShellError> {
        if let Some(sink) = self.output.as_mut().and_then(|o| o.stream.take()) {
            sink.send.shutdown_detached();
        }
        self.release_output_engine_if_idle();
        self.stream_lifecycle = StreamLifecycle::Idle;
        self.stream_started_at = None;
        self.emit_stream_state();
        Ok(())
    }

    /// Drains the RTMP send worker's events. Keeps the stream lifecycle
    /// truthful even when nothing is encoded. Public so hosts can poll it.
    pub fn stream_drain_worker(&mut self) {
        let mut rekey = false;
        let mut lifecycle_changed = false;
        {
            let Some(a) = self.output.as_mut().and_then(|o| o.stream.as_mut()) else {
                return;
            };
            let lifecycle = &mut self.stream_lifecycle;
            for ev in a.send.drain_events() {
                match ev {
                    crate::send_worker::SendEvent::Ack { dts_us, bytes_sent } => {
                        a.bytes_sent = bytes_sent;
                        while let Some(&(d, _)) = a.pending.front() {
                            if d <= dts_us {
                                a.pending.pop_front();
                            } else {
                                break;
                            }
                        }
                    }
                    crate::send_worker::SendEvent::Status(s) => {
                        a.worker_status = s.clone();
                        let next = match &s {
                            crate::send_worker::WorkerStatus::Connecting => {
                                StreamLifecycle::Connecting
                            }
                            crate::send_worker::WorkerStatus::Live => StreamLifecycle::Live,
                            crate::send_worker::WorkerStatus::Reconnecting => {
                                StreamLifecycle::Reconnecting
                            }
                            crate::send_worker::WorkerStatus::Failed(m) => {
                                StreamLifecycle::Error {
                                    message: m.clone(),
                                }
                            }
                        };
                        if *lifecycle != next {
                            *lifecycle = next;
                            lifecycle_changed = true;
                        }
                    }
                    crate::send_worker::SendEvent::Resync => {
                        // Fresh (re)connection: the worker dropped stale
                        // packets. Restart this sink at 0 on a new IDR.
                        a.pending.clear();
                        a.rebase.restart_at_zero();
                        rekey = true;
                    }
                    crate::send_worker::SendEvent::Stopped { bytes_sent } => {
                        a.bytes_sent = bytes_sent;
                    }
                }
            }
        }
        if rekey {
            if let Some(o) = self.output.as_mut() {
                o.video_enc.force_keyframe();
            }
        }
        if lifecycle_changed {
            self.emit_stream_state();
        }
    }

    /// Hands one packet to the send worker, applying the OBS time-based
    /// drop policy: past 900 ms of unsent video, non-keyframe video is
    /// shed (counted as a network drop). Audio and keyframes never are.
    fn stream_enqueue(a: &mut StreamSink, packet: EncodedPacket, dropped: &mut u64) {
        let kind = if packet.stream_index == 0 {
            if packet.is_keyframe {
                PacketKind::VideoKey
            } else {
                PacketKind::VideoDelta
            }
        } else {
            PacketKind::Audio
        };
        if kind != PacketKind::Audio {
            a.newest_dts_us = a.newest_dts_us.max(packet.dts_us);
        }
        if drop_decision(kind, buffered_video_us(&a.pending, packet.dts_us)) {
            *dropped += 1;
            return;
        }
        a.pending.push_back((packet.dts_us, kind));
        if a.send.try_send(packet).is_err() {
            a.pending.pop_back();
            *dropped += 1;
        }
    }

    /// Encodes the program frame and the tick's audio ONCE and fans the
    /// packets out to the recording and the stream.
    pub(crate) fn output_tick(&mut self, frame: &Frame, audio: &[f32], tick_us: i64) {
        self.stream_drain_worker();
        let frame_us = self.frame_interval_us() as i64;
        let record_active = self
            .record_runtime
            .as_ref()
            .map(|r| !r.paused)
            .unwrap_or(false);
        let Some(o) = self.output.as_mut() else {
            return;
        };
        let stream_live = o.stream.as_ref().is_some_and(|s| s.is_live());
        // Recording resume: continue the file's timeline on a fresh IDR.
        if let Some(rec) = o.record.as_mut() {
            if !record_active {
                rec.was_paused = true;
            } else if rec.was_paused {
                rec.was_paused = false;
                rec.rebase.restart_continuous();
                o.video_enc.force_keyframe();
            }
        }
        let wants_output = (o.record.is_some() && record_active) || stream_live;
        if !wants_output {
            // Nobody consumes packets this tick (stream still connecting,
            // recording paused): skip the encoder entirely.
            return;
        }
        let mut packets: Vec<EncodedPacket> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        match o.video_enc.encode_rgba(&frame.pixels, tick_us) {
            Ok(p) => {
                if p.is_empty() {
                    o.skipped_slots += 1;
                } else {
                    o.encoded_times.push_back(tick_us);
                }
                packets.extend(p);
            }
            Err(e) => errors.push(format!("encode video: {e}")),
        }
        while o
            .encoded_times
            .front()
            .is_some_and(|&t| tick_us - t > 1_000_000)
        {
            o.encoded_times.pop_front();
        }
        if !audio.is_empty() {
            let t = Instant::now();
            // The block covers the wall time just before this tick.
            let start = tick_us - (audio.len() / 2) as i64 * 1_000_000 / 48_000;
            match o.audio_enc.encode_pcm(audio, start) {
                Ok(p) => packets.extend(p),
                Err(e) => errors.push(format!("encode audio: {e}")),
            }
            o.last_audio_encode_us = t.elapsed().as_micros() as u64;
        }

        let mut record_failed: Option<String> = errors.first().cloned();
        if let Some(rec) = o.record.as_mut() {
            if record_active && record_failed.is_none() {
                for p in &packets {
                    let Some(p) = rec.rebase.map(p, frame_us) else {
                        continue;
                    };
                    let r = if p.stream_index == 0 {
                        rec.recorder.write_video(&p)
                    } else {
                        rec.recorder.write_audio(&p)
                    };
                    if let Err(e) = r {
                        record_failed = Some(format!("write: {e}"));
                        break;
                    }
                }
            }
        }
        if let Some(s) = o.stream.as_mut() {
            if stream_live {
                for p in &packets {
                    if let Some(p) = s.rebase.map(p, frame_us) {
                        Self::stream_enqueue(s, p, &mut self.perf_dropped_network);
                    }
                }
            }
        }
        if !errors.is_empty() {
            self.perf_skipped_render += 1;
        }
        if let Some(m) = record_failed.filter(|_| self.output.as_ref().is_some_and(|o| o.record.is_some()) && record_active) {
            // Fail the recording loudly; the stream (if any) keeps going.
            if let Some(o) = self.output.as_mut() {
                o.record = None;
            }
            self.release_output_engine_if_idle();
            self.record_runtime = None;
            self.record_error = Some(m);
            self.emit_record_state();
        }
    }

    // ------------------------------------------------------------------
    // Tick entry
    // ------------------------------------------------------------------

    /// One media tick at the current time. Hosts with a deadline clock
    /// should call [`Self::av_tick_at`] with the scheduled tick time.
    pub fn av_tick(&mut self) {
        self.av_tick_at(sf_capture::clock::now_us());
    }

    /// One media tick scheduled at `tick_us` (pipeline clock, see
    /// [`crate::media_clock_us`]): capture → render → audio → encode →
    /// send, plus the monitors. Never panics; backend failures degrade to
    /// lifecycle `Error` states.
    pub fn av_tick_at(&mut self, tick_us: i64) {
        let tick_start = Instant::now();
        let frame_us = self.frame_interval_us() as i64;
        // [1] capture
        let t = Instant::now();
        self.refresh_source_frames(tick_us);
        let capture_us = t.elapsed().as_micros() as u64;
        self.retire_stale_caches();

        // [2] render — full output size only when something encodes it.
        let encoding = self.output.is_some();
        let (w, h) = if encoding {
            (
                self.video_settings.output_width,
                self.video_settings.output_height,
            )
        } else {
            self.monitor_size
        };
        let t = Instant::now();
        self.perf_gpu_us_accum = 0;
        // Nothing encodes and nobody looks at the monitor (window hidden,
        // other page): don't render a picture no one will see. A UI request
        // renders on demand, and the next tick resumes rendering.
        let needed = encoding || self.monitor_watched() || self.transition.is_some() || self.studio_watch_active();
        let frame = if needed {
            self.program_frame_raw(w, h)
        } else {
            self.monitor_frame = None;
            self.monitor_pending = None;
            Err(ShellError::NoFrame("not rendered: no output and no viewer".into()))
        };
        let render_us = t.elapsed().as_micros() as u64;

        // [3] audio — exactly the wall time since the previous tick.
        let t = Instant::now();
        let first_block = (48_000 * frame_us / 1_000_000) as usize;
        let frames = self.audio_timeline.frames_until(tick_us, first_block);
        let audio = self.pump_audio(frames, tick_us);
        if let Some(monitor) = &self.audio_monitor {
            // Solo active → the monitor hears the solo bus, never the
            // program (which still goes to stream/record unchanged).
            let local = self.monitor_solo.as_deref().unwrap_or(&audio);
            // After a long stall play only the newest 100 ms locally.
            let keep = local.len().min(4_800 * 2);
            monitor.push(&local[local.len() - keep..]);
        }
        let audio_us = t.elapsed().as_micros() as u64;

        // [4] encode + send
        let t = Instant::now();
        match frame {
            Ok(frame) => {
                if encoding {
                    self.output_tick(&frame, &audio, tick_us);
                }
                let encode_us = t.elapsed().as_micros() as u64;
                self.set_monitor_frame(frame);
                // The UI shows one output (program). A separate preview
                // scene is only rendered when a host asks for it
                // (`monitor_snapshot`) — never every tick.
                self.perf_stages_us = [capture_us, render_us, audio_us, encode_us];
            }
            Err(_) if !needed => {
                self.perf_stages_us = [capture_us, 0, audio_us, 0];
            }
            Err(_) => {
                self.perf_skipped_render += 1;
                self.perf_stages_us = [capture_us, render_us, audio_us, 0];
            }
        }
        self.last_program_video_age_us = self.program_video_age_us();
        let tick_us_spent = tick_start.elapsed().as_micros() as u64;
        self.perf_mark_tick(tick_us_spent);
        self.perf_mark_frame();
    }

    /// Drop cached frames of sources that no longer exist.
    fn retire_stale_caches(&mut self) {
        if self.source_frames.len() > self.media_producers.len() {
            let live: std::collections::HashSet<&String> = self
                .scenes
                .iter()
                .flat_map(|s| s.sources.iter().map(|x| &x.id))
                .collect();
            self.source_frames.retain(|k, _| live.contains(k));
        }
        if !self.resized_frames.is_empty() {
            let frames = &self.source_frames;
            self.resized_frames
                .retain(|(id, _, _), (seq, _)| frames.get(id).is_some_and(|f| f.seq == *seq));
        }
    }
}

#[cfg(test)]
mod drop_policy_tests {
    use super::{buffered_video_us, drop_decision, PacketKind, Rebase};
    use sf_encode::EncodedPacket;
    use std::collections::VecDeque;

    #[test]
    fn buffered_span_ignores_audio() {
        let pending: VecDeque<(i64, PacketKind)> = vec![
            (1_000_000, PacketKind::Audio),
            (1_010_000, PacketKind::VideoKey),
            (1_020_000, PacketKind::Audio),
            (1_040_000, PacketKind::VideoDelta),
        ]
        .into_iter()
        .collect();
        assert_eq!(buffered_video_us(&pending, 1_100_000), 90_000);
    }

    #[test]
    fn buffered_span_empty_is_zero() {
        let pending: VecDeque<(i64, PacketKind)> = VecDeque::new();
        assert_eq!(buffered_video_us(&pending, 5_000_000), 0);
        let audio_only: VecDeque<(i64, PacketKind)> =
            vec![(1_000_000, PacketKind::Audio)].into_iter().collect();
        assert_eq!(buffered_video_us(&audio_only, 2_000_000), 0);
    }

    #[test]
    fn only_delta_video_is_shed() {
        assert!(!drop_decision(PacketKind::VideoDelta, 899_999));
        assert!(drop_decision(PacketKind::VideoDelta, 900_001));
        assert!(!drop_decision(PacketKind::VideoKey, 5_000_000));
        assert!(!drop_decision(PacketKind::Audio, 5_000_000));
    }

    fn pkt(stream: u8, pts: i64, key: bool) -> EncodedPacket {
        EncodedPacket {
            data: vec![1],
            pts_us: pts,
            dts_us: pts,
            is_keyframe: key,
            stream_index: stream,
        }
    }

    #[test]
    fn late_sink_starts_at_zero_on_keyframe_and_resumes_continuously() {
        let mut r = Rebase::default();
        // Joins mid-session: delta + audio before the IDR are skipped.
        assert!(r.map(&pkt(0, 10_000_000, false), 33_333).is_none());
        assert!(r.map(&pkt(1, 10_010_000, false), 33_333).is_none());
        let k = r.map(&pkt(0, 10_033_333, true), 33_333).unwrap();
        assert_eq!(k.pts_us, 0);
        let a = r.map(&pkt(1, 10_040_000, false), 33_333).unwrap();
        assert_eq!(a.pts_us, 6_667);
        // Audio from before the sink's first frame is dropped.
        assert!(r.map(&pkt(1, 10_000_000, false), 33_333).is_none());
        let d = r.map(&pkt(0, 10_066_666, false), 33_333).unwrap();
        assert_eq!(d.pts_us, 33_333);
        // Paused for 20 s, resumed on an IDR: file time continues.
        r.restart_continuous();
        let k2 = r.map(&pkt(0, 30_000_000, true), 33_333).unwrap();
        assert_eq!(k2.pts_us, 66_666);
        // New RTMP connection: starts over at 0.
        r.restart_at_zero();
        let k3 = r.map(&pkt(0, 40_000_000, true), 33_333).unwrap();
        assert_eq!(k3.pts_us, 0);
    }
}
