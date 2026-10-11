//! H.264 video encoder: RGBA in, [`EncodedPacket`]s out.
//!
//! Frames are converted RGBA → YUV420P with swscale, then fed to the chosen
//! H.264 encoder (NVENC/QSV/AMF on Windows, libx264 software fallback).
//! The encoder runs with `GLOBAL_HEADER` so SPS/PPS live in extradata —
//! required by the MP4/FLV muxers in `sf-output` — and are exposed via
//! [`VideoEncoder::parameters`] for stream setup and decoder init.

use super::{EncodeError, EncodedPacket, EncoderId};
use ffmpeg_next as ff;

/// Options for [`VideoEncoder::open`].
#[derive(Clone)]
pub struct VideoEncoderOpts {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// Target bitrate, bits per second.
    pub bitrate_bps: u32,
    /// Keyframe interval in frames.
    pub gop: u32,
    /// Encoder preset string (e.g. `"veryfast"` for libx264; passed through
    /// to NVENC's `preset` too). Ignored for qsv/amf/mpeg4.
    pub preset: String,
}

/// H.264 video encoder.
pub struct VideoEncoder {
    encoder: ff::codec::encoder::Video,
    scaler: ff::software::scaling::Context,
    src_frame: ff::util::frame::Video,
    dst_frame: ff::util::frame::Video,
    /// Encoder stream time base: (fps_den, fps_num); frame pts = frame index.
    time_base: ff::Rational,
    frame_index: i64,
    /// Microsecond anchor: `packet_pts_us = rescale(encoder_pts) + pts_offset_us`,
    /// where the offset is taken from the first `encode_rgba` call's `pts_us`.
    pts_offset_us: Option<i64>,
    id: EncoderId,
    width: u32,
    height: u32,
}

impl VideoEncoder {
    /// Open with the best encoder available on this machine.
    ///
    /// Tries encoders in priority order (see [`select_video_encoder`]) and
    /// falls back when one fails to open: registry presence does not imply
    /// usability (e.g. `h264_nvenc` is compiled into many FFmpeg builds but
    /// fails to open without an NVIDIA GPU).
    pub fn open(opts: VideoEncoderOpts) -> Result<Self, EncodeError> {
        let _ = ff::init();
        const ORDER: [EncoderId; 5] = [
            EncoderId::H264Nvenc,
            EncoderId::H264Qsv,
            EncoderId::H264Amf,
            EncoderId::H264Libx264,
            EncoderId::Mpeg4,
        ];
        let mut last_err = EncodeError::EncoderNotFound("no encoder in registry".into());
        for id in ORDER {
            if ffmpeg_next::encoder::find_by_name(id.ffmpeg_name()).is_none() {
                continue;
            }
            match Self::open_with_encoder(id, opts.clone()) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Open with an explicitly chosen encoder (used by tests and by the
    /// engine when it wants to force software encoding).
    pub fn open_with_encoder(id: EncoderId, opts: VideoEncoderOpts) -> Result<Self, EncodeError> {
        if opts.width == 0 || opts.height == 0 {
            return Err(EncodeError::InvalidInput(
                "width and height must be non-zero".into(),
            ));
        }
        if opts.fps_num == 0 || opts.fps_den == 0 {
            return Err(EncodeError::InvalidInput(
                "fps_num/fps_den must be non-zero".into(),
            ));
        }

        ff::init().map_err(|e| EncodeError::Init(format!("{e}")))?;

        let codec = ff::encoder::find_by_name(id.ffmpeg_name())
            .ok_or_else(|| EncodeError::EncoderNotFound(id.ffmpeg_name().to_string()))?;
        // Allocate against the codec (see `context_with_codec` docs).
        // SAFETY: `codec` is a live registry codec.
        let context = unsafe { crate::context_with_codec(&codec)? };
        let mut v = context
            .encoder()
            .video()
            .map_err(|e| EncodeError::Open(format!("video encoder context: {e}")))?;

        v.set_width(opts.width);
        v.set_height(opts.height);
        v.set_format(ff::util::format::pixel::Pixel::YUV420P);
        let time_base = ff::Rational(opts.fps_den as i32, opts.fps_num as i32);
        v.set_time_base(time_base);
        v.set_frame_rate(Some(ff::Rational(opts.fps_num as i32, opts.fps_den as i32)));
        v.set_gop(opts.gop);
        v.set_max_b_frames(2);
        // SPS/PPS in extradata (not in-band): required by MP4/FLV muxers.
        v.set_flags(ff::codec::flag::Flags::GLOBAL_HEADER);

        let mut dict = ff::Dictionary::new();
        dict.set("b", &opts.bitrate_bps.to_string());
        match id {
            EncoderId::H264Libx264 | EncoderId::H264Nvenc => {
                dict.set("preset", &opts.preset);
            }
            _ => {}
        }
        // Live streaming: disable encoder buffering so frames are output
        // immediately (otherwise x264 lookahead delays the first packets
        // and YouTube sees "No data").
        if id == EncoderId::H264Libx264 {
            dict.set("tune", "zerolatency");
        }
        let encoder = v
            .open_with(dict)
            .map_err(|e| EncodeError::Open(format!("{}: {e}", id.ffmpeg_name())))?;

        let scaler = ff::software::scaling::Context::get(
            ff::util::format::pixel::Pixel::RGBA,
            opts.width,
            opts.height,
            ff::util::format::pixel::Pixel::YUV420P,
            opts.width,
            opts.height,
            ff::software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| EncodeError::Open(format!("swscale RGBA->YUV420P: {e}")))?;

        let src_frame = ff::util::frame::Video::new(
            ff::util::format::pixel::Pixel::RGBA,
            opts.width,
            opts.height,
        );
        let dst_frame = ff::util::frame::Video::new(
            ff::util::format::pixel::Pixel::YUV420P,
            opts.width,
            opts.height,
        );

        Ok(Self {
            encoder,
            scaler,
            src_frame,
            dst_frame,
            time_base,
            frame_index: 0,
            pts_offset_us: None,
            id,
            width: opts.width,
            height: opts.height,
        })
    }

    /// The encoder actually in use.
    pub fn encoder_id(&self) -> EncoderId {
        self.id
    }

    /// Codec parameters (including extradata with SPS/PPS) for muxer stream
    /// setup and for initializing a matching decoder.
    pub fn parameters(&self) -> ff::codec::parameters::Parameters {
        ff::codec::parameters::Parameters::from(&self.encoder)
    }

    /// Encode one RGBA frame (`width*height*4` bytes, tightly packed rows).
    ///
    /// `pts_us` anchors this frame on the caller's microsecond clock; the
    /// first call sets the stream's time origin.
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        pts_us: i64,
    ) -> Result<Vec<EncodedPacket>, EncodeError> {
        let row_bytes = self.width as usize * 4;
        let need = row_bytes * self.height as usize;
        if rgba.len() < need {
            return Err(EncodeError::InvalidInput(format!(
                "rgba buffer too small: {} < {need}",
                rgba.len()
            )));
        }
        if self.pts_offset_us.is_none() {
            self.pts_offset_us = Some(pts_us);
        }

        // Copy rows into the source frame, honoring swscale stride.
        let stride = self.src_frame.stride(0);
        {
            let dst = self.src_frame.data_mut(0);
            for (d_row, s_row) in dst.chunks_mut(stride).zip(rgba.chunks_exact(row_bytes)) {
                d_row[..row_bytes].copy_from_slice(s_row);
            }
        }
        self.src_frame.set_pts(Some(self.frame_index));
        self.scaler
            .run(&self.src_frame, &mut self.dst_frame)
            .map_err(|e| EncodeError::Encode(format!("swscale: {e}")))?;
        self.dst_frame.set_pts(Some(self.frame_index));

        self.encoder
            .send_frame(&self.dst_frame)
            .map_err(|e| EncodeError::Encode(format!("send_frame: {e}")))?;
        let packets = self.drain()?;
        self.frame_index += 1;
        Ok(packets)
    }

    /// Flush delayed (B-)frames; call at end of stream.
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        self.encoder
            .send_eof()
            .map_err(|e| EncodeError::Encode(format!("send_eof: {e}")))?;
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<EncodedPacket>, EncodeError> {
        use ff::util::error::{Error, EAGAIN};
        let mut out = Vec::new();
        loop {
            let mut pkt = ff::codec::packet::Packet::empty();
            match self.encoder.receive_packet(&mut pkt) {
                Ok(()) => out.push(self.to_encoded(&pkt)),
                Err(Error::Eof) => break,
                Err(Error::Other { errno }) if errno == EAGAIN => break,
                Err(e) => return Err(EncodeError::Encode(format!("receive_packet: {e}"))),
            }
        }
        Ok(out)
    }

    fn to_encoded(&self, pkt: &ff::codec::packet::Packet) -> EncodedPacket {
        let scale = self.time_base.0 as f64 / self.time_base.1 as f64 * 1_000_000.0;
        let offset = self.pts_offset_us.unwrap_or(0);
        let pts_us = pkt
            .pts()
            .map(|p| offset + (p as f64 * scale).round() as i64)
            .unwrap_or(-1);
        let dts_us = pkt
            .dts()
            .map(|d| offset + (d as f64 * scale).round() as i64)
            .unwrap_or(pts_us);
        EncodedPacket {
            data: pkt.data().map(|d| d.to_vec()).unwrap_or_default(),
            pts_us,
            dts_us,
            is_keyframe: pkt.is_key(),
            stream_index: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 640x360 gradient with a moving bright vertical bar.
    fn synth_frame(w: u32, h: u32, i: u32) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 4) as usize];
        let bar_x = (i as usize * 7) % w as usize;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = (y * w as usize + x) * 4;
                let bar = if x >= bar_x.saturating_sub(6) && x <= bar_x + 6 {
                    255
                } else {
                    0
                };
                px[o] = ((x * 255) / w as usize) as u8; // R: horizontal gradient
                px[o + 1] = ((y * 255) / h as usize) as u8; // G: vertical gradient
                px[o + 2] = bar; // B: moving bar
                px[o + 3] = 255;
            }
        }
        px
    }

    /// Decode raw packets with a decoder initialized from the encoder's
    /// parameters (carries codec id + extradata/SPS/PPS).
    fn decode_all(
        params: &ff::codec::parameters::Parameters,
        packets: &[EncodedPacket],
    ) -> Vec<ff::util::frame::Video> {
        let context =
            ff::codec::context::Context::from_parameters(params.clone()).expect("decoder ctx");
        let mut decoder = context.decoder().video().expect("open video decoder");
        let mut frames = Vec::new();
        for p in packets {
            let pkt = ff::codec::packet::Packet::copy(&p.data);
            decoder.send_packet(&pkt).expect("send_packet");
            let mut frame = ff::util::frame::Video::empty();
            while decoder.receive_frame(&mut frame).is_ok() {
                frames.push(frame.clone());
                frame = ff::util::frame::Video::empty();
            }
        }
        decoder.send_eof().expect("decoder eof");
        let mut frame = ff::util::frame::Video::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            frames.push(frame.clone());
            frame = ff::util::frame::Video::empty();
        }
        frames
    }

    /// BT.601 luma of an RGB pixel.
    fn luma(r: u8, g: u8, b: u8) -> f64 {
        0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64
    }

    fn psnr_y(source_rgba: &[u8], decoded: &ff::util::frame::Video, w: u32, h: u32) -> f64 {
        let stride = decoded.stride(0);
        let y = decoded.data(0);
        let mut se = 0.0;
        let mut n = 0u64;
        for row in 0..h as usize {
            for x in 0..w as usize {
                let s = luma(
                    source_rgba[(row * w as usize + x) * 4],
                    source_rgba[(row * w as usize + x) * 4 + 1],
                    source_rgba[(row * w as usize + x) * 4 + 2],
                );
                let d = y[row * stride + x] as f64;
                se += (s - d) * (s - d);
                n += 1;
            }
        }
        let mse = se / n as f64;
        if mse == 0.0 {
            return 99.0;
        }
        10.0 * (255.0 * 255.0 / mse).log10()
    }

    #[test]
    fn encode_decode_60_frames_roundtrip() {
        let (w, h) = (640u32, 360u32);
        let fps = 30u32;
        let n_frames = 60u32;
        let mut enc = VideoEncoder::open_with_encoder(
            EncoderId::H264Libx264,
            VideoEncoderOpts {
                width: w,
                height: h,
                fps_num: fps,
                fps_den: 1,
                bitrate_bps: 2_000_000,
                gop: 30,
                preset: "ultrafast".to_string(),
            },
        )
        .expect("open libx264");

        let t0 = std::time::Instant::now();
        let mut packets = Vec::new();
        let mut sources = Vec::new();
        for i in 0..n_frames {
            let rgba = synth_frame(w, h, i);
            let pts_us = i as i64 * 1_000_000 / fps as i64;
            packets.extend(enc.encode_rgba(&rgba, pts_us).expect("encode"));
            sources.push(rgba);
        }
        packets.extend(enc.flush().expect("flush"));
        let elapsed = t0.elapsed();
        let fps_measured = n_frames as f64 / elapsed.as_secs_f64();
        eprintln!("encode: {n_frames} frames in {:.2}s = {fps_measured:.1} fps (libx264 ultrafast, 640x360)", elapsed.as_secs_f64());

        assert!(!packets.is_empty(), "no packets produced");
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "no keyframe in stream"
        );
        // PTS strictly increasing in encode order? (B-frames reorder DTS, not PTS.)
        // Check monotonic non-decreasing PTS across packets in output order is
        // NOT guaranteed; instead verify every frame's worth of packets exists
        // via decode below. Here just check timestamps are sane.
        assert!(packets
            .iter()
            .all(|p| p.pts_us >= 0 && p.dts_us >= -1_000_000));
        assert!(packets.iter().all(|p| p.stream_index == 0));

        let params = enc.parameters();
        let frames = decode_all(&params, &packets);
        assert_eq!(frames.len(), n_frames as usize, "decoded frame count");

        let mut psnr_sum = 0.0;
        for (src, dec) in sources.iter().zip(frames.iter()) {
            assert_eq!(dec.width(), w);
            assert_eq!(dec.height(), h);
            psnr_sum += psnr_y(src, dec, w, h);
        }
        let psnr_mean = psnr_sum / n_frames as f64;
        eprintln!("mean luma PSNR vs source: {psnr_mean:.2} dB");
        assert!(
            psnr_mean > 30.0,
            "PSNR too low ({psnr_mean:.2} dB); encoder may be broken"
        );
    }

    #[test]
    fn rejects_short_rgba_buffer() {
        let mut enc = VideoEncoder::open_with_encoder(
            EncoderId::H264Libx264,
            VideoEncoderOpts {
                width: 64,
                height: 64,
                fps_num: 30,
                fps_den: 1,
                bitrate_bps: 500_000,
                gop: 30,
                preset: "ultrafast".to_string(),
            },
        )
        .expect("open");
        let err = enc.encode_rgba(&vec![0u8; 100], 0);
        assert!(matches!(err, Err(EncodeError::InvalidInput(_))));
    }

    #[test]
    fn rejects_zero_dimensions() {
        let res = VideoEncoder::open_with_encoder(
            EncoderId::H264Libx264,
            VideoEncoderOpts {
                width: 0,
                height: 64,
                fps_num: 30,
                fps_den: 1,
                bitrate_bps: 500_000,
                gop: 30,
                preset: "ultrafast".to_string(),
            },
        );
        assert!(matches!(res, Err(EncodeError::InvalidInput(_))));
    }

    #[test]
    fn unknown_encoder_name_errors() {
        // Direct registry miss path (bypasses selection).
        let codec = ff::encoder::find_by_name("no_such_encoder_xyz");
        assert!(codec.is_none());
    }
}
