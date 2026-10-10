//! RTMP streaming: [`StreamTarget`], [`Transport`], [`RealTransport`],
//! [`FakeTransport`], [`ReconnectPolicy`], and the [`Streamer`] state machine.
//!
//! # Verification status (honest)
//!
//! * FLV muxing through the exact libavformat code path used for RTMP is
//!   verified by writing to a local `.flv` file and reading it back with
//!   ffprobe (see test `flv_mux_to_file_proves_rtmp_code_path`).
//! * **RTMP network delivery is untested on this VM** — there is no RTMP
//!   server here. On Windows, `RealTransport::connect` opens the RTMP URL
//!   with libavformat (`format::output_as(url, "flv")`), which performs the
//!   RTMP handshake at connect time.
//!
//! # Stream-key secrecy
//!
//! [`StreamTarget`] implements `Debug` manually and redacts the key; the
//! redacted form is what [`Transport::connect`] receives for logging. Tested.

use ffmpeg_next as ff;
use sf_encode::EncodedPacket;
use std::collections::VecDeque;
use std::fmt;
use thiserror::Error;

/// FLV timestamps are milliseconds: both streams use a 1/1000 time base and
/// `send` rescales the µs packet clock into it.
const FLV_TIME_BASE: ff::Rational = ff::Rational(1, 1_000);

/// Where to stream. The key is never shown: `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub enum StreamTarget {
    /// RTMP publish URL, e.g. `rtmp://live.twitch.tv/app/<key>`.
    Rtmp(String),
}

impl StreamTarget {
    /// The real URL — only ever handed to the transport layer, never logged.
    pub fn url(&self) -> &str {
        match self {
            StreamTarget::Rtmp(url) => url,
        }
    }

    /// The redacted form safe for logs and [`Transport::connect`].
    pub fn redacted(&self) -> String {
        format!("{self:?}")
    }
}

impl fmt::Debug for StreamTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamTarget::Rtmp(url) => write!(f, "Rtmp({})", redact_url(url)),
        }
    }
}

/// Redact the secret tail of an RTMP URL for logs.
///
/// `rtmp://host/app/SECRET` → `rtmp://host/app/•••`. With a single path
/// segment (`rtmp://host/SECRET`) the segment itself is the secret and is
/// redacted; a URL with no path is returned unchanged (nothing secret in it).
pub fn redact_url(url: &str) -> String {
    const HIDDEN: &str = "•••";
    let rest = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => return HIDDEN.to_string(),
    };
    let (host, path) = match rest.find('/') {
        Some(k) => (&rest[..k], &rest[k..]),
        None => return url.to_string(),
    };
    let scheme = &url[..url.find("://").unwrap()];
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segs.len() {
        0 => url.to_string(),
        1 => format!("{scheme}://{host}/{HIDDEN}"),
        _ => format!(
            "{scheme}://{host}/{}/{HIDDEN}",
            segs[..segs.len() - 1].join("/")
        ),
    }
}

/// Transport errors.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TransportError {
    /// `send`/`close` called with no live connection.
    #[error("not connected")]
    NotConnected,
    /// The RTMP/file connection could not be established.
    #[error("connection failed: {0}")]
    ConnectionFailed(String),
    /// A packet could not be delivered on a live connection.
    #[error("send failed: {0}")]
    SendFailed(String),
}

/// Packet transport to the stream target.
///
/// `connect` receives only the *redacted* URL (for logging); the real URL
/// lives inside the transport (see [`RealTransport::new`]).
pub trait Transport {
    fn connect(&mut self, url_redacted: &str) -> Result<(), TransportError>;
    fn send(&mut self, packet: &EncodedPacket) -> Result<(), TransportError>;
    fn close(&mut self) -> Result<(), TransportError>;
}

/// Real libavformat transport: FLV mux to the RTMP URL.
///
/// Created with the full [`StreamTarget`] (kept private); `connect` opens the
/// URL with libavformat, which performs the RTMP handshake. Streams use the
/// encoder's codec parameters (SPS/PPS in extradata → FLV sequence header).
pub struct RealTransport {
    target: StreamTarget,
    octx: Option<ff::format::context::Output>,
    video_params: ff::codec::parameters::Parameters,
    audio_params: Option<ff::codec::parameters::Parameters>,
    video_stream: usize,
    audio_stream: Option<usize>,
    header_written: bool,
}

impl RealTransport {
    pub fn new(
        target: StreamTarget,
        video_params: ff::codec::parameters::Parameters,
        audio_params: Option<ff::codec::parameters::Parameters>,
    ) -> Self {
        Self {
            target,
            octx: None,
            video_params,
            audio_params,
            video_stream: 0,
            audio_stream: None,
            header_written: false,
        }
    }

    fn add_stream(
        octx: &mut ff::format::context::Output,
        params: &ff::codec::parameters::Parameters,
    ) -> Result<(), TransportError> {
        // ffmpeg-next 6.x: add the stream for the codec, then overwrite its
        // parameters (incl. extradata) from the encoder's.
        let codec = ff::encoder::find(params.id()).ok_or_else(|| {
            TransportError::ConnectionFailed(format!(
                "no encoder for {:?} in registry",
                params.id()
            ))
        })?;
        let mut stream = octx
            .add_stream(codec)
            .map_err(|e| TransportError::ConnectionFailed(format!("add_stream: {e}")))?;
        stream.set_time_base(FLV_TIME_BASE);
        stream.set_parameters(params.clone());
        Ok(())
    }
}

impl Transport for RealTransport {
    fn connect(&mut self, url_redacted: &str) -> Result<(), TransportError> {
        ff::init().map_err(|e| TransportError::ConnectionFailed(format!("ffmpeg init: {e}")))?;
        // Explicit "flv": the RTMP URL has no file extension to guess from.
        // For a local .flv path (tests) this is the identical mux code path.
        let url = self.target.url().to_string();
        let mut octx = ff::format::output_as(&url, "flv")
            .map_err(|e| TransportError::ConnectionFailed(format!("{url_redacted}: {e}")))?;
        Self::add_stream(&mut octx, &self.video_params)?;
        self.video_stream = 0;
        if let Some(ap) = &self.audio_params {
            Self::add_stream(&mut octx, ap)?;
            self.audio_stream = Some(1);
        }
        self.octx = Some(octx);
        self.header_written = false;
        Ok(())
    }

    fn send(&mut self, packet: &EncodedPacket) -> Result<(), TransportError> {
        let octx = self.octx.as_mut().ok_or(TransportError::NotConnected)?;
        if !self.header_written {
            // See Recorder: shift negative B-frame DTS to zero.
            let mut dict = ff::Dictionary::new();
            dict.set("avoid_negative_ts", "make_zero");
            octx.write_header_with(dict)
                .map_err(|e| TransportError::SendFailed(format!("flv header: {e}")))?;
            self.header_written = true;
        }
        let stream_index = match packet.stream_index {
            0 => self.video_stream,
            1 => self
                .audio_stream
                .ok_or(TransportError::SendFailed("no audio stream".into()))?,
            s => return Err(TransportError::SendFailed(format!("bad stream_index {s}"))),
        };
        let mut fp = ff::codec::packet::Packet::copy(&packet.data);
        // FLV timestamps are milliseconds: rescale the µs packet clock.
        fp.set_pts(Some(super::rescale_us_to_tb(packet.pts_us, FLV_TIME_BASE)));
        fp.set_dts(Some(super::rescale_us_to_tb(packet.dts_us, FLV_TIME_BASE)));
        fp.set_stream(stream_index);
        fp.write(octx)
            .map_err(|e| TransportError::SendFailed(format!("{e}")))?;
        Ok(())
    }

    fn close(&mut self) -> Result<(), TransportError> {
        if let Some(mut octx) = self.octx.take() {
            super::write_trailer(&mut octx)
                .map_err(|e| TransportError::SendFailed(format!("flv trailer: {e}")))?;
        }
        self.header_written = false;
        Ok(())
    }
}

/// Test transport with scripted outcomes. Records every call.
#[derive(Debug, Default)]
pub struct FakeTransport {
    pub connect_script: VecDeque<Result<(), TransportError>>,
    pub send_script: VecDeque<Result<(), TransportError>>,
    pub connects: u32,
    pub sends: u32,
    pub closes: u32,
    /// The redacted URL the last `connect` received (for redaction tests).
    pub last_connect_arg: Option<String>,
}

impl FakeTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// A transport whose first `n` connects fail, then succeed.
    pub fn failing_connects(n: u32) -> Self {
        Self {
            connect_script: (0..n)
                .map(|_| Err(TransportError::ConnectionFailed("scripted".into())))
                .collect(),
            ..Self::default()
        }
    }

    /// A transport whose connects always succeed but sends fail per script.
    pub fn with_send_script(script: Vec<Result<(), TransportError>>) -> Self {
        Self {
            send_script: script.into(),
            ..Self::default()
        }
    }
}

impl Transport for FakeTransport {
    fn connect(&mut self, url_redacted: &str) -> Result<(), TransportError> {
        self.connects += 1;
        self.last_connect_arg = Some(url_redacted.to_string());
        self.connect_script.pop_front().unwrap_or(Ok(()))
    }

    fn send(&mut self, _packet: &EncodedPacket) -> Result<(), TransportError> {
        self.sends += 1;
        self.send_script.pop_front().unwrap_or(Ok(()))
    }

    fn close(&mut self) -> Result<(), TransportError> {
        self.closes += 1;
        Ok(())
    }
}

/// Exponential-backoff reconnect policy.
#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    /// How many reconnect attempts before giving up (the initial `start`
    /// attempt does not count against this).
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay_ms: 1_000,
            max_delay_ms: 30_000,
        }
    }
}

impl ReconnectPolicy {
    /// Delay before reconnect attempt `attempt` (0-based): base * 2^attempt,
    /// capped at `max_delay_ms`.
    pub fn delay_for(&self, attempt: u32) -> u64 {
        let shift = attempt.min(20);
        self.base_delay_ms
            .saturating_mul(1 << shift)
            .min(self.max_delay_ms)
    }
}

/// Streamer connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    Connecting,
    Live,
    Reconnecting { attempt: u32 },
    Failed,
}

/// RTMP streamer: owns a [`Transport`], drives connect → live → reconnect.
///
/// The engine loop calls [`Streamer::on_reconnect_timer`] after
/// [`Streamer::reconnect_delay_ms`] has elapsed; no sleeping happens here,
/// which keeps the state machine fully unit-testable.
pub struct Streamer<T: Transport> {
    target: StreamTarget,
    transport: T,
    policy: ReconnectPolicy,
    state: StreamState,
    /// Attempts since the last success (1-based while reconnecting).
    failures: u32,
    total_attempts: u32,
    next_delay_ms: u64,
}

impl<T: Transport> Streamer<T> {
    pub fn new(target: StreamTarget, transport: T, policy: ReconnectPolicy) -> Self {
        Self {
            target,
            transport,
            policy,
            state: StreamState::Idle,
            failures: 0,
            total_attempts: 0,
            next_delay_ms: 0,
        }
    }

    pub fn state(&self) -> StreamState {
        self.state
    }

    /// All connect attempts so far (initial + reconnects).
    pub fn total_attempts(&self) -> u32 {
        self.total_attempts
    }

    /// Delay the engine should wait before [`Streamer::on_reconnect_timer`];
    /// `None` unless currently reconnecting.
    pub fn reconnect_delay_ms(&self) -> Option<u64> {
        match self.state {
            StreamState::Reconnecting { .. } => Some(self.next_delay_ms),
            _ => None,
        }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Start streaming: Idle → Connecting, connect immediately.
    pub fn start(&mut self) -> Result<(), TransportError> {
        let _ = self.transport.close();
        self.failures = 0;
        self.state = StreamState::Connecting;
        self.try_connect()
    }

    /// The reconnect backoff has elapsed — try again now.
    /// No-op unless currently reconnecting.
    pub fn on_reconnect_timer(&mut self) -> Result<(), TransportError> {
        if !matches!(self.state, StreamState::Reconnecting { .. }) {
            return Ok(());
        }
        self.state = StreamState::Connecting;
        self.try_connect()
    }

    fn try_connect(&mut self) -> Result<(), TransportError> {
        self.total_attempts += 1;
        let redacted = self.target.redacted();
        match self.transport.connect(&redacted) {
            Ok(()) => {
                self.state = StreamState::Live;
                self.failures = 0;
                Ok(())
            }
            Err(e) => {
                self.failures += 1;
                if self.failures > self.policy.max_attempts {
                    self.state = StreamState::Failed;
                } else {
                    self.next_delay_ms = self.policy.delay_for(self.failures - 1);
                    self.state = StreamState::Reconnecting {
                        attempt: self.failures,
                    };
                }
                Err(e)
            }
        }
    }

    /// Send one packet. Only valid while Live; a send error drops the
    /// connection and enters the reconnect backoff.
    pub fn send_packet(&mut self, packet: &EncodedPacket) -> Result<(), TransportError> {
        if self.state != StreamState::Live {
            return Err(TransportError::NotConnected);
        }
        match self.transport.send(packet) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = self.transport.close();
                self.failures = 1;
                self.next_delay_ms = self.policy.delay_for(0);
                self.state = StreamState::Reconnecting { attempt: 1 };
                Err(e)
            }
        }
    }

    /// Stop streaming and close the transport.
    pub fn stop(&mut self) {
        let _ = self.transport.close();
        self.state = StreamState::Idle;
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sf_encode::{AudioEncoder, EncoderId, VideoEncoder, VideoEncoderOpts};
    use std::process::Command;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: 5,
            base_delay_ms: 1_000,
            max_delay_ms: 30_000,
        }
    }

    fn target() -> StreamTarget {
        StreamTarget::Rtmp("rtmp://live.twitch.tv/app/live_SECRETKEY".into())
    }

    #[test]
    fn reconnect_succeeds_after_two_failures() {
        let mut s = Streamer::new(target(), FakeTransport::failing_connects(2), policy());
        assert_eq!(s.state(), StreamState::Idle);

        assert!(s.start().is_err());
        assert_eq!(s.state(), StreamState::Reconnecting { attempt: 1 });
        assert_eq!(s.reconnect_delay_ms(), Some(1_000));

        assert!(s.on_reconnect_timer().is_err());
        assert_eq!(s.state(), StreamState::Reconnecting { attempt: 2 });
        assert_eq!(s.reconnect_delay_ms(), Some(2_000));

        assert!(s.on_reconnect_timer().is_ok());
        assert_eq!(s.state(), StreamState::Live);
        assert_eq!(s.reconnect_delay_ms(), None);
        assert_eq!(s.total_attempts(), 3);
        assert_eq!(s.transport().connects, 3);
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let mut s = Streamer::new(
            target(),
            FakeTransport::failing_connects(100),
            ReconnectPolicy {
                max_attempts: 3,
                ..policy()
            },
        );
        assert!(s.start().is_err()); // attempt 1
        assert!(s.on_reconnect_timer().is_err()); // attempt 2
        assert!(s.on_reconnect_timer().is_err()); // attempt 3
        assert_eq!(s.state(), StreamState::Reconnecting { attempt: 3 });
        assert!(s.on_reconnect_timer().is_err()); // attempt 4 > max -> Failed
        assert_eq!(s.state(), StreamState::Failed);
        assert_eq!(s.total_attempts(), 4);
        // Further timer ticks are no-ops once failed.
        assert!(s.on_reconnect_timer().is_ok());
        assert_eq!(s.state(), StreamState::Failed);
        assert_eq!(s.total_attempts(), 4);
    }

    #[test]
    fn backoff_delays_double_and_cap() {
        let p = ReconnectPolicy {
            max_attempts: 10,
            base_delay_ms: 1_000,
            max_delay_ms: 5_000,
        };
        assert_eq!(p.delay_for(0), 1_000);
        assert_eq!(p.delay_for(1), 2_000);
        assert_eq!(p.delay_for(2), 4_000);
        assert_eq!(p.delay_for(3), 5_000);
        assert_eq!(p.delay_for(30), 5_000);
    }

    #[test]
    fn send_failure_enters_reconnect() {
        let mut s = Streamer::new(
            target(),
            FakeTransport::with_send_script(vec![Err(TransportError::SendFailed("boom".into()))]),
            policy(),
        );
        s.start().expect("connect ok");
        assert_eq!(s.state(), StreamState::Live);
        let pkt = EncodedPacket {
            data: vec![1, 2, 3],
            pts_us: 0,
            dts_us: 0,
            is_keyframe: true,
            stream_index: 0,
        };
        assert!(s.send_packet(&pkt).is_err());
        assert_eq!(s.state(), StreamState::Reconnecting { attempt: 1 });
        assert_eq!(s.reconnect_delay_ms(), Some(1_000));
        // Sending while reconnecting is rejected.
        assert!(matches!(
            s.send_packet(&pkt).unwrap_err(),
            TransportError::NotConnected
        ));
    }

    #[test]
    fn stop_resets_to_idle() {
        let mut s = Streamer::new(target(), FakeTransport::failing_connects(100), policy());
        let _ = s.start();
        assert!(matches!(s.state(), StreamState::Reconnecting { .. }));
        s.stop();
        assert_eq!(s.state(), StreamState::Idle);
        assert_eq!(s.transport().closes, 2); // one from start(), one from stop()
    }

    #[test]
    fn stream_key_never_leaks() {
        let t = target();
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("SECRETKEY"), "key leaked in Debug: {dbg}");
        assert!(dbg.contains("•••"), "redaction marker missing: {dbg}");
        assert_eq!(
            redact_url("rtmp://live.twitch.tv/app/live_SECRETKEY"),
            "rtmp://live.twitch.tv/app/•••"
        );
        assert_eq!(redact_url("rtmp://host/SECRET"), "rtmp://host/•••");
        assert_eq!(redact_url("rtmp://host"), "rtmp://host");
        assert_eq!(redact_url("not a url"), "•••");

        // The transport only ever sees the redacted form.
        let mut s = Streamer::new(t, FakeTransport::new(), policy());
        s.start().expect("connect");
        let arg = s.transport().last_connect_arg.clone().unwrap();
        assert!(!arg.contains("SECRETKEY"), "key leaked to transport: {arg}");
        assert!(arg.contains("•••"));
    }

    /// FLV muxing through the exact libavformat code path `RealTransport`
    /// uses for RTMP, verified against a local .flv file + ffprobe.
    /// RTMP *network* delivery remains untested on this VM (no RTMP server).
    #[test]
    fn flv_mux_to_file_proves_rtmp_code_path() {
        let (w, h, fps) = (320u32, 180u32, 30u32);
        let mut venc = VideoEncoder::open_with_encoder(
            EncoderId::H264Libx264,
            VideoEncoderOpts {
                width: w,
                height: h,
                fps_num: fps,
                fps_den: 1,
                bitrate_bps: 800_000,
                gop: 30,
                preset: "ultrafast".to_string(),
            },
        )
        .expect("open video");
        let mut aenc = AudioEncoder::open().expect("open audio");

        let mut pkts: Vec<EncodedPacket> = Vec::new();
        for i in 0..30u32 {
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for (j, px) in rgba.chunks_exact_mut(4).enumerate() {
                px[0] = ((j + i as usize * 8) % 256) as u8;
                px[1] = 64;
                px[2] = 192;
                px[3] = 255;
            }
            pkts.extend(
                venc.encode_rgba(&rgba, i as i64 * 1_000_000 / fps as i64)
                    .expect("v"),
            );
        }
        pkts.extend(venc.flush().expect("vflush"));
        let mut pcm = Vec::new();
        for i in 0..48_000usize {
            let s = (0.4 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48_000.0).sin()) as f32;
            pcm.push(s);
            pcm.push(s);
        }
        for (ci, chunk) in pcm.chunks(8192).enumerate() {
            pkts.extend(
                aenc.encode_pcm(chunk, ci as i64 * 8192 / 2 * 1_000_000 / 48_000)
                    .expect("a"),
            );
        }
        pkts.extend(aenc.flush().expect("aflush"));
        // DTS interleave order (see recorder test).
        pkts.sort_by_key(|p| (p.dts_us, p.stream_index));

        let path = "/tmp/stream-test.flv";
        let _ = std::fs::remove_file(path);
        // NOTE: a file path as the "RTMP URL" — format::output_as(path, "flv")
        // runs the identical libavformat FLV mux path as a real rtmp:// URL;
        // only the AVIO target differs (file vs network).
        let mut t = RealTransport::new(
            StreamTarget::Rtmp(path.to_string()),
            venc.parameters(),
            Some(aenc.parameters()),
        );
        t.connect("Rtmp(/tmp/•••)").expect("connect");
        for p in &pkts {
            t.send(p).expect("send");
        }
        t.close().expect("close");

        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "csv",
                path,
            ])
            .output()
            .expect("run ffprobe");
        assert!(
            out.status.success(),
            "ffprobe failed on flv: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        eprintln!("flv streams: {text}");
        assert!(text.contains("h264"), "no h264 in flv");
        assert!(text.contains("aac"), "no aac in flv");

        // Duration sanity: 30 video frames @30fps + 1 s audio → ~1 s total.
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=nw=1",
                path,
            ])
            .output()
            .expect("run ffprobe");
        let text = String::from_utf8_lossy(&out.stdout);
        let duration: f64 = text
            .lines()
            .find_map(|l| l.strip_prefix("duration="))
            .and_then(|s| s.parse().ok())
            .expect("duration in ffprobe output");
        eprintln!("flv duration: {duration}");
        assert!(
            (0.8..1.5).contains(&duration),
            "flv duration {duration} not near 1 s"
        );
    }
}
