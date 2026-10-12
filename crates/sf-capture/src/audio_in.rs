//! Windows microphone / line-in / capture-card audio via FFmpeg's `dshow`
//! audio device.
//!
//! # OBS parity
//! OBS captures audio through WASAPI on a dedicated thread and stamps each
//! buffer with its capture time. We reach the same physical devices
//! through FFmpeg's dshow audio input (`audio="Device Name"`), normalize to
//! the engine's 48 kHz stereo f32, and stamp every chunk with its capture
//! time on the pipeline clock ([`crate::clock`]), so the mixer can align
//! audio with video captured at the same moment.
//!
//! # Realtime rules
//! * Reads are **non-blocking** (`AVFMT_FLAG_NONBLOCK`): dshow ignores the
//!   interrupt callback, so the previous blocking read could hang its thread
//!   forever on an unplugged device (and the old `packets()` loop could
//!   never time out). [`AudioInputProducer::next_audio`] returns `Ok(None)`
//!   immediately when the device has nothing queued.
//! * DirectShow's default capture buffer is ~500 ms; `audio_buffer_size=20`
//!   requests 20 ms packets so each word arrives promptly.
//! * The internal queue is bounded (newest wins).
//!
//! # Status
//! Code-complete; **hardware-unverified** here (no Windows audio device on
//! the dev machines). Non-blocking read semantics are unit-tested on Linux
//! with a live network input (same FFmpeg read path).

use std::collections::VecDeque;
#[cfg(any(windows, sf_wincheck))]
use std::time::Duration;

use ffmpeg_next as ff;

use super::clock::{now_us, ClockMapper};
use super::demux::pts_to_us;
use super::media::{DecodedAudio, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use super::CaptureError;

/// Max queued audio chunks (~170 ms of 20 ms packets), newest wins.
const MAX_QUEUE: usize = 8;
/// Opening a missing/unplugged device must fail fast, not hang.
#[cfg(any(windows, sf_wincheck))]
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on packets consumed per call.
const MAX_PACKETS_PER_CALL: usize = 64;

/// Live audio input producer (Windows dshow), non-blocking.
pub struct AudioInputProducer {
    ictx: ff::format::context::Input,
    audio_stream: usize,
    decoder: ff::decoder::Audio,
    resampler: ff::software::resampling::Context,
    audio_tb: (i32, i32),
    queue: VecDeque<DecodedAudio>,
    samples_out: u64,
    device_name: String,
    clock: ClockMapper,
}

// SAFETY: FFmpeg contexts have no thread affinity; the producer is owned
// by exactly one capture thread at a time.
unsafe impl Send for AudioInputProducer {}

impl AudioInputProducer {
    /// Open the named dshow audio device (`list_audio_devices` names).
    /// `None` picks the first available device.
    #[cfg(any(windows, sf_wincheck))]
    pub fn open(device: Option<&str>) -> Result<Self, CaptureError> {
        super::demux::ensure_init();
        let name = match device {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => super::dshow::list_audio_devices()?
                .into_iter()
                .next()
                .ok_or_else(|| CaptureError::OpenFailed("no audio input devices found".into()))?,
        };
        let url = format!("audio={name}");
        let mut ictx = super::live::open_input(
            Some("dshow"),
            &url,
            // 20 ms device packets instead of DirectShow's ~500 ms default.
            &[("audio_buffer_size", "20")],
            OPEN_TIMEOUT,
        )
        .map_err(|e| CaptureError::OpenFailed(format!("cannot open audio device '{name}': {e}")))?;
        unsafe {
            (*ictx.as_mut_ptr()).flags |= ff::ffi::AVFMT_FLAG_NONBLOCK;
        }
        Self::from_input(ictx, name)
    }

    /// Build from an already-open live input (any FFmpeg input with an
    /// audio stream). Used by [`Self::open`] and by tests.
    pub fn from_input(
        mut ictx: ff::format::context::Input,
        device_name: String,
    ) -> Result<Self, CaptureError> {
        unsafe {
            (*ictx.as_mut_ptr()).flags |= ff::ffi::AVFMT_FLAG_NONBLOCK;
        }
        let audio_stream = ictx
            .streams()
            .best(ff::media::Type::Audio)
            .ok_or_else(|| CaptureError::OpenFailed("audio device has no audio stream".into()))?;
        let audio_index = audio_stream.index();
        let tb = audio_stream.time_base();
        let mut decoder = ff::codec::context::Context::from_parameters(audio_stream.parameters())
            .map_err(|e| CaptureError::OpenFailed(format!("audio codec params: {e}")))?
            .decoder()
            .audio()
            .map_err(|e| CaptureError::OpenFailed(format!("audio decoder open: {e}")))?;
        if decoder.channel_layout().channels() == 0 {
            let _ = decoder.set_channel_layout(ff::channel_layout::ChannelLayout::STEREO);
        }
        let resampler = ff::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            ff::util::format::sample::Sample::F32(ff::util::format::sample::Type::Packed),
            ff::channel_layout::ChannelLayout::STEREO,
            AUDIO_SAMPLE_RATE,
        )
        .map_err(|e| CaptureError::OpenFailed(format!("swresample setup: {e}")))?;
        Ok(Self {
            ictx,
            audio_stream: audio_index,
            decoder,
            resampler,
            audio_tb: (tb.numerator(), tb.denominator()),
            queue: VecDeque::with_capacity(MAX_QUEUE),
            samples_out: 0,
            device_name,
            clock: ClockMapper::new(),
        })
    }

    /// Device name, for diagnostics.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Next normalized chunk (48 kHz stereo f32) with its capture time.
    /// Never blocks: `Ok(None)` when the device has nothing queued yet.
    /// `Err(EndOfStream)` when the device ended (unplugged).
    pub fn next_audio(&mut self) -> Result<Option<DecodedAudio>, CaptureError> {
        use ff::util::error::{Error, EAGAIN};
        if let Some(c) = self.queue.pop_front() {
            return Ok(Some(c));
        }
        for _ in 0..MAX_PACKETS_PER_CALL {
            let mut pkt = ff::Packet::empty();
            match pkt.read(&mut self.ictx) {
                Ok(()) => {}
                Err(Error::Other { errno }) if errno == EAGAIN => break,
                Err(Error::Exit) => break,
                Err(Error::Eof) => return Err(CaptureError::EndOfStream),
                Err(e) => return Err(CaptureError::ReadFailed(format!("audio read: {e}"))),
            }
            if pkt.stream() != self.audio_stream {
                continue;
            }
            let read_at = now_us();
            self.decoder
                .send_packet(&pkt)
                .map_err(|e| CaptureError::ReadFailed(format!("audio send_packet: {e}")))?;
            self.drain_decoder(read_at)?;
        }
        Ok(self.queue.pop_front())
    }

    fn drain_decoder(&mut self, read_at: i64) -> Result<(), CaptureError> {
        loop {
            let mut decoded = ff::util::frame::audio::Audio::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(ff::util::error::Error::Eof) => break,
                Err(ff::util::error::Error::Other { errno: 11 }) => break,
                Err(e) => {
                    return Err(CaptureError::ReadFailed(format!(
                        "audio receive_frame: {e}"
                    )))
                }
            }
            let max_out = decoded.samples() as usize * 2 + 1024;
            let mut out = ff::util::frame::audio::Audio::new(
                ff::util::format::sample::Sample::F32(ff::util::format::sample::Type::Packed),
                max_out,
                ff::channel_layout::ChannelLayout::STEREO,
            );
            self.resampler
                .run(&decoded, &mut out)
                .map_err(|e| CaptureError::ReadFailed(format!("swresample run: {e}")))?;
            let n = out.samples() as usize * AUDIO_CHANNELS as usize;
            let bytes = out.data(0);
            let floats: &[f32] = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr() as *const f32, n.min(bytes.len() / 4))
            };
            let pts_us = pts_to_us(
                decoded.pts(),
                self.audio_tb.0,
                self.audio_tb.1,
                (self.samples_out * 1_000_000 / AUDIO_SAMPLE_RATE as u64) as i64,
            );
            self.samples_out += out.samples() as u64;
            let captured_at_us = self.clock.map(pts_us, read_at);
            if self.queue.len() >= MAX_QUEUE {
                self.queue.pop_front();
            }
            self.queue.push_back(DecodedAudio {
                samples: floats.to_vec(),
                channels: AUDIO_CHANNELS,
                sample_rate: AUDIO_SAMPLE_RATE,
                pts_us,
                captured_at_us,
            });
        }
        Ok(())
    }
}
