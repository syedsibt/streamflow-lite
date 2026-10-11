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
//!   (driven by [`Shell::av_tick`], which the adapter calls on its 66 ms
//!   poll loop — so the shell records at ~15 fps; production will move to
//!   a dedicated thread at 1080p60);
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

use std::time::{Duration, Instant};

use sf_audio::AudioFrame;
use sf_capture::{
    media::MediaFileProducer, CapturedFrame, CaptureError, FrameProducer, StreamInfo,
};
use sf_encode::{AudioEncoder, EncodedPacket, VideoEncoder, VideoEncoderOpts};
use sf_engine::Frame;
use sf_output::{
    redact_url, ContainerFormat, RealTransport, ReconnectPolicy, Recorder,
    StreamState as OutputStreamState, StreamTarget, Streamer,
};

use super::production::{transition_progress, RecordFormat, RecordRuntime, StreamLifecycle};
use super::{Shell, ShellError};
use crate::production::crossfade;

/// Program-bus capture size for recording/streaming in the shell.
/// 640×360 keeps libx264 ultrafast inside the 66 ms poll budget on modest
/// CPUs; production will capture at the full program size on a thread.
pub const AV_W: u32 = 640;
/// See [`AV_W`].
pub const AV_H: u32 = 360;
/// Microseconds per output frame at the ~15 fps poll cadence.
pub const AV_FRAME_US: i64 = 66_667;
/// Recording bitrate (bits/s) for the shell.
const RECORD_BITRATE_BPS: u32 = 4_000_000;

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

/// An active streaming session: streamer + encoders + clocks.
pub(crate) struct ActiveStream {
    streamer: Streamer<RealTransport>,
    video_enc: VideoEncoder,
    audio_enc: Option<AudioEncoder>,
    video_pts_us: i64,
    audio_pts_us: i64,
    /// When the next reconnect attempt is due (`None` while live).
    reconnect_at: Option<Instant>,
}

impl ActiveStream {
    pub(crate) fn bytes_sent(&self) -> u64 {
        self.streamer.bytes_sent()
    }
    pub(crate) fn encoder_name(&self) -> &'static str {
        self.video_enc.encoder_name()
    }
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
        }
    }

    fn stream_info(&self) -> StreamInfo {
        match self {
            ProducerKind::Media(p) => p.stream_info(),
            #[cfg(windows)]
            ProducerKind::Camera(p) => p.stream_info(),
        }
    }
}

impl ProducerKind {
    /// Next normalized audio chunk, if the producer has audio.
    /// Camera sources have no audio capture in this MVP.
    fn next_audio(&mut self) -> Result<Option<sf_capture::media::DecodedAudio>, CaptureError> {
        match self {
            ProducerKind::Media(p) => p.next_audio(),
            #[cfg(windows)]
            ProducerKind::Camera(_) => Ok(None),
        }
    }
}

/// Nearest-neighbor RGBA resize (shell-grade; production will use the GPU).
fn resize_rgba_nearest(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
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
        width: u32,
        height: u32,
    ) -> Result<(), ShellError> {
        // Guard against a panic in the FFmpeg/dshow open path taking down
        // the whole app: convert it into a clean capture error instead.
        let device_owned = device.map(|d| d.to_string());
        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match device_owned.as_deref() {
                Some(d) => sf_capture::dshow::DshowProducer::open(d, width, height, 30),
                None => sf_capture::dshow::DshowProducer::open_default(width, height, 30),
            }
        }));
        let prod = match opened {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                return Err(ShellError::Capture(format!("cannot open camera: {e}")))
            }
            Err(_) => {
                return Err(ShellError::Capture(
                    "cannot open camera: the capture backend crashed while opening the device"
                        .into(),
                ))
            }
        };
        self.media_producers.insert(
            source_id.to_string(),
            SendProducer(ProducerKind::Camera(prod)),
        );
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
                    resize_rgba_nearest(&f.data, f.width, f.height, w, h)
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

    /// True when any open media producer has an audio stream.
    fn has_audio_sources(&self) -> bool {
        self.media_producers
            .values()
            .any(|p| p.stream_info().has_audio)
    }

    // ------------------------------------------------------------------
    // Audio: pull + mix + meters
    // ------------------------------------------------------------------

    /// Pulls pending audio chunks from all media producers, runs the
    /// `sf-audio` mixer, and refreshes meter state. Returns the owned
    /// mixed samples (empty when nothing flowed). This is the single
    /// place meters become real.
    pub(crate) fn pump_audio(&mut self) -> Vec<f32> {
        // Sync mixer registration with the current media sources.
        let ids: Vec<String> = self.media_producers.keys().cloned().collect();
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

        // Drain a bounded number of chunks per producer per tick.
        let mut chunks: Vec<(String, Vec<f32>, u32, u32)> = Vec::new();
        for id in &ids {
            if let Some(prod) = self.media_producers.get_mut(id) {
                for _ in 0..4 {
                    match prod.next_audio() {
                        Ok(Some(c)) => {
                            chunks.push((id.clone(), c.samples, c.channels, c.sample_rate))
                        }
                        _ => break,
                    }
                }
            }
        }
        if chunks.is_empty() {
            return Vec::new();
        }
        let frames: Vec<(&str, AudioFrame)> = chunks
            .iter()
            .map(|(id, s, ch, sr)| {
                (
                    id.as_str(),
                    AudioFrame {
                        samples: s,
                        channels: *ch,
                        sample_rate: *sr,
                    },
                )
            })
            .collect();
        let mixed = self.audio_mixer.mix(&frames);
        for m in &mixed.per_source {
            self.meter_values.insert(m.id.clone(), m.peak);
        }
        if !mixed.samples.is_empty() {
            self.meters_live = true;
        }
        mixed.samples
    }

    // ------------------------------------------------------------------
    // Program frame (raw, for encode)
    // ------------------------------------------------------------------

    /// Raw RGBA program frame: live crossfade during an auto transition,
    /// otherwise the program scene composite. Mirrors `program_frame_png`.
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

    fn video_encoder_opts() -> VideoEncoderOpts {
        VideoEncoderOpts {
            width: AV_W,
            height: AV_H,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: RECORD_BITRATE_BPS,
            gop: 60,
            preset: "veryfast".to_string(),
        }
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
            let video_enc = VideoEncoder::open(Self::video_encoder_opts())
                .map_err(|e| format!("video encoder: {e}"))?;
            let mut recorder =
                Recorder::create(&path, fmt).map_err(|e| format!("recorder: {e}"))?;
            recorder
                .add_video_stream(&video_enc.parameters())
                .map_err(|e| format!("recorder: {e}"))?;
            let audio_enc = if self.has_audio_sources() {
                let enc = AudioEncoder::open().map_err(|e| format!("audio encoder: {e}"))?;
                recorder
                    .add_audio_stream(&enc.parameters(), enc.sample_rate())
                    .map_err(|e| format!("recorder: {e}"))?;
                Some(enc)
            } else {
                None
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
        rec.video_pts_us += AV_FRAME_US;
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
            self.active_recording = None;
            self.record_runtime = None;
            self.record_error = Some(m);
            self.emit_record_state();
        }
    }

    // ------------------------------------------------------------------
    // Streaming
    // ------------------------------------------------------------------

    /// Starts streaming: builds encoders, opens a real RTMP connection
    /// through `sf-output`'s `Streamer`. Unreachable servers fail cleanly
    /// into `Error` with a redacted message; the key never appears.
    pub fn stream_start(&mut self) -> Result<(), ShellError> {
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
            let video_enc = VideoEncoder::open(Self::video_encoder_opts())
                .map_err(|e| format!("video encoder: {e}"))?;
            let audio_enc = if self.has_audio_sources() {
                Some(AudioEncoder::open().map_err(|e| format!("audio encoder: {e}"))?)
            } else {
                None
            };
            let audio_params = audio_enc.as_ref().map(|a| a.parameters());
            let target = StreamTarget::Rtmp(full_url.clone());
            let transport =
                RealTransport::new(target.clone(), video_enc.parameters(), audio_params);
            Ok(ActiveStream {
                streamer: Streamer::new(target, transport, ReconnectPolicy::default()),
                video_enc,
                audio_enc,
                video_pts_us: 0,
                audio_pts_us: 0,
                reconnect_at: None,
            })
        };
        let mut active = match build() {
            Ok(a) => a,
            Err(m) => {
                let msg = sanitize_stream_error(&full_url, &key, "stream setup failed", &m);
                self.stream_lifecycle = StreamLifecycle::Error {
                    message: msg.clone(),
                };
                self.emit_stream_state();
                return Err(ShellError::Stream(msg));
            }
        };
        match active.streamer.start() {
            Ok(()) => {
                self.stream_lifecycle = StreamLifecycle::Live;
                self.active_stream = Some(active);
                self.emit_stream_state();
                Ok(())
            }
            Err(e) => {
                let msg = sanitize_stream_error(&full_url, &key, "RTMP connect failed", &e);
                self.stream_lifecycle = StreamLifecycle::Error {
                    message: msg.clone(),
                };
                self.emit_stream_state();
                Err(ShellError::Stream(msg))
            }
        }
    }

    /// Stops streaming (any state → `Idle`), closing the transport.
    pub fn stream_stop(&mut self) -> Result<(), ShellError> {
        if let Some(mut active) = self.active_stream.take() {
            active.streamer.stop();
        }
        self.stream_lifecycle = StreamLifecycle::Idle;
        self.emit_stream_state();
        Ok(())
    }

    /// One streaming step, called from [`Shell::av_tick`]: encodes the
    /// given program frame and mixed audio, and sends; drives the
    /// reconnect state machine.
    pub(crate) fn stream_tick(&mut self, frame: &Frame, audio: &[f32]) {
        if self.active_stream.is_none() {
            return;
        }
        // Reconnect timer path.
        let due = self
            .active_stream
            .as_ref()
            .and_then(|a| a.reconnect_at)
            .map(|t| Instant::now() >= t)
            .unwrap_or(false);
        if due {
            let (result, state, delay) = {
                let a = match self.active_stream.as_mut() {
                    Some(a) => a,
                    None => return,
                };
                a.reconnect_at = None;
                let r = a.streamer.on_reconnect_timer();
                (r, a.streamer.state(), a.streamer.reconnect_delay_ms())
            };
            match (result, state) {
                (Ok(()), OutputStreamState::Live) => {
                    self.stream_lifecycle = StreamLifecycle::Live;
                    self.emit_stream_state();
                }
                (Ok(()), OutputStreamState::Failed) => {
                    let m = "reconnect attempts exhausted".to_string();
                    self.stream_lifecycle = StreamLifecycle::Error { message: m };
                    self.active_stream = None;
                    self.emit_stream_state();
                }
                (Ok(()), _) => {
                    // Still reconnecting: schedule the next attempt.
                    if let Some(a) = self.active_stream.as_mut() {
                        a.reconnect_at =
                            Some(Instant::now() + Duration::from_millis(delay.unwrap_or(1000)));
                    }
                    self.stream_lifecycle = StreamLifecycle::Reconnecting;
                    self.emit_stream_state();
                }
                (Err(e), _) => {
                    let url = self.stream_settings.url.clone();
                    let key = self.stream_settings.key.clone();
                    let m = sanitize_stream_error(&url, &key, "reconnect failed", &e);
                    self.stream_lifecycle = StreamLifecycle::Error { message: m };
                    self.active_stream = None;
                    self.emit_stream_state();
                }
            }
            return;
        }
        // If the streamer is mid-reconnect (timer not yet due), hold.
        let reconnecting = matches!(
            self.active_stream.as_ref().map(|a| a.streamer.state()),
            Some(OutputStreamState::Reconnecting { .. })
        );
        if reconnecting {
            return;
        }
        // Live path: encode the rendered frame, send.
        let url = self.stream_settings.url.clone();
        let key = self.stream_settings.key.clone();
        let a = match self.active_stream.as_mut() {
            Some(a) => a,
            None => return,
        };
        let vpts = a.video_pts_us;
        a.video_pts_us += AV_FRAME_US;
        let mut send_failed: Option<String> = None;
        // (No helper closure: `a` is already mutably borrowed, so the
        // send is inlined to keep borrowck happy.)
        match a.video_enc.encode_rgba(&frame.pixels, vpts) {
            Ok(pkts) => {
                for p in &pkts {
                    if let Err(e) = a.streamer.send_packet(p) {
                        send_failed =
                            Some(sanitize_stream_error(&url, &key, "RTMP send failed", &e));
                        break;
                    }
                }
            }
            Err(e) => send_failed = Some(format!("encode video: {e}")),
        }
        if send_failed.is_none() && !audio.is_empty() {
            if let Some(aenc) = a.audio_enc.as_mut() {
                let apts = a.audio_pts_us;
                a.audio_pts_us += audio.len() as i64 / 2 * 1_000_000 / 48_000;
                match aenc.encode_pcm(&audio, apts) {
                    Ok(pkts) => {
                        for p in &pkts {
                            if let Err(e) = a.streamer.send_packet(p) {
                                send_failed =
                                    Some(sanitize_stream_error(&url, &key, "RTMP send failed", &e));
                                break;
                            }
                        }
                    }
                    Err(e) => send_failed = Some(format!("encode audio: {e}")),
                }
            }
        }
        if send_failed.is_some() {
            // Drop to the reconnect path (or error if the streamer gave up).
            let delay = a.streamer.reconnect_delay_ms().unwrap_or(1000);
            a.reconnect_at = Some(Instant::now() + Duration::from_millis(delay));
            // Nudge the streamer into its reconnect state machine.
            let _ = a.streamer.on_reconnect_timer();
            self.stream_lifecycle = StreamLifecycle::Reconnecting;
            self.emit_stream_state();
        }
    }

    // ------------------------------------------------------------------
    // Poll entry
    // ------------------------------------------------------------------

    /// Called by the adapter's 66 ms poll loop (after `transition_tick`):
    /// renders one program frame, pumps audio (meters + record/stream
    /// feed), and advances recording and streaming. Never panics;
    /// backend failures degrade to lifecycle `Error` states.
    pub fn av_tick(&mut self) {
        let tick_start = std::time::Instant::now();
        let recording = self.active_recording.is_some();
        let streaming = self.active_stream.is_some();
        if !recording && !streaming {
            // Audio metering runs even when idle so the UI meters stay live.
            self.pump_audio();
            return;
        }
        let frame = match self.program_frame_raw(AV_W, AV_H) {
            Ok(f) => f,
            Err(_) => return,
        };
        // Audio metering runs on every tick; the same mixed block feeds
        // the record/stream encoders below.
        let audio = self.pump_audio();
        if recording {
            self.record_tick(&frame, &audio);
        }
        if streaming {
            self.stream_tick(&frame, &audio);
        }
        let tick_us = tick_start.elapsed().as_micros() as u64;
        self.perf_mark_tick(tick_us);
        self.perf_mark_frame();
    }
}
