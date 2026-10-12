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
    /// Encoder stream time base: (fps_den, fps_num) — one tick per frame
    /// slot. A frame's pts is its *wall-clock* slot (see
    /// [`VideoEncoder::encode_rgba`]), not a frame counter.
    time_base: ff::Rational,
    /// Last pts sent to the encoder (time-base units).
    last_pts: Option<i64>,
    /// Microsecond anchor: `packet_pts_us = rescale(encoder_pts) + pts_offset_us`,
    /// where the offset is taken from the first `encode_rgba` call's `pts_us`.
    pts_offset_us: Option<i64>,
    /// Next frame must be an IDR keyframe (a new output joined, or the
    /// stream reconnected).
    force_key: bool,
    /// Duration of the last `encode_rgba` (convert + encode), µs.
    last_encode_us: u64,
    id: EncoderId,
    width: u32,
    height: u32,
}

/// Per-encoder low-latency, CBR options (FFmpeg n6.1 option names).
///
/// Each wrapper ignores options it does not define (they simply stay in
/// the dictionary), so a name valid for one wrapper never fails another.
fn low_latency_options(id: EncoderId, opts: &VideoEncoderOpts) -> ff::Dictionary<'static> {
    let mut d = ff::Dictionary::new();
    match id {
        EncoderId::H264Libx264 => {
            d.set("preset", &opts.preset);
            // zerolatency: no lookahead, no frame-thread delay, no
            // B-frames — every frame leaves in the same call it went in.
            d.set("tune", "zerolatency");
            d.set("profile", "high");
            d.set("forced-idr", "1");
            d.set("x264-params", "nal-hrd=cbr");
        }
        EncoderId::H264Nvenc => {
            // NVENC presets are p1..p7; the x264 names ("veryfast") are
            // invalid since NVENC SDK 10 and used to fail the open,
            // silently falling back to a slower encoder.
            d.set("preset", "p4");
            d.set("tune", "ll");
            d.set("rc", "cbr");
            d.set("zerolatency", "1");
            d.set("delay", "0");
            d.set("rc-lookahead", "0");
            d.set("forced-idr", "1");
            d.set("profile", "high");
        }
        EncoderId::H264Qsv => {
            d.set("preset", "veryfast");
            d.set("async_depth", "1");
            d.set("look_ahead", "0");
            d.set("forced_idr", "1");
            d.set("low_delay_brc", "1");
            d.set("profile", "high");
        }
        EncoderId::H264Amf => {
            d.set("usage", "lowlatency");
            d.set("rc", "cbr");
            d.set("quality", "speed");
            d.set("forced_idr", "1");
            d.set("profile", "high");
        }
        EncoderId::Mpeg4 => {}
    }
    d
}

/// Microseconds → frame-slot index in `tb` (seconds per slot = num/den),
/// rounded to the nearest slot.
fn us_to_slot(us: i64, tb: ff::Rational) -> i64 {
    let n = us as i128 * tb.1 as i128;
    let d = tb.0 as i128 * 1_000_000;
    (n + d / 2).div_euclid(d) as i64
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
        // Live: no B-frames. B-frames hold back output by their reorder
        // depth (2 frames = 66 ms at 30 fps) and every hardware encoder adds
        // its own lookahead on top; for a live stream the current frame must
        // leave the encoder immediately.
        v.set_max_b_frames(0);
        // SPS/PPS in extradata (not in-band): required by MP4/FLV muxers.
        v.set_flags(ff::codec::flag::Flags::GLOBAL_HEADER);
        // CBR with a 1-second VBV (YouTube / Facebook / TikTok ingest
        // guidance): constant upload rate, no bitrate spikes that would
        // queue in the socket and add latency.
        v.set_bit_rate(opts.bitrate_bps as usize);
        v.set_max_bit_rate(opts.bitrate_bps as usize);
        // SAFETY: plain field writes on the not-yet-opened codec context.
        unsafe {
            let ctx = v.as_mut_ptr();
            (*ctx).rc_min_rate = opts.bitrate_bps as i64;
            (*ctx).rc_buffer_size = opts.bitrate_bps as i32;
        }

        let dict = low_latency_options(id, &opts);
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
            last_pts: None,
            pts_offset_us: None,
            force_key: false,
            last_encode_us: 0,
            id,
            width: opts.width,
            height: opts.height,
        })
    }

    /// The encoder actually in use.
    pub fn encoder_id(&self) -> EncoderId {
        self.id
    }

    /// Human-readable encoder name (e.g. "h264_nvenc", "h264_qsv").
    pub fn encoder_name(&self) -> &'static str {
        self.id.ffmpeg_name()
    }

    /// Codec parameters (including extradata with SPS/PPS) for muxer stream
    /// setup and for initializing a matching decoder.
    pub fn parameters(&self) -> ff::codec::parameters::Parameters {
        ff::codec::parameters::Parameters::from(&self.encoder)
    }

    /// Request an IDR keyframe on the next encoded frame (a new output
    /// joined mid-session, or the stream reconnected).
    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Duration of the last [`Self::encode_rgba`] call in µs (RGBA→YUV
    /// conversion + encode), for diagnostics.
    pub fn last_encode_us(&self) -> u64 {
        self.last_encode_us
    }

    /// Encode one RGBA frame (`width*height*4` bytes, tightly packed rows).
    ///
    /// `pts_us` is the frame's presentation time on the caller's
    /// microsecond clock. The first call sets the stream origin; every
    /// later frame lands on the frame-slot grid at its **real** time
    /// (`round((pts_us − origin) · fps)`), so a frame the caller could not
    /// render (overloaded tick) becomes a timestamp gap instead of pulling
    /// every later frame earlier. With a frame counter as pts (the old
    /// behaviour) each skipped tick made the stream's media clock fall
    /// further behind real time — viewers drifted a minute behind after a
    /// few minutes under load. A frame mapping to an already-used slot is
    /// skipped (no packets).
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        pts_us: i64,
    ) -> Result<Vec<EncodedPacket>, EncodeError> {
        let t0 = std::time::Instant::now();
        let row_bytes = self.width as usize * 4;
        let need = row_bytes * self.height as usize;
        if rgba.len() < need {
            return Err(EncodeError::InvalidInput(format!(
                "rgba buffer too small: {} < {need}",
                rgba.len()
            )));
        }
        let origin = *self.pts_offset_us.get_or_insert(pts_us);
        let pts = us_to_slot(pts_us - origin, self.time_base);
        if self.last_pts.is_some_and(|last| pts <= last) {
            return Ok(Vec::new());
        }

        // Copy rows into the source frame, honoring swscale stride.
        let stride = self.src_frame.stride(0);
        {
            let dst = self.src_frame.data_mut(0);
            for (d_row, s_row) in dst.chunks_mut(stride).zip(rgba.chunks_exact(row_bytes)) {
                d_row[..row_bytes].copy_from_slice(s_row);
            }
        }
        self.scaler
            .run(&self.src_frame, &mut self.dst_frame)
            .map_err(|e| EncodeError::Encode(format!("swscale: {e}")))?;
        self.dst_frame.set_pts(Some(pts));
        let key = std::mem::take(&mut self.force_key);
        self.dst_frame.set_kind(if key {
            ff::picture::Type::I
        } else {
            ff::picture::Type::None
        });

        self.encoder
            .send_frame(&self.dst_frame)
            .map_err(|e| EncodeError::Encode(format!("send_frame: {e}")))?;
        self.last_pts = Some(pts);
        let packets = self.drain()?;
        self.last_encode_us = t0.elapsed().as_micros() as u64;
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

    fn open_test(w: u32, h: u32) -> VideoEncoder {
        VideoEncoder::open_with_encoder(
            EncoderId::H264Libx264,
            VideoEncoderOpts {
                width: w,
                height: h,
                fps_num: 30,
                fps_den: 1,
                bitrate_bps: 1_000_000,
                gop: 60,
                preset: "ultrafast".to_string(),
            },
        )
        .expect("open libx264")
    }

    /// Skipped ticks must become timestamp gaps — the media clock of the
    /// stream stays on real time (root cause of the growing live delay).
    #[test]
    fn pts_follow_real_time_across_skipped_frames() {
        let mut enc = open_test(160, 96);
        let mut pkts = Vec::new();
        // Real times: frames 0,1,2 on time, then a 10-frame stall, then 13,14.
        for slot in [0i64, 1, 2, 13, 14] {
            let t = 5_000_000 + slot * 33_333;
            pkts.extend(enc.encode_rgba(&synth_frame(160, 96, slot as u32), t).unwrap());
        }
        pkts.extend(enc.flush().unwrap());
        let pts: Vec<i64> = pkts.iter().map(|p| p.pts_us - 5_000_000).collect();
        assert_eq!(pts.len(), 5, "{pts:?}");
        // Zero-latency: decode order == presentation order, dts == pts.
        assert!(pkts.iter().all(|p| p.dts_us == p.pts_us));
        assert!((pts[3] - 13 * 33_333).abs() <= 10, "{pts:?}");
        assert!((pts[4] - 14 * 33_333).abs() <= 10, "{pts:?}");
    }

    /// Zero-latency: the packet for a frame comes out of the same call.
    #[test]
    fn no_encoder_delay() {
        let mut enc = open_test(160, 96);
        for i in 0..10 {
            let out = enc.encode_rgba(&synth_frame(160, 96, i), i as i64 * 33_333).unwrap();
            assert_eq!(out.len(), 1, "frame {i} delayed by the encoder");
        }
    }

    #[test]
    fn duplicate_slot_is_skipped_and_forced_key_is_idr() {
        let mut enc = open_test(160, 96);
        let f = synth_frame(160, 96, 0);
        assert_eq!(enc.encode_rgba(&f, 0).unwrap().len(), 1);
        for i in 1..5 {
            let p = enc.encode_rgba(&f, i * 33_333).unwrap();
            assert!(!p[0].is_keyframe);
        }
        // Same slot as the previous frame → skipped.
        assert!(enc.encode_rgba(&f, 4 * 33_333 + 5_000).unwrap().is_empty());
        enc.force_keyframe();
        let p = enc.encode_rgba(&f, 5 * 33_333).unwrap();
        assert!(p[0].is_keyframe, "forced keyframe not honored");
        let p = enc.encode_rgba(&f, 6 * 33_333).unwrap();
        assert!(!p[0].is_keyframe);
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
