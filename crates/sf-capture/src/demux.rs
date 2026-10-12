//! Shared FFmpeg demux + decode pump (private to this crate).
//!
//! Both [`super::media::MediaFileProducer`] and [`super::screen::ScreenProducer`]
//! are thin wrappers around [`DemuxDecoder`]: it owns the input context,
//! dispatches packets to the video/audio decoders, scales video to RGBA8
//! and resamples audio to the crate's normalized format (48000 Hz, stereo,
//! interleaved `f32` — see [`super::media::AUDIO_SAMPLE_RATE`]).
//!
//! The pump is pull-based and never blocks on the consumer: each call
//! decodes at most one output frame per stream; decoded frames of the
//! *other* stream are parked in a small bounded queue.

use std::collections::VecDeque;
use std::sync::Once;

use ffmpeg_next as ff;

use super::{CaptureError, CapturedFrame, StreamInfo};

static FFMPEG_INIT: Once = Once::new();

pub(crate) fn ensure_init() {
    FFMPEG_INIT.call_once(|| {
        let _ = ff::init();
    });
}

/// Convert a stream timestamp to microseconds. `None` (missing PTS) falls
/// back to `fallback_us` supplied by the caller.
pub(crate) fn pts_to_us(pts: Option<i64>, tb_num: i32, tb_den: i32, fallback_us: i64) -> i64 {
    match pts {
        Some(p) if tb_den != 0 => (p as i128 * 1_000_000 * tb_num as i128 / tb_den as i128)
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        _ => fallback_us,
    }
}

/// One decoded + resampled audio chunk. Interleaved `f32`, always
/// [`super::media::AUDIO_CHANNELS`] channels at
/// [`super::media::AUDIO_SAMPLE_RATE`] Hz.
#[derive(Debug, Clone)]
pub(crate) struct RawAudio {
    pub samples: Vec<f32>,
    pub pts_us: i64,
}

// SAFETY: `DemuxDecoder` wraps FFmpeg contexts (raw pointers) which have
// no thread affinity. It is only ever accessed from a single thread at a
// time — the capture thread that owns it after being moved there.
unsafe impl Send for DemuxDecoder {}

pub(crate) struct DemuxDecoder {
    ictx: ff::format::context::Input,
    video_stream: usize,
    audio_stream: Option<usize>,
    video_decoder: ff::decoder::Video,
    audio_decoder: Option<ff::decoder::Audio>,
    scaler: ff::software::scaling::Context,
    resampler: Option<ff::software::resampling::Context>,
    video_tb: (i32, i32),
    audio_tb: (i32, i32),
    video_queue: VecDeque<CapturedFrame>,
    audio_queue: VecDeque<RawAudio>,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    video_frames_out: u64,
    audio_samples_out: u64,
    nominal_frame_us: u64,
    packets_exhausted: bool,
    eof_sent: bool,
    /// Live sources (e.g. dshow capture) drop backlog: the queue never
    /// holds more than one frame, so a slow consumer can't accumulate
    /// unbounded memory. File sources keep every frame.
    live: bool,
}

/// Bound on the decoded-video queue for file sources. The audio seek path
/// used to decode-and-queue every video packet it skipped, growing this
/// queue by ~3 frames/tick (~25 MB/tick at 1080p RGBA) until the process
/// OOM-crashed. With the discard fix below the queue stays tiny; this cap
/// is the backstop demanded by the realtime blueprint ("never let frames
/// accumulate").
const MAX_VIDEO_QUEUE: usize = 16;

/// Max frames `drain_video` decodes per call. The old code decoded the
/// entire remainder of the file at EOF into RAM in one call.
const MAX_DRAIN_PER_CALL: usize = 8;

impl DemuxDecoder {
    /// Build from an already-opened input context. `nominal_frame_us` is
    /// the expected per-frame duration (used for `duration_us` and as the
    /// PTS fallback when the container omits timestamps).
    pub(crate) fn from_input(
        ictx: ff::format::context::Input,
        nominal_frame_us: u64,
    ) -> Result<Self, CaptureError> {
        let video_stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .ok_or_else(|| CaptureError::OpenFailed("no video stream found".into()))?;
        let video_index = video_stream.index();
        let video_tb = {
            let tb = video_stream.time_base();
            (tb.numerator(), tb.denominator())
        };
        let (fps_num, fps_den) = {
            let r = video_stream.avg_frame_rate();
            (r.numerator().max(0) as u32, r.denominator().max(0) as u32)
        };

        let video_decoder = ff::codec::context::Context::from_parameters(video_stream.parameters())
            .map_err(|e| CaptureError::OpenFailed(format!("video codec params: {e}")))?
            .decoder()
            .video()
            .map_err(|e| CaptureError::OpenFailed(format!("video decoder open: {e}")))?;
        let (w, h) = (video_decoder.width(), video_decoder.height());
        if w == 0 || h == 0 {
            return Err(CaptureError::OpenFailed(
                "video stream has zero dimensions".into(),
            ));
        }
        let scaler = ff::software::scaling::Context::get(
            video_decoder.format(),
            w,
            h,
            ff::util::format::pixel::Pixel::RGBA,
            w,
            h,
            ff::software::scaling::flag::Flags::BILINEAR,
        )
        .map_err(|e| CaptureError::OpenFailed(format!("swscale setup: {e}")))?;

        let audio_stream = ictx.streams().best(ff::media::Type::Audio);
        let (audio_index, audio_decoder, resampler, audio_tb) = match audio_stream {
            Some(s) => {
                let idx = s.index();
                let tb = s.time_base();
                let mut dec = ff::codec::context::Context::from_parameters(s.parameters())
                    .map_err(|e| CaptureError::OpenFailed(format!("audio codec params: {e}")))?
                    .decoder()
                    .audio()
                    .map_err(|e| CaptureError::OpenFailed(format!("audio decoder open: {e}")))?;
                // Normalize: some decoders need an explicit channel layout.
                if dec.channel_layout().channels() == 0 {
                    let _ = dec.set_channel_layout(ff::channel_layout::ChannelLayout::STEREO);
                }
                let rs = ff::software::resampling::Context::get(
                    dec.format(),
                    dec.channel_layout(),
                    dec.rate(),
                    ff::util::format::sample::Sample::F32(ff::util::format::sample::Type::Packed),
                    ff::channel_layout::ChannelLayout::STEREO,
                    super::media::AUDIO_SAMPLE_RATE,
                )
                .map_err(|e| CaptureError::OpenFailed(format!("swresample setup: {e}")))?;
                (
                    Some(idx),
                    Some(dec),
                    Some(rs),
                    (tb.numerator(), tb.denominator()),
                )
            }
            None => (None, None, None, (1, 1)),
        };
        Ok(Self {
            ictx,
            video_stream: video_index,
            audio_stream: audio_index,
            video_decoder,
            audio_decoder,
            scaler,
            resampler,
            video_tb,
            audio_tb,
            video_queue: VecDeque::with_capacity(4),
            audio_queue: VecDeque::with_capacity(8),
            width: w,
            height: h,
            fps_num,
            fps_den,
            video_frames_out: 0,
            audio_samples_out: 0,
            nominal_frame_us,
            packets_exhausted: false,
            eof_sent: false,
            live: false,
        })
    }

    /// Mark this demuxer as a live source: the video queue is capped at
    /// one frame (newest wins), preventing unbounded memory growth when
    /// the producer outpaces the consumer.
    pub(crate) fn set_live(&mut self, live: bool) {
        self.live = live;
        if live {
            // Drop any backlog accumulated before the flag was set.
            while self.video_queue.len() > 1 {
                self.video_queue.pop_front();
            }
        }
    }

    pub(crate) fn stream_info(&self, has_audio: bool) -> StreamInfo {
        StreamInfo {
            width: self.width,
            height: self.height,
            fps_num: self.fps_num,
            fps_den: self.fps_den,
            has_audio: has_audio && self.audio_stream.is_some(),
        }
    }

    /// Pull the next video frame. Returns `Ok(None)` only when the input
    /// is a live device with nothing ready yet (files always produce or
    /// hit EOF).
    pub(crate) fn next_video(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        if let Some(f) = self.video_queue.pop_front() {
            return Ok(Some(f));
        }
        loop {
            if self.packets_exhausted {
                if !self.eof_sent {
                    self.eof_sent = true;
                    let _ = self.video_decoder.send_eof();
                    if let Some(ref mut ad) = self.audio_decoder {
                        let _ = ad.send_eof();
                    }
                }
                // Drain decoders.
                if let Some(f) = self.drain_video()? {
                    return Ok(Some(f));
                }
                // Audio drain happens in next_audio; here just finish.
                self.drain_audio()?;
                if self.video_queue.is_empty() {
                    return Err(CaptureError::EndOfStream);
                }
                return Ok(self.video_queue.pop_front());
            }
            let Some((stream, packet)) = self.ictx.packets().next() else {
                // Live sources: a `None` here is a transient read timeout,
                // NOT end-of-stream. Return "no frame yet" without marking
                // the stream exhausted, so the next poll tries again.
                if self.live {
                    return Ok(None);
                }
                self.packets_exhausted = true;
                continue;
            };
            if stream.index() == self.video_stream {
                self.video_decoder
                    .send_packet(&packet)
                    .map_err(|e| CaptureError::ReadFailed(format!("video send_packet: {e}")))?;
                while let Some(f) = self.decode_one_video()? {
                    if self.live {
                        // Live: keep only the newest frame.
                        self.video_queue.clear();
                    } else {
                        // File: drop the oldest when at the cap — a slow
                        // consumer must never accumulate unbounded memory.
                        while self.video_queue.len() >= MAX_VIDEO_QUEUE {
                            self.video_queue.pop_front();
                        }
                    }
                    self.video_queue.push_back(f);
                }
                if let Some(f) = self.video_queue.pop_front() {
                    return Ok(Some(f));
                }
            } else if Some(stream.index()) == self.audio_stream {
                if let Some(ref mut ad) = self.audio_decoder {
                    ad.send_packet(&packet)
                        .map_err(|e| CaptureError::ReadFailed(format!("audio send_packet: {e}")))?;
                    self.pump_audio()?;
                }
            }
        }
    }

    /// Live-source newest-frame drain: reads packets until the caller's
    /// interrupt deadline fires, **discarding video packets without
    /// decoding**, then decodes only the newest video packet.
    ///
    /// Decoding every queued frame just to throw all but one away is what
    /// melts the media tick on 1080p60+ capture devices (each MJPEG decode
    /// costs milliseconds; the old drain decoded up to 4 per tick while
    /// delivering 1). Packet reads are cheap; a single decode bounds the
    /// per-tick work no matter how far behind the queue got — the backlog
    /// is dropped, never caught up frame-by-frame.
    ///
    /// Audio packets met along the way are fed to the audio decoder (via
    /// [`Self::pump_audio`]) so A/V stays in sync; they are not dropped.
    /// `Ok(None)` = no video packet arrived before the deadline.
    /// The caller sets the interrupt deadline before calling; on a live
    /// source `packets().next()` returns `None` at the deadline (transient,
    /// not EOF).
    pub(crate) fn next_video_newest(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        debug_assert!(self.live, "next_video_newest is for live sources");
        let mut newest_packet: Option<ff::Packet> = None;
        loop {
            let Some((stream, packet)) = self.ictx.packets().next() else {
                break;
            };
            if stream.index() == self.video_stream {
                // Drop the previous video packet without decoding it:
                // only the newest frame will ever be shown.
                newest_packet = Some(packet);
            } else if Some(stream.index()) == self.audio_stream {
                if let Some(ref mut ad) = self.audio_decoder {
                    ad.send_packet(&packet)
                        .map_err(|e| CaptureError::ReadFailed(format!("audio send_packet: {e}")))?;
                    self.pump_audio()?;
                }
            }
        }
        let Some(packet) = newest_packet else {
            return Ok(None);
        };
        self.video_decoder
            .send_packet(&packet)
            .map_err(|e| CaptureError::ReadFailed(format!("video send_packet: {e}")))?;
        // One packet yields one frame for MJPEG-like live formats; take
        // the first decoded frame.
        match self.decode_one_video()? {
            Some(f) => Ok(Some(f)),
            None => Ok(None),
        }
    }

    /// Pull the next audio chunk (48000 Hz stereo interleaved f32).
    /// `Ok(None)` = no audio stream, or EOF reached on the audio side.
    pub(crate) fn next_audio(&mut self) -> Result<Option<RawAudio>, CaptureError> {
        if self.audio_stream.is_none() {
            return Ok(None);
        }
        if let Some(a) = self.audio_queue.pop_front() {
            return Ok(Some(a));
        }
        loop {
            if self.packets_exhausted {
                if !self.eof_sent {
                    self.eof_sent = true;
                    let _ = self.video_decoder.send_eof();
                    if let Some(ref mut ad) = self.audio_decoder {
                        let _ = ad.send_eof();
                    }
                }
                self.drain_audio()?;
                return Ok(self.audio_queue.pop_front());
            }
            let Some((stream, packet)) = self.ictx.packets().next() else {
                // Live sources: transient timeout, not EOF (see next_video).
                if self.live {
                    return Ok(None);
                }
                self.packets_exhausted = true;
                continue;
            };
            if Some(stream.index()) == self.audio_stream {
                if let Some(ref mut ad) = self.audio_decoder {
                    ad.send_packet(&packet)
                        .map_err(|e| CaptureError::ReadFailed(format!("audio send_packet: {e}")))?;
                    self.pump_audio()?;
                    if let Some(a) = self.audio_queue.pop_front() {
                        return Ok(Some(a));
                    }
                }
            } else if stream.index() == self.video_stream {
                // Seeking audio past video packets: feed the video decoder
                // (keeps its reference state correct) but DISCARD the decoded
                // frames — no RGBA scale, no 8 MB allocation, no queue
                // growth. The old code decoded+scaled+queued every skipped
                // video frame here: ~25 MB/tick of queue growth at 1080p →
                // 90% CPU, then OOM crash when adding a media file.
                self.video_decoder
                    .send_packet(&packet)
                    .map_err(|e| CaptureError::ReadFailed(format!("video send_packet: {e}")))?;
                while self.discard_one_video()? {}
            }
        }
    }

    /// Decode one video frame and discard it (no RGBA scale, no queue).
    /// Used when skipping over video packets while seeking audio.
    /// Returns `true` when a frame was discarded.
    fn discard_one_video(&mut self) -> Result<bool, CaptureError> {
        let mut decoded = ff::util::frame::video::Video::empty();
        match self.video_decoder.receive_frame(&mut decoded) {
            Ok(()) => Ok(true),
            // EAGAIN (errno 11): need more packets. EOF: decoder drained.
            Err(ff::util::error::Error::Eof) => Ok(false),
            Err(ff::util::error::Error::Other { errno: 11 }) => Ok(false),
            Err(e) => Err(CaptureError::ReadFailed(format!(
                "video receive_frame: {e}"
            ))),
        }
    }

    fn decode_one_video(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        let mut decoded = ff::util::frame::video::Video::empty();
        match self.video_decoder.receive_frame(&mut decoded) {
            Ok(()) => {}
            // EAGAIN (errno 11): need more packets. EOF: decoder drained.
            Err(ff::util::error::Error::Eof) => return Ok(None),
            Err(ff::util::error::Error::Other { errno: 11 }) => return Ok(None),
            Err(e) => {
                return Err(CaptureError::ReadFailed(format!(
                    "video receive_frame: {e}"
                )))
            }
        }
        let mut rgba = ff::util::frame::video::Video::empty();
        self.scaler
            .run(&decoded, &mut rgba)
            .map_err(|e| CaptureError::ReadFailed(format!("swscale run: {e}")))?;
        let fallback = self.video_frames_out as i64 * self.nominal_frame_us as i64;
        let pts_us = pts_to_us(decoded.pts(), self.video_tb.0, self.video_tb.1, fallback);
        self.video_frames_out += 1;
        let stride = rgba.stride(0);
        let w4 = self.width as usize * 4;
        let mut data = vec![0u8; w4 * self.height as usize];
        let src = rgba.data(0);
        for y in 0..self.height as usize {
            let s = y * stride;
            let d = y * w4;
            data[d..d + w4].copy_from_slice(&src[s..s + w4]);
        }
        Ok(Some(CapturedFrame {
            data,
            width: self.width,
            height: self.height,
            pts_us,
            duration_us: self.nominal_frame_us,
        }))
    }

    fn drain_video(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        // Bounded: at most a few frames per call, so reaching EOF can't
        // burst-decode a whole file into RAM in one go.
        for _ in 0..MAX_DRAIN_PER_CALL {
            match self.decode_one_video()? {
                Some(f) => {
                    if !self.live {
                        while self.video_queue.len() >= MAX_VIDEO_QUEUE {
                            self.video_queue.pop_front();
                        }
                    }
                    self.video_queue.push_back(f);
                }
                None => break,
            }
        }
        Ok(self.video_queue.pop_front())
    }

    fn pump_audio(&mut self) -> Result<(), CaptureError> {
        let ad = self.audio_decoder.as_mut().expect("audio decoder present");
        let rs = self.resampler.as_mut().expect("resampler present");
        loop {
            let mut decoded = ff::util::frame::audio::Audio::empty();
            match ad.receive_frame(&mut decoded) {
                Ok(()) => {}
                // EAGAIN (errno 11): need more packets. EOF: decoder drained.
                Err(ff::util::error::Error::Eof) => break,
                Err(ff::util::error::Error::Other { errno: 11 }) => break,
                Err(e) => {
                    return Err(CaptureError::ReadFailed(format!(
                        "audio receive_frame: {e}"
                    )))
                }
            }
            // Upper bound on resampled output samples for this input frame.
            let max_out = decoded.samples() as usize * 2 + 1024;
            let mut out = ff::util::frame::audio::Audio::new(
                ff::util::format::sample::Sample::F32(ff::util::format::sample::Type::Packed),
                max_out,
                ff::channel_layout::ChannelLayout::STEREO,
            );
            let delay = rs
                .run(&decoded, &mut out)
                .map_err(|e| CaptureError::ReadFailed(format!("swresample run: {e}")))?;
            let _ = delay;
            let n = out.samples() as usize * super::media::AUDIO_CHANNELS as usize;
            let bytes = out.data(0);
            let floats: &[f32] = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr() as *const f32, n.min(bytes.len() / 4))
            };
            // PTS fallback: running sample count → µs at the normalized rate.
            let fallback_us =
                self.audio_samples_out * 1_000_000 / super::media::AUDIO_SAMPLE_RATE as u64;
            let pts_us = pts_to_us(
                decoded.pts(),
                self.audio_tb.0,
                self.audio_tb.1,
                fallback_us as i64,
            );
            self.audio_samples_out += out.samples() as u64;
            self.audio_queue.push_back(RawAudio {
                samples: floats.to_vec(),
                pts_us,
            });
        }
        Ok(())
    }

    fn drain_audio(&mut self) -> Result<(), CaptureError> {
        self.pump_audio()
    }
}

#[cfg(test)]
mod queue_bounds_tests {
    use super::*;
    use std::process::Command;

    fn make_av_file() -> String {
        let path = std::env::temp_dir()
            .join("sf-demux-queue-test.mp4")
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&path);
        let st = Command::new("ffmpeg")
            .args([
                "-y", "-v", "error",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30:duration=5",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
                "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest",
                path.as_str(),
            ])
            .status()
            .expect("ffmpeg CLI must be installed");
        assert!(st.success());
        path
    }

    #[test]
    fn audio_seeking_never_accumulates_video_frames() {
        ensure_init();
        let path = make_av_file();
        let ictx = ff::format::input(&std::path::PathBuf::from(&path)).expect("open");
        let mut d = DemuxDecoder::from_input(ictx, 33_333).expect("demux");
        // Simulate the media tick: one video pull + 4 audio drains per tick,
        // like `Shell::av_tick` does. The old code decoded+queued every
        // video packet skipped while seeking audio (~25 MB/tick at 1080p →
        // 90% CPU then OOM crash).
        for _ in 0..150 {
            let _ = d.next_video();
            for _ in 0..4 {
                let _ = d.next_audio();
            }
            assert!(
                d.video_queue.len() <= super::MAX_VIDEO_QUEUE,
                "video queue grew to {} (unbounded accumulation)",
                d.video_queue.len()
            );
        }
    }
}
