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
    resampler: super::audio_norm::AudioNormalizer,
    audio_tb: (i32, i32),
    queue: VecDeque<DecodedAudio>,
    samples_out: u64,
    device_name: String,
    clock: ClockMapper,
    /// The input ended; queued chunks are still handed out first.
    ended: bool,
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
        // Normalised per decoded frame (see `audio_norm`): never trust the
        // header's channel layout for a live device.
        let resampler = super::audio_norm::AudioNormalizer::default();
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
            ended: false,
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
        if self.ended {
            return Err(CaptureError::EndOfStream);
        }
        for _ in 0..MAX_PACKETS_PER_CALL {
            let mut pkt = ff::Packet::empty();
            match pkt.read(&mut self.ictx) {
                Ok(()) => {}
                Err(Error::Other { errno }) if errno == EAGAIN => break,
                Err(Error::Exit) => break,
                Err(Error::Eof) => {
                    self.ended = true;
                    break;
                }
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
        match self.queue.pop_front() {
            None if self.ended => Err(CaptureError::EndOfStream),
            c => Ok(c),
        }
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
            let floats = self.resampler.convert(&decoded)?;
            let frames_out = (floats.len() / AUDIO_CHANNELS as usize) as u64;
            let pts_us = pts_to_us(
                decoded.pts(),
                self.audio_tb.0,
                self.audio_tb.1,
                (self.samples_out * 1_000_000 / AUDIO_SAMPLE_RATE as u64) as i64,
            );
            self.samples_out += frames_out;
            let captured_at_us = self.clock.map(pts_us, read_at);
            if self.queue.len() >= MAX_QUEUE {
                self.queue.pop_front();
            }
            self.queue.push_back(DecodedAudio {
                samples: floats,
                channels: AUDIO_CHANNELS,
                sample_rate: AUDIO_SAMPLE_RATE,
                pts_us,
                captured_at_us,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw PCM with only a channel count (no layout) — what a DirectShow
    /// microphone delivers. Every chunk must normalize to 48 kHz stereo.
    fn decode_raw(channels: u32, rate: u32, fmt: &str) -> Result<Vec<f32>, CaptureError> {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sf-audio-{channels}ch-{rate}-{fmt}.raw"));
        let st = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", &format!("sine=frequency=440:sample_rate={rate}:duration=1")])
            .args(["-ac", &channels.to_string(), "-f", fmt])
            .arg(&path)
            .status()
            .expect("ffmpeg");
        assert!(st.success());
        super::super::demux::ensure_init();
        let ch = channels.to_string();
        let sr = rate.to_string();
        let ictx = super::super::live::open_input(
            Some(fmt),
            path.to_str().unwrap(),
            // Channel count only, no layout — like a dshow microphone.
            &[("channels", ch.as_str()), ("sample_rate", sr.as_str())],
            std::time::Duration::from_secs(5),
        )?;
        let mut p = AudioInputProducer::from_input(ictx, "test".into())?;
        let mut all: Vec<f32> = Vec::new();
        loop {
            match p.next_audio() {
                Ok(Some(c)) => {
                    assert_eq!(c.channels, 2);
                    assert_eq!(c.sample_rate, AUDIO_SAMPLE_RATE);
                    all.extend_from_slice(&c.samples);
                }
                Ok(None) => continue,
                Err(CaptureError::EndOfStream) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(all)
    }

    #[test]
    fn mono_and_stereo_mics_normalize() {
        for (ch, rate, fmt) in [(1, 48000, "s16le"), (1, 44100, "s16le"), (2, 44100, "s16le"), (1, 16000, "f32le")] {
            let pcm = decode_raw(ch, rate, fmt).unwrap_or_else(|e| panic!("{ch} ch {rate} Hz {fmt}: {e}"));
            let frames = pcm.len() / 2;
            assert!(frames > 4000, "{ch} ch {rate} Hz {fmt}: only {frames} frames");
            let left: Vec<f32> = pcm.iter().step_by(2).copied().collect();
            let right: Vec<f32> = pcm.iter().skip(1).step_by(2).copied().collect();
            let peak = left.iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!(peak > 0.05, "{ch} ch {rate} Hz {fmt}: silent (peak {peak})");
            let diff = left.iter().zip(&right).fold(0f32, |m, (l, r)| m.max((l - r).abs()));
            assert!(diff < 1e-3, "mono/stereo source must reach both channels equally");
            // 440 Hz at 48 kHz: 880 zero crossings per second.
            let zc = left.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f64;
            let hz = zc / 2.0 / (frames as f64 / 48_000.0);
            assert!((hz - 440.0).abs() < 15.0, "{ch} ch {rate} Hz {fmt}: pitch {hz:.0} Hz after resampling");
        }
    }
}
