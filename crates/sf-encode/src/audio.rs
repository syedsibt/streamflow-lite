//! AAC audio encoder: interleaved stereo f32 PCM in, [`EncodedPacket`]s out.
//!
//! Input is 48 kHz stereo interleaved `f32`. Samples are buffered into the
//! AAC frame size (1024) and encoded as planar float. Packet microsecond
//! timestamps are computed cumulatively (`frame_index * 1024 * 1e6 / 48000`)
//! so there is no per-packet rounding drift.

use super::{EncodeError, EncodedPacket};
use ffmpeg_next as ff;

/// Sample rate expected by [`AudioEncoder`].
pub const SAMPLE_RATE: u32 = 48_000;
/// Channel count expected by [`AudioEncoder`].
pub const CHANNELS: u32 = 2;

/// AAC audio encoder (native FFmpeg AAC, 48 kHz stereo).
pub struct AudioEncoder {
    encoder: ff::codec::encoder::Audio,
    frame_size: usize,
    /// Accumulated interleaved stereo samples not yet forming a full frame.
    pending: Vec<f32>,
    next_pts_samples: i64,
    /// Packets emitted so far; drives the drift-free µs clock. Counting
    /// packets (not drains) is what makes this correct: the AAC encoder has
    /// a frame of delay, so the first drain() yields zero packets and a later
    /// drain yields two — assigning one timestamp per drain would duplicate.
    packets_emitted: u64,
    pts_offset_us: Option<i64>,
    bitrate_bps: u32,
}

impl AudioEncoder {
    /// Open the AAC encoder: 48 kHz stereo, 128 kbit/s.
    pub fn open() -> Result<Self, EncodeError> {
        Self::open_with_bitrate(128_000)
    }

    /// Open the AAC encoder with an explicit bitrate.
    pub fn open_with_bitrate(bitrate_bps: u32) -> Result<Self, EncodeError> {
        ff::init().map_err(|e| EncodeError::Init(format!("{e}")))?;

        let codec = ff::encoder::find_by_name("aac")
            .ok_or_else(|| EncodeError::EncoderNotFound("aac".to_string()))?;
        // Allocate against the codec (see `context_with_codec` docs).
        // SAFETY: `codec` is a live registry codec.
        let context = unsafe { crate::context_with_codec(&codec)? };
        let mut a = context
            .encoder()
            .audio()
            .map_err(|e| EncodeError::Open(format!("audio encoder context: {e}")))?;

        a.set_rate(SAMPLE_RATE as i32);
        a.set_channels(CHANNELS as i32);
        a.set_channel_layout(ff::util::channel_layout::ChannelLayout::STEREO);
        a.set_format(ff::util::format::sample::Sample::F32(
            ff::util::format::sample::Type::Planar,
        ));
        a.set_time_base(ff::Rational(1, SAMPLE_RATE as i32));
        // SPS/PPS-equivalent (AudioSpecificConfig) in extradata for muxers.
        a.set_flags(ff::codec::flag::Flags::GLOBAL_HEADER);

        let mut dict = ff::Dictionary::new();
        dict.set("b", &bitrate_bps.to_string());
        let encoder = a
            .open_with(dict)
            .map_err(|e| EncodeError::Open(format!("aac: {e}")))?;
        let frame_size = encoder.frame_size() as usize;

        Ok(Self {
            encoder,
            frame_size,
            pending: Vec::new(),
            next_pts_samples: 0,
            packets_emitted: 0,
            pts_offset_us: None,
            bitrate_bps,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    pub fn channels(&self) -> u32 {
        CHANNELS
    }

    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// Codec parameters (including AudioSpecificConfig extradata) for muxer
    /// stream setup and decoder init.
    pub fn parameters(&self) -> ff::codec::parameters::Parameters {
        ff::codec::parameters::Parameters::from(&self.encoder)
    }

    /// Encode interleaved stereo f32 samples at 48 kHz.
    ///
    /// `samples.len()` must be even (LRLR…); any count is accepted — partial
    /// frames are buffered internally. `pts_us` anchors the first call on the
    /// caller's clock.
    pub fn encode_pcm(
        &mut self,
        samples: &[f32],
        pts_us: i64,
    ) -> Result<Vec<EncodedPacket>, EncodeError> {
        if samples.len() % 2 != 0 {
            return Err(EncodeError::InvalidInput(
                "interleaved stereo sample count must be even".into(),
            ));
        }
        if self.pts_offset_us.is_none() {
            self.pts_offset_us = Some(pts_us);
        }
        self.pending.extend_from_slice(samples);

        let mut out = Vec::new();
        while self.pending.len() >= self.frame_size * 2 {
            let frame: Vec<f32> = self.pending.drain(..self.frame_size * 2).collect();
            out.extend(self.encode_frame(&frame)?);
        }
        Ok(out)
    }

    /// Flush: encode the buffered tail (zero-padded) and drain the encoder.
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let mut frame = std::mem::take(&mut self.pending);
            frame.resize(self.frame_size * 2, 0.0);
            out.extend(self.encode_frame(&frame)?);
        }
        self.encoder
            .send_eof()
            .map_err(|e| EncodeError::Encode(format!("send_eof: {e}")))?;
        out.extend(self.drain()?);
        Ok(out)
    }

    fn encode_frame(&mut self, interleaved: &[f32]) -> Result<Vec<EncodedPacket>, EncodeError> {
        debug_assert_eq!(interleaved.len(), self.frame_size * 2);
        let mut frame = ff::util::frame::Audio::new(
            ff::util::format::sample::Sample::F32(ff::util::format::sample::Type::Planar),
            self.frame_size,
            ff::util::channel_layout::ChannelLayout::STEREO,
        );
        {
            let left = frame.plane_mut::<f32>(0);
            for (i, s) in left.iter_mut().enumerate() {
                *s = interleaved[2 * i];
            }
        }
        {
            let right = frame.plane_mut::<f32>(1);
            for (i, s) in right.iter_mut().enumerate() {
                *s = interleaved[2 * i + 1];
            }
        }
        frame.set_pts(Some(self.next_pts_samples));
        self.encoder
            .send_frame(&frame)
            .map_err(|e| EncodeError::Encode(format!("send_frame: {e}")))?;
        self.next_pts_samples += self.frame_size as i64;
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        use ff::util::error::{Error, EAGAIN};
        let offset = self.pts_offset_us.unwrap_or(0);
        let mut out = Vec::new();
        loop {
            let mut pkt = ff::codec::packet::Packet::empty();
            match self.encoder.receive_packet(&mut pkt) {
                Ok(()) => {
                    // Drift-free cumulative µs clock, one step per packet:
                    // packet k ↔ frame k (AAC has no B-frames; the encoder's
                    // initial delay only shifts *when* packets emerge).
                    let pts_us = offset
                        + (self.packets_emitted * self.frame_size as u64 * 1_000_000
                            / SAMPLE_RATE as u64) as i64;
                    self.packets_emitted += 1;
                    out.push(EncodedPacket {
                        data: pkt.data().map(|d| d.to_vec()).unwrap_or_default(),
                        pts_us,
                        dts_us: pts_us,
                        is_keyframe: true,
                        stream_index: 1,
                    });
                }
                Err(Error::Eof) => break,
                Err(Error::Other { errno }) if errno == EAGAIN => break,
                Err(e) => return Err(EncodeError::Encode(format!("receive_packet: {e}"))),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 440 Hz sine, stereo interleaved, amplitude 0.5.
    fn sine_440(seconds: f64) -> Vec<f32> {
        let n = (seconds * SAMPLE_RATE as f64) as usize;
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (0.5
                * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / SAMPLE_RATE as f64).sin())
                as f32;
            out.push(s);
            out.push(s);
        }
        out
    }

    fn decode_all(
        params: &ff::codec::parameters::Parameters,
        packets: &[EncodedPacket],
    ) -> Vec<ff::util::frame::Audio> {
        let context =
            ff::codec::context::Context::from_parameters(params.clone()).expect("decoder ctx");
        let mut decoder = context.decoder().audio().expect("open audio decoder");
        let mut frames = Vec::new();
        for p in packets {
            let pkt = ff::codec::packet::Packet::copy(&p.data);
            decoder.send_packet(&pkt).expect("send_packet");
            let mut frame = ff::util::frame::Audio::empty();
            while decoder.receive_frame(&mut frame).is_ok() {
                frames.push(frame.clone());
                frame = ff::util::frame::Audio::empty();
            }
        }
        decoder.send_eof().expect("decoder eof");
        let mut frame = ff::util::frame::Audio::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            frames.push(frame.clone());
            frame = ff::util::frame::Audio::empty();
        }
        frames
    }

    #[test]
    fn aac_roundtrip_two_seconds() {
        let mut enc = AudioEncoder::open().expect("open aac");
        assert_eq!(enc.frame_size(), 1024);

        let pcm = sine_440(2.0);
        assert_eq!(pcm.len(), 2 * 48_000 * 2);

        // Feed in odd-sized chunks to exercise the internal buffering.
        let t0 = std::time::Instant::now();
        let mut packets = Vec::new();
        for (ci, chunk) in pcm.chunks(5000).enumerate() {
            let pts_us = ci as i64 * 5000 / 2 * 1_000_000 / SAMPLE_RATE as i64;
            packets.extend(enc.encode_pcm(chunk, pts_us).expect("encode_pcm"));
        }
        packets.extend(enc.flush().expect("flush"));
        let elapsed = t0.elapsed();
        eprintln!(
            "audio encode: 2 s stereo 48 kHz in {:.3}s",
            elapsed.as_secs_f64()
        );

        assert!(!packets.is_empty());
        assert!(packets.iter().all(|p| p.stream_index == 1));
        // PTS strictly increasing (drift-free cumulative clock).
        let mut prev = -1i64;
        for p in &packets {
            assert!(
                p.pts_us > prev,
                "audio pts not monotonic: {} <= {prev}",
                p.pts_us
            );
            prev = p.pts_us;
        }
        // First packet starts at the anchor (0), last is near 2 s.
        assert_eq!(packets[0].pts_us, 0);
        let last = packets.last().unwrap().pts_us;
        assert!(
            (1_900_000..2_100_000).contains(&last),
            "last audio pts_us {last} not near 2 s"
        );

        let params = enc.parameters();
        let frames = decode_all(&params, &packets);
        let total: usize = frames.iter().map(|f| f.samples()).sum();
        eprintln!("decoded {total} samples per channel");
        // 96000 source samples; AAC pads to 1024-sample frames and has ~1024
        // samples of encoder delay. Tolerance: ±2 frames.
        assert!(
            (96_000 - 2048..=96_000 + 2048).contains(&total),
            "decoded sample count {total} out of tolerance"
        );

        // Sanity: decoded audio actually resembles the 440 Hz tone
        // (source RMS for amplitude 0.5 is ~0.354).
        let mut se = 0.0f64;
        let mut n = 0u64;
        for f in &frames {
            for ch in 0..2 {
                for s in f.plane::<f32>(ch) {
                    se += (*s as f64) * (*s as f64);
                    n += 1;
                }
            }
        }
        let rms = (se / n as f64).sqrt();
        eprintln!("decoded RMS: {rms:.3}");
        assert!(rms > 0.15 && rms < 0.5, "decoded RMS {rms:.3} implausible");
    }

    #[test]
    fn rejects_odd_sample_count() {
        let mut enc = AudioEncoder::open().expect("open");
        let err = enc.encode_pcm(&[0.0; 7], 0).unwrap_err();
        assert!(matches!(err, EncodeError::InvalidInput(_)));
    }
}
