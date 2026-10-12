//! Real capture/encode/output/audio backend wiring for the desktop shell.
//!
//! This module owns everything that touches `sf-capture`, `sf-encode`,
//! `sf-output`, and `sf-audio`, keeping `production.rs` focused on the
//! state machines. All logic lives on [`Shell`](super::Shell)
//! (headless-testable, no tauri dependency).
//!
//! # What is real vs. honest stub here
//!
//! - REAL: media-file sources (decode via `MediaFileProducer`, frames into
//!   the composite, audio into the `sf-audio` mixer → live meters);
//!   recording the program bus to MP4/MKV via `sf-encode` + `sf-output`
//!   (driven by [`Shell::av_tick`], which the adapter calls on its fixed
//!   33.3 ms media tick — so the shell records at 30 fps);
//!   RTMP streaming attempts via `sf-output`'s `Streamer` + `RealTransport`
//!   (connects for real; fails cleanly with a redacted error when the
//!   server is unreachable; exponential-backoff reconnect is live).
//! - HONEST stubs: `Screen` (no capture hardware on a headless VM;
//!   `#[cfg(windows)]` scaffolds exist in `sf-capture`); `Camera` on
//!   non-Windows (on Windows, DirectShow capture via `sf-capture::dshow`
//!   is wired — hardware-unverified); hardware encoders (selection logic
//!   is real; NVENC/QSV/AMF only exist on Windows GPUs — here libx264 is
//!   used, verified by roundtrip tests).
//!
//! # Stream-key hygiene
//!
//! The key lives in `StreamSettings.key` (never serialized, redacted
//! `Debug`). Every error path that touches the RTMP URL goes through
//! [`sanitize_stream_error`], which redacts the key even if the underlying
//! FFmpeg error echoes the URL.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sf_audio::AudioFrame;
use sf_capture::{
    media::MediaFileProducer, CapturedFrame, CaptureError, FrameProducer, StreamInfo,
};
use sf_encode::{AudioEncoder, EncodedPacket, EncoderId, VideoEncoder, VideoEncoderOpts};

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
use sf_engine::Frame;
use sf_output::{
    redact_url, ContainerFormat, RealTransport, ReconnectPolicy, Recorder, StreamTarget, Streamer,
};

use super::production::{
    transition_progress, EnhancerView, RecordFormat, RecordRuntime, StreamLifecycle, VoiceStatsView,
};
use crate::production::SourceKind;
use super::{Shell, ShellError};
use crate::production::crossfade;

/// Legacy fixed program size / tick constants. The live values now come
/// from [`crate::production::VideoSettings`] (`video_settings` on the
/// shell); these remain for reference.
pub const AV_W: u32 = 640;
/// See [`AV_W`].
pub const AV_H: u32 = 360;
/// See [`AV_W`].
pub const AV_FRAME_US: i64 = 33_333;

/// Hard timeout for DirectShow device opens. FFmpeg's interrupt callback
/// is NOT consulted while DirectShow builds its COM filter graph, so a
/// wedged driver (or a device held exclusive by another app) can block
/// `avformat_open_input` forever — holding the shell Mutex and freezing
/// the whole app ("Not Responding"). Opening on a dedicated thread bounds
/// the damage: on timeout the thread is abandoned (it holds no shell
/// locks) and the user gets a clean error instead of a dead app.
const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(8);

/// Runs a device `open` on a dedicated thread with a hard timeout.
/// Also converts an FFmpeg panic into a clean error.
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
/// (`sf_audio::LiveInput`). The tick then only drains a FIFO, so a
/// blocking device read (dshow waits up to 25 ms per packet) can never
/// stall rendering/encoding.
#[cfg(windows)]
fn spawn_live_audio(
    source_id: &str,
    mut prod: sf_capture::audio_in::AudioInputProducer,
) -> Result<sf_audio::LiveInput, ShellError> {
    sf_audio::LiveInput::spawn(source_id, move || match prod.next_audio() {
        Ok(Some(c)) => Ok(Some(c.samples)),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    })
    .map_err(|e| ShellError::Capture(format!("cannot start audio capture thread: {e}")))
}

/// An active recording session: encoders + muxer + clocks.
pub(crate) struct ActiveRecording {
    video_enc: VideoEncoder,
    audio_enc: Option<AudioEncoder>,
    recorder: Recorder,
    /// Output path (kept for diagnostics / future "reveal in folder").
    #[allow(dead_code)]
    path: String,
    video_pts_us: i64,
    audio_pts_us: i64,
}

// SAFETY: the fields hold FFmpeg encoder/muxer contexts, which are raw
// pointers (`!Send`) only by conservatism — they carry no thread affinity,
// and every session is only ever touched through the shell's `Mutex`
// (commands and the poll loop lock it exclusively). Serialized use of
// FFmpeg contexts across threads is safe.
unsafe impl Send for ActiveRecording {}

/// An active streaming session: async RTMP send worker + encoders + clocks.
///
/// The [`SendWorker`](crate::send_worker::SendWorker) owns the RTMP
/// [`Streamer`] on its own thread (OBS's dedicated send thread): the media
/// tick encodes and hands packets over with non-blocking `try_send`, so a
/// stalled network can never stall rendering. The worker owns the
/// connect/reconnect backoff; the tick mirrors its status into
/// [`StreamLifecycle`] and applies the OBS time-based drop policy.
pub(crate) struct ActiveStream {
    send: crate::send_worker::SendWorker,
    video_enc: VideoEncoder,
    audio_enc: Option<AudioEncoder>,
    video_pts_us: i64,
    audio_pts_us: i64,
    /// `(dts_us, kind)` of packets handed to the send thread but not yet
    /// acked — drives the exact buffered-span computation for the drop
    /// policy. FIFO: the worker acks in send order.
    pending: VecDeque<(i64, PacketKind)>,
    /// Last acked byte total (drives the kbps meter).
    bytes_sent: u64,
    /// Worker's last reported status.
    worker_status: crate::send_worker::WorkerStatus,
}

/// Packet class for the drop policy: video delta frames may be shed under
/// congestion; audio and keyframes never are (OBS `drop_frames`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PacketKind {
    VideoKey,
    VideoDelta,
    Audio,
}

impl ActiveStream {
    pub(crate) fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }
    pub(crate) fn encoder_name(&self) -> &'static str {
        self.video_enc.encoder_name()
    }

    /// Buffered video span in microseconds: newest dts handed to the worker
    /// minus the oldest unacked video dts. Mirrors OBS's
    /// `last_dts_usec - first_video.dts_usec` buffer-duration metric.
    fn buffered_video_us(&self, newest_dts: i64) -> i64 {
        buffered_video_us(&self.pending, newest_dts)
    }
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

// SAFETY: same argument as `ActiveRecording` — FFmpeg contexts with no
// thread affinity, all access serialized by the shell's `Mutex`.
unsafe impl Send for ActiveStream {}

/// Producers hold FFmpeg decoder/scaler contexts, which are raw pointers
/// and therefore `!Send`. Every producer is only ever touched through the
/// shell's `Mutex` (Tauri commands and the poll loop lock it exclusively),
/// and FFmpeg contexts are safe to move between threads as long as
/// concurrent use is serialized — so this wrapper is sound.
pub(crate) enum ProducerKind {
    Media(MediaFileProducer),
    #[cfg(windows)]
    Camera(sf_capture::dshow::DshowProducer),
    #[cfg(windows)]
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
            #[cfg(windows)]
            ProducerKind::Camera(p) => p.next_frame(),
            #[cfg(windows)]
            ProducerKind::Screen(p) => p.next_frame(),
        }
    }

    fn stream_info(&self) -> StreamInfo {
        match self {
            ProducerKind::Media(p) => p.stream_info(),
            #[cfg(windows)]
            ProducerKind::Camera(p) => p.stream_info(),
            #[cfg(windows)]
            ProducerKind::Screen(p) => p.stream_info(),
        }
    }
}

impl ProducerKind {
    /// Next normalized audio chunk, if the producer has audio.
    /// Cameras opened with an audio device (`open_with_audio`) deliver
    /// it here; screen capture has no audio.
    fn next_audio(&mut self) -> Result<Option<sf_capture::media::DecodedAudio>, CaptureError> {
        match self {
            ProducerKind::Media(p) => p.next_audio(),
            #[cfg(windows)]
            ProducerKind::Camera(p) => p.next_audio(),
            #[cfg(windows)]
            ProducerKind::Screen(_) => Ok(None),
        }
    }
}

/// Nearest-neighbor RGBA resize (shell-grade; production will use the GPU).
pub(crate) fn resize_rgba_nearest(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw.max(1), sh.max(1), dw.max(1), dh.max(1));
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
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
    /// Called from `source_add` before the source is pushed, so a missing
    /// camera fails cleanly.
    #[cfg(windows)]
    pub(crate) fn open_camera_producer(
        &mut self,
        source_id: &str,
        device: Option<&str>,
        audio_device: Option<&str>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), ShellError> {
        // Timeout-guarded (see `open_device`): a wedged camera driver
        // must never freeze the app.
        //
        // Video and audio open as SEPARATE dshow graphs (like OBS's
        // separate video/audio sources). A combined `video=X:audio=Y`
        // context lets the audio packet flood starve video packets in the
        // drain loop — the capture delay then grows without bound (the
        // "one minute late" bug).
        let device_owned = device.map(|d| d.to_string());
        let audio_owned = audio_device.map(|d| d.to_string());
        let prod: sf_capture::dshow::DshowProducer = open_device("camera", move || {
            match device_owned.as_deref() {
                // Video-only; audio (if any) gets its own graph below.
                Some(d) => sf_capture::dshow::DshowProducer::open(d, width, height, fps),
                None => sf_capture::dshow::DshowProducer::open_default(width, height, fps),
            }
        })?;
        // Remember what the camera actually negotiated (codec/res/fps) for
        // diagnostics — shown in the UI, not just what was requested.
        self.camera_formats
            .insert(source_id.to_string(), prod.negotiated_format().to_string());
        self.media_producers.insert(
            source_id.to_string(),
            SendProducer(ProducerKind::Camera(prod)),
        );
        // Separate audio graph (OBS parity): never share the video's dshow
        // context.
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

    /// Opens a desktop screen producer (Windows gdigrab). Called from
    /// `source_add` before the source is pushed, so a failed grab fails
    /// cleanly.
    #[cfg(windows)]
    pub(crate) fn open_screen_producer(&mut self, source_id: &str) -> Result<(), ShellError> {
        // Timeout-guarded like the other device opens (see `open_device`).
        let prod: sf_capture::gdigrab::ScreenProducer =
            open_device("screen capture", || sf_capture::gdigrab::ScreenProducer::open(30))?;
        self.media_producers.insert(
            source_id.to_string(),
            SendProducer(ProducerKind::Screen(prod)),
        );
        Ok(())
    }

    /// Negotiated camera format for a source, e.g.
    /// `"mjpeg 1920x1080 @30fps (via mjpeg)"`. `None` for non-camera
    /// sources or when the camera was never opened.
    pub fn camera_format(&self, source_id: &str) -> Option<String> {
        self.camera_formats.get(source_id).cloned()
    }

    /// Audio device label for a source: the mic name for audio inputs,
    /// the camera's audio device when set. `None` for sources without an
    /// audio device (media files, silent sources).
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
    /// Windows only). Called from `source_add` before the source is
    /// pushed, so a missing device fails cleanly.
    ///
    /// The open runs on a timeout-guarded thread (see `open_device`): a
    /// wedged audio driver must never freeze the app.
    #[cfg(windows)]
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

    /// Pulls the next decoded frame for a media source, resized to the
    /// composite size. Returns `None` when no frame is ready; freezes on
    /// the last frame at end-of-stream (documented behavior).
    pub(crate) fn pull_media_frame(&mut self, source_id: &str, w: u32, h: u32) -> Option<Vec<u8>> {
        let frame = {
            let prod = self.media_producers.get_mut(source_id)?;
            match prod.next_frame() {
                Ok(Some(f)) => Some(f),
                Ok(None) => None,
                Err(CaptureError::EndOfStream) => {
                    // Freeze on the last frame; stop polling the demuxer.
                    self.media_producers.remove(source_id);
                    None
                }
                Err(_) => return None,
            }
        };
        match frame {
            Some(f) => {
                let px = if f.width == w && f.height == h {
                    f.data
                } else {
                    // Hybrid: GPU resize first, fall back to CPU on error.
                    match self.engine.gpu_resize_rgba(&f.data, f.width, f.height, w, h) {
                        Ok(px) => px,
                        Err(_) => resize_rgba_nearest(&f.data, f.width, f.height, w, h),
                    }
                };
                self.media_last_frame
                    .insert(source_id.to_string(), (w, h, px.clone()));
                Some(px)
            }
            None => self
                .media_last_frame
                .get(source_id)
                .map(|(_, _, px)| px.clone()),
        }
    }

    /// Pause/resume a media source's playback. While paused the video
    /// freezes on the last frame and its audio goes silent. Returns the
    /// new paused state.
    pub fn media_set_paused(&mut self, source_id: &str, paused: bool) -> Result<bool, ShellError> {
        let prod = self
            .media_producers
            .get_mut(source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
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
    /// Opening is lazy and fallible: no output device → clean error,
    /// never a startup crash.
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
    /// Takes effect on the very next audio block (≤ one 33 ms tick) for
    /// every live voice source — this is what makes slider moves audible
    /// in real time.
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
                    },
                )
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Audio: pull + mix + meters
    // ------------------------------------------------------------------

    /// Runs one audio tick and returns exactly one tick's worth of mixed
    /// 48 kHz stereo samples (`frames_per_tick * 2`, silence when nothing
    /// is playing). This is the single place meters become real.
    ///
    /// Pipeline per tick:
    /// 1. The bus clock says how many sample frames this tick covers
    ///    (exact, rational fps — 1600 at 30 fps).
    /// 2. Live inputs: drain their capture threads' FIFOs into the bus
    ///    (never blocks — device reads happen on their own threads).
    /// 3. Media files: pull decoded audio only until the bus holds one
    ///    block + prebuffer (paced by the audio clock — files play at the
    ///    right speed instead of being decoded ahead and dropped).
    /// 4. Pull exactly one block per source; voice sources go through
    ///    their own enhancer (RNNoise AI denoise → gate → EQ → compressor)
    ///    PRE-fader; then the mixer sums equal-length blocks.
    ///
    /// Because the block length is fixed per tick, the encoder's audio
    /// timeline advances exactly like video PTS — no A/V drift — and
    /// ticks with no new audio still produce (silent) samples.
    pub(crate) fn pump_audio(&mut self) -> Vec<f32> {
        self.audio_bus
            .set_fps(self.video_settings.output_fps.max(1), 1);
        let frames = self.audio_bus.next_block_frames();

        // Sources that actually carry audio.
        // (A camera's audio arrives through `audio_inputs` under the same
        // id; never count it twice.)
        let media_ids: Vec<String> = self
            .media_producers
            .iter()
            .filter(|(id, p)| p.stream_info().has_audio && !self.audio_inputs.contains_key(*id))
            .map(|(id, _)| id.clone())
            .collect();
        let live_ids: Vec<String> = self.audio_inputs.keys().cloned().collect();
        let ids: Vec<String> = media_ids.iter().chain(live_ids.iter()).cloned().collect();

        // Sync mixer registration + strips with the current sources.
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

        if ids.is_empty() {
            // Keep the audio timeline continuous for the encoders.
            return vec![0.0; frames * 2];
        }

        // Live inputs → bus (non-blocking drain of the capture threads).
        let mut tmp: Vec<f32> = Vec::new();
        for (id, input) in &self.audio_inputs {
            tmp.clear();
            input.drain_into(&mut tmp);
            if !tmp.is_empty() {
                self.audio_bus.push(id, &tmp);
            }
        }
        // Media files → bus, paced by the audio clock.
        let target = self.audio_bus.refill_target_frames(frames);
        for id in &media_ids {
            let Some(prod) = self.media_producers.get_mut(id) else {
                continue;
            };
            let mut guard = 0;
            while self.audio_bus.buffered_frames(id) < target && guard < 16 {
                guard += 1;
                match prod.next_audio() {
                    Ok(Some(c)) if c.channels == 2 && c.sample_rate == 48_000 => {
                        self.audio_bus.push(id, &c.samples)
                    }
                    Ok(Some(_)) => {} // off-contract chunk: skip (capture normalizes)
                    _ => break,
                }
            }
        }

        // Exactly one block per source; voice sources get enhanced.
        let mut blocks: Vec<(String, Vec<f32>)> = Vec::with_capacity(ids.len());
        for id in &ids {
            let mut block = Vec::with_capacity(frames * 2);
            self.audio_bus.pull(id, frames, &mut block);
            if live_ids.contains(id) {
                let settings = &self.enhancer_settings;
                let enh = self.voice_enhancers.entry(id.clone()).or_insert_with(|| {
                    let mut e = sf_audio::Enhancer::new(48_000);
                    e.set_settings(settings.clone());
                    e
                });
                enh.process(&mut block);
            }
            blocks.push((id.clone(), block));
        }
        let frames_in: Vec<(&str, AudioFrame)> = blocks
            .iter()
            .map(|(id, b)| (id.as_str(), AudioFrame::new(b, 2, 48_000)))
            .collect();
        let mixed = self.audio_mixer.mix(&frames_in);
        for m in &mixed.per_source {
            self.meter_values.insert(m.id.clone(), m.peak);
        }
        self.meters_live = true;
        mixed.samples
    }

    // ------------------------------------------------------------------
    // Program frame (raw, for encode)
    // ------------------------------------------------------------------

    /// Raw RGBA program frame: live crossfade during an auto transition,
    /// otherwise the program scene composite. Mirrors `program_frame_png`.
    /// Renders the preview scene at monitor size and caches it for the UI
    /// monitors. Called by the media tick every tick so the preview
    /// monitor stays as fresh as the program monitor without re-rendering
    /// (and re-pulling cameras) on every UI poll — a major source of
    /// perceived capture latency.
    fn refresh_preview_monitor(&mut self) {
        let preview_id = self.preview_scene_id.clone();
        if preview_id == self.program_scene_id || preview_id.is_empty() {
            return;
        }
        if let Ok(f) =
            self.render_scene_frame(&preview_id, crate::MONITOR_W, crate::MONITOR_H)
        {
            self.set_preview_monitor_frame(f);
        }
    }

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
            )
        });
        match active {
            Some((from, to, progress)) => {
                let a = self.render_scene_frame(&from, width, height)?;
                let b = self.render_scene_frame(&to, width, height)?;
                Ok(crossfade(&a, &b, progress))
            }
            None => {
                let program = self.program_scene_id.clone();
                self.render_scene_frame(&program, width, height)
            }
        }
    }

    // ------------------------------------------------------------------
    // Recording
    // ------------------------------------------------------------------

    fn video_encoder_opts(&self) -> VideoEncoderOpts {
        let vs = &self.video_settings;
        VideoEncoderOpts {
            width: vs.output_width,
            height: vs.output_height,
            fps_num: vs.output_fps,
            fps_den: 1,
            bitrate_bps: vs.bitrate_kbps * 1000,
            gop: vs.output_fps * 2,
            preset: "veryfast".to_string(),
        }
    }

    /// Opens the video encoder honoring the Video panel's encoder override:
    /// `"auto"` probes the registry (NVENC → QSV → AMF → x264 → mpeg4),
    /// anything else forces that encoder (falls back to auto on failure).
    fn open_video_encoder(&self, opts: VideoEncoderOpts) -> Result<VideoEncoder, ShellError> {
        let name = self.video_settings.encoder.as_str();
        if name != "auto" {
            if let Some(id) = encoder_id_from_name(name) {
                match VideoEncoder::open_with_encoder(id, opts.clone()) {
                    Ok(enc) => return Ok(enc),
                    Err(e) => {
                        // Forced encoder failed (e.g. NVENC without NVIDIA
                        // GPU) — fall through to auto so start still works.
                        eprintln!("video: forced encoder {name} failed ({e}); falling back to auto");
                    }
                }
            }
        }
        VideoEncoder::open(opts).map_err(|e| ShellError::Capture(format!("video encoder: {e}")))
    }

    /// Rebuilds the live video encoders (stream and/or record) with the
    /// current output settings — used for real-time FPS changes mid
    /// session. Each encoder is replaced only after its successor opens,
    /// so a failure leaves the working encoder untouched. The first packet
    /// out of a fresh encoder is a keyframe, so decoders resync
    /// immediately; PTS stays continuous via the running `video_pts_us`
    /// clock (`VideoEncoder::encode_rgba` anchors to it).
    pub(crate) fn reinit_video_encoders(&mut self) -> Result<(), ShellError> {
        if self.active_stream.is_some() {
            let enc = self.open_video_encoder(self.video_encoder_opts())?;
            if let Some(a) = self.active_stream.as_mut() {
                a.video_enc = enc;
            }
        }
        if self.active_recording.is_some() {
            let enc = self.open_video_encoder(self.video_encoder_opts())?;
            if let Some(rec) = self.active_recording.as_mut() {
                rec.video_enc = enc;
            }
        }
        Ok(())
    }

    /// Starts recording the program bus to `record_settings.path`.
    /// Real backend: libx264 (or the selected system encoder) + AAC into
    /// fragmented MP4 / MKV. Failures move the lifecycle to `Error`.
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
        let build = || -> Result<ActiveRecording, String> {
            let video_enc = self
                .open_video_encoder(self.video_encoder_opts())
                .map_err(|e| e.to_string())?;
            let mut recorder =
                Recorder::create(&path, fmt).map_err(|e| format!("recorder: {e}"))?;
            recorder
                .add_video_stream(&video_enc.parameters())
                .map_err(|e| format!("recorder: {e}"))?;
            // Always record an audio track (silent until a source plays):
            // a mic added mid-recording must still be heard, and the
            // audio timeline stays locked to video from frame 0.
            let audio_enc = {
                let enc = AudioEncoder::open().map_err(|e| format!("audio encoder: {e}"))?;
                recorder
                    .add_audio_stream(&enc.parameters(), enc.sample_rate())
                    .map_err(|e| format!("recorder: {e}"))?;
                Some(enc)
            };
            Ok(ActiveRecording {
                video_enc,
                audio_enc,
                recorder,
                path: path.clone(),
                video_pts_us: 0,
                audio_pts_us: 0,
            })
        };
        match build() {
            Ok(rec) => {
                self.active_recording = Some(rec);
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
                self.active_recording = None;
                self.record_runtime = None;
                self.record_error = Some(m.clone());
                self.emit_record_state();
                Err(ShellError::Record(m))
            }
        }
    }

    /// Stops recording: flushes encoders, finalizes the file, back to Idle.
    pub fn record_stop(&mut self) -> Result<(), ShellError> {
        if let Some(mut rec) = self.active_recording.take() {
            let mut failed: Option<String> = None;
            let mut drain =
                |enc: &mut dyn FnMut() -> Result<Vec<EncodedPacket>, String>,
                 write: &mut dyn FnMut(&EncodedPacket) -> Result<(), String>| {
                    if failed.is_some() {
                        return;
                    }
                    match enc() {
                        Ok(pkts) => {
                            for p in &pkts {
                                if let Err(e) = write(p) {
                                    failed = Some(e);
                                    break;
                                }
                            }
                        }
                        Err(e) => failed = Some(e),
                    }
                };
            let mut v = rec.video_enc;
            drain(
                &mut || v.flush().map_err(|e| format!("video flush: {e}")),
                &mut |p| {
                    rec.recorder
                        .write_video(p)
                        .map_err(|e| format!("write video: {e}"))
                },
            );
            if let Some(mut a) = rec.audio_enc.take() {
                drain(
                    &mut || a.flush().map_err(|e| format!("audio flush: {e}")),
                    &mut |p| {
                        rec.recorder
                            .write_audio(p)
                            .map_err(|e| format!("write audio: {e}"))
                    },
                );
            }
            if failed.is_none() {
                if let Err(e) = rec.recorder.finish() {
                    failed = Some(format!("finalize: {e}"));
                }
            }
            // `finish` consumes the recorder; on earlier failure the file
            // is left fragmented-but-present (crash-safe by design).
            if let Some(m) = failed {
                self.record_error = Some(m);
            }
        }
        self.record_runtime = None;
        self.emit_record_state();
        Ok(())
    }

    /// One recording step, called from [`Shell::av_tick`]: encodes the
    /// given program frame and the mixed audio, and writes. Errors fail
    /// the recording loudly (lifecycle `Error`) rather than dropping
    /// silently.
    pub(crate) fn record_tick(&mut self, frame: &Frame, audio: &[f32]) {
        let frame_us = self.frame_interval_us();
        let rec = match self.active_recording.as_mut() {
            Some(r) => r,
            None => return,
        };
        if self
            .record_runtime
            .as_ref()
            .map(|r| r.paused)
            .unwrap_or(true)
        {
            return;
        }
        let mut failed: Option<String> = None;
        let vpts = rec.video_pts_us;
        rec.video_pts_us += frame_us as i64;
        match rec.video_enc.encode_rgba(&frame.pixels, vpts) {
            Ok(pkts) => {
                for p in &pkts {
                    if let Err(e) = rec.recorder.write_video(p) {
                        failed = Some(format!("write video: {e}"));
                        break;
                    }
                }
            }
            Err(e) => failed = Some(format!("encode video: {e}")),
        }
        if failed.is_none() && !audio.is_empty() {
            if let Some(aenc) = rec.audio_enc.as_mut() {
                let apts = rec.audio_pts_us;
                rec.audio_pts_us += audio.len() as i64 / 2 * 1_000_000 / 48_000;
                match aenc.encode_pcm(&audio, apts) {
                    Ok(pkts) => {
                        for p in &pkts {
                            if let Err(e) = rec.recorder.write_audio(p) {
                                failed = Some(format!("write audio: {e}"));
                                break;
                            }
                        }
                    }
                    Err(e) => failed = Some(format!("encode audio: {e}")),
                }
            }
        }
        if let Some(m) = failed {
            // Fail loudly: drop the session, surface the error.
            // The frame never made it through encode/write: count it as
            // skipped on the render side (not a network drop).
            self.perf_skipped_render += 1;
            self.active_recording = None;
            self.record_runtime = None;
            self.record_error = Some(m);
            self.emit_record_state();
        }
    }

    // ------------------------------------------------------------------
    // Streaming
    // ------------------------------------------------------------------

    /// Starts streaming: builds encoders and spawns the async RTMP send
    /// worker, which connects on its own thread (the media tick never blocks
    /// on TCP connect). Lifecycle goes `Connecting` → `Live`, or → `Error`
    /// with a redacted message if the server is unreachable; the key never
    /// appears in any message.
    pub fn stream_start(&mut self) -> Result<(), ShellError> {
        self.stream_start_with_policy(ReconnectPolicy::default())
    }

    /// [`stream_start`](Self::stream_start) with an explicit reconnect policy.
    /// Public so embedders (and tests) can tune or fast-fail the backoff.
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
        let url = self.stream_settings.url.trim_end_matches('/').to_string();
        let key = self.stream_settings.key.clone();
        if url.is_empty() {
            let m = "no stream URL set".to_string();
            self.stream_lifecycle = StreamLifecycle::Error { message: m.clone() };
            self.emit_stream_state();
            return Err(ShellError::Stream(m));
        }
        let full_url = if key.is_empty() {
            url.clone()
        } else {
            format!("{url}/{key}")
        };
        let build = || -> Result<ActiveStream, String> {
            let video_enc = self
                .open_video_encoder(self.video_encoder_opts())
                .map_err(|e| e.to_string())?;
            // Always stream an audio track: YouTube/Facebook/TikTok ingest
            // expect one, and a mic added mid-stream can only be heard if
            // the track was declared at connect time.
            let audio_enc = Some(AudioEncoder::open().map_err(|e| format!("audio encoder: {e}"))?);
            let audio_params = audio_enc.as_ref().map(|a| a.parameters());
            let target = StreamTarget::Rtmp(full_url.clone());
            let transport =
                RealTransport::new(target.clone(), video_enc.parameters(), audio_params);
            let streamer = Streamer::new(target, transport, policy);
            // The worker connects asynchronously; the tick never stalls on
            // the network from here on.
            let send = crate::send_worker::SendWorker::spawn(full_url.clone(), key.clone(), streamer);
            Ok(ActiveStream {
                send,
                video_enc,
                audio_enc,
                video_pts_us: 0,
                audio_pts_us: 0,
                pending: VecDeque::new(),
                bytes_sent: 0,
                worker_status: crate::send_worker::WorkerStatus::Connecting,
            })
        };
        match build() {
            Ok(active) => {
                self.stream_lifecycle = StreamLifecycle::Connecting;
                self.active_stream = Some(active);
                self.emit_stream_state();
                Ok(())
            }
            Err(m) => {
                let msg = sanitize_stream_error(&full_url, &key, "stream setup failed", &m);
                self.stream_lifecycle = StreamLifecycle::Error {
                    message: msg.clone(),
                };
                self.emit_stream_state();
                Err(ShellError::Stream(msg))
            }
        }
    }

    /// Stops streaming (any state → `Idle`), asking the send worker to shut
    /// down and close the transport.
    pub fn stream_stop(&mut self) -> Result<(), ShellError> {
        if let Some(active) = self.active_stream.take() {
            // Bounded wait: the worker exits promptly on Shutdown.
            let _bytes = active.send.shutdown();
        }
        self.stream_lifecycle = StreamLifecycle::Idle;
        self.emit_stream_state();
        Ok(())
    }

    /// Drains the RTMP send worker's events without encoding a frame.
    /// Keeps the stream lifecycle truthful even when the render stage has
    /// nothing to send. Public so hosts can poll it from their own loop.
    pub fn stream_drain_worker(&mut self) {
        if self.active_stream.is_none() {
            return;
        }
        let mut rekey = false;
        let mut lifecycle_changed = false;
        {
            let a = self.active_stream.as_mut().expect("checked above");
            let lifecycle = &mut self.stream_lifecycle;
            for ev in a.send.drain_events() {
                match ev {
                    crate::send_worker::SendEvent::Ack { dts_us, bytes_sent } => {
                        a.bytes_sent = bytes_sent;
                        // FIFO: the worker acks in send order.
                        while let Some(&(d, _)) = a.pending.front() {
                            if d <= dts_us {
                                a.pending.pop_front();
                            } else {
                                break;
                            }
                        }
                    }
                    crate::send_worker::SendEvent::Status(s) => {
                        let became_live = s == crate::send_worker::WorkerStatus::Live
                            && a.worker_status != crate::send_worker::WorkerStatus::Live;
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
                        if became_live {
                            rekey = true;
                        }
                    }
                    crate::send_worker::SendEvent::Resync => {
                        // Worker dropped stale packets on (re)connect.
                        a.pending.clear();
                        rekey = true;
                    }
                    crate::send_worker::SendEvent::Stopped { bytes_sent } => {
                        a.bytes_sent = bytes_sent;
                    }
                }
            }
        }
        if rekey {
            // Fresh connection: restart the video encoder so the first
            // packet is a keyframe and decoders resync immediately. PTS
            // stays continuous via the running `video_pts_us` clock.
            if let Ok(enc) = self.open_video_encoder(self.video_encoder_opts()) {
                if let Some(a) = self.active_stream.as_mut() {
                    a.video_enc = enc;
                }
            }
        }
        if lifecycle_changed {
            self.emit_stream_state();
        }
    }

    /// Hands one encoded packet to the send worker, applying the OBS
    /// time-based drop policy: when the worker's buffered video span exceeds
    /// 900 ms, non-keyframe video is shed (counted as a network drop). Audio
    /// and keyframes are never dropped.
    fn stream_enqueue(a: &mut ActiveStream, packet: EncodedPacket, dropped: &mut u64) {
        let kind = if packet.stream_index == 0 {
            if packet.is_keyframe {
                PacketKind::VideoKey
            } else {
                PacketKind::VideoDelta
            }
        } else {
            PacketKind::Audio
        };
        if kind == PacketKind::VideoDelta
            && drop_decision(kind, a.buffered_video_us(packet.dts_us))
        {
            *dropped += 1;
            return;
        }
        a.pending.push_back((packet.dts_us, kind));
        if let Err(e) = a.send.try_send(packet) {
            // Backstop: the worker is wedged (the drop policy above should
            // have shed load long before the 256-packet queue fills).
            a.pending.pop_back();
            *dropped += 1;
            let _ = e;
        }
    }

    /// One streaming step, called from [`Shell::av_tick`]: drains worker
    /// events, encodes the given program frame and mixed audio, and hands
    /// the packets to the send thread with a non-blocking `try_send`.
    /// The network can never stall this tick.
    pub(crate) fn stream_tick(&mut self, frame: &Frame, audio: &[f32]) {
        if self.active_stream.is_none() {
            return;
        }
        self.stream_drain_worker();
        // Only encode when the worker is actually live: packets handed over
        // while connecting/reconnecting/failed would be dropped by the
        // worker (and never acked, which would poison the pending-packet
        // accounting). Skipping encode here also saves CPU during outages.
        let live = self
            .active_stream
            .as_ref()
            .map(|a| a.worker_status == crate::send_worker::WorkerStatus::Live)
            .unwrap_or(false);
        if !live {
            return;
        }
        let frame_us = self.frame_interval_us();
        // Encode video (stays on the tick, like OBS's video-io thread —
        // only the network send moved off-thread).
        let vpackets: Vec<EncodedPacket>;
        {
            let a = self.active_stream.as_mut().expect("checked above");
            let vpts = a.video_pts_us;
            a.video_pts_us += frame_us as i64;
            match a.video_enc.encode_rgba(&frame.pixels, vpts) {
                Ok(pkts) => vpackets = pkts,
                Err(e) => {
                    self.perf_skipped_render += 1;
                    let _ = e;
                    vpackets = Vec::new();
                }
            }
        }
        for p in vpackets {
            let a = self.active_stream.as_mut().expect("checked above");
            Self::stream_enqueue(a, p, &mut self.perf_dropped_network);
        }
        // Encode audio.
        if !audio.is_empty() {
            let apackets: Vec<EncodedPacket>;
            {
                let a = self.active_stream.as_mut().expect("checked above");
                if let Some(aenc) = a.audio_enc.as_mut() {
                    let apts = a.audio_pts_us;
                    a.audio_pts_us += audio.len() as i64 / 2 * 1_000_000 / 48_000;
                    match aenc.encode_pcm(&audio, apts) {
                        Ok(pkts) => apackets = pkts,
                        Err(e) => {
                            self.perf_skipped_render += 1;
                            let _ = e;
                            apackets = Vec::new();
                        }
                    }
                } else {
                    apackets = Vec::new();
                }
            }
            for p in apackets {
                let a = self.active_stream.as_mut().expect("checked above");
                Self::stream_enqueue(a, p, &mut self.perf_dropped_network);
            }
        }
    }

    // ------------------------------------------------------------------
    // Poll entry
    // ------------------------------------------------------------------

    /// Called by the adapter's fixed 33.3 ms media tick (after `transition_tick` runs
    /// on the separate 66 ms UI poll loop):
    /// renders one program frame, pumps audio (meters + record/stream
    /// feed), and advances recording and streaming. Never panics;
    /// backend failures degrade to lifecycle `Error` states.
    pub fn av_tick(&mut self) {
        let tick_start = std::time::Instant::now();
        let recording = self.active_recording.is_some();
        let streaming = self.active_stream.is_some();
        if !recording && !streaming {
            // Audio metering runs even when idle so the UI meters stay live.
            let t = std::time::Instant::now();
            let audio = self.pump_audio();
            // Local monitoring must work before going live too (mic check).
            if let Some(monitor) = &self.audio_monitor {
                monitor.push(&audio);
            }
            // The program monitor must stay live even when idle: render the
            // program scene small directly on this tick. (When recording or
            // streaming the full-res frame below is downscaled instead, so
            // the camera is only ever pulled once per tick.)
            if let Ok(f) = self.program_frame_raw(
                crate::MONITOR_W,
                crate::MONITOR_H,
            ) {
                self.set_monitor_frame(f);
            }
            // Keep the preview-scene monitor as fresh as the program one.
            self.refresh_preview_monitor();
            self.perf_stages_us = [0, t.elapsed().as_micros() as u64, 0, 0];
            // Track perf even when idle so the panel shows live values.
            let tick_us = tick_start.elapsed().as_micros() as u64;
            self.perf_mark_tick(tick_us);
            self.perf_mark_frame();
            return;
        }
        let t0 = std::time::Instant::now();
        let (ow, oh) = (
            self.video_settings.output_width,
            self.video_settings.output_height,
        );
        let frame = match self.program_frame_raw(ow, oh) {
            Ok(f) => f,
            Err(_) => {
                // Render stage couldn't produce a frame this tick: count it
                // as skipped (render-side, not network).
                self.perf_skipped_render += 1;
                return;
            }
        };
        let render_us = t0.elapsed().as_micros() as u64;
        // Audio metering runs on every tick; the same mixed block feeds
        // the record/stream encoders below.
        let t1 = std::time::Instant::now();
        let audio = self.pump_audio();
        let audio_us = t1.elapsed().as_micros() as u64;
        // Local speaker monitoring (when enabled): the same mixed block.
        if let Some(monitor) = &self.audio_monitor {
            monitor.push(&audio);
        }
        let t2 = std::time::Instant::now();
        if recording {
            self.record_tick(&frame, &audio);
        }
        let record_us = t2.elapsed().as_micros() as u64;
        let t3 = std::time::Instant::now();
        if streaming {
            self.stream_tick(&frame, &audio);
        }
        let stream_us = t3.elapsed().as_micros() as u64;
        self.perf_stages_us = [render_us, audio_us, record_us, stream_us];
        // Refresh the live program monitor from the frame this tick already
        // rendered (downscaled) — the UI serves this instead of re-rendering.
        self.set_monitor_frame(frame);
        // Same for the preview-scene monitor.
        self.refresh_preview_monitor();
        let tick_us = tick_start.elapsed().as_micros() as u64;
        self.perf_mark_tick(tick_us);
        self.perf_mark_frame();
    }
}

#[cfg(test)]
mod drop_policy_tests {
    use super::{buffered_video_us, drop_decision, PacketKind};
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
        // Newest video dts 1_100_000, oldest unacked video 1_010_000.
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
        // Under the 900ms threshold: nothing drops.
        assert!(!drop_decision(PacketKind::VideoDelta, 899_999));
        // Past it: delta video sheds...
        assert!(drop_decision(PacketKind::VideoDelta, 900_001));
        // ...but keyframes and audio never do.
        assert!(!drop_decision(PacketKind::VideoKey, 5_000_000));
        assert!(!drop_decision(PacketKind::Audio, 5_000_000));
    }
}
