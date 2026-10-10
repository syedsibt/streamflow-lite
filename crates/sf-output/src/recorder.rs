//! File recording: mux [`EncodedPacket`][sf_encode::EncodedPacket]s into
//! MP4 (fragmented or flat) / MKV via libavformat.
//!
//! Output streams use a 1/1_000_000 (microsecond) time base, so packet
//! `pts_us`/`dts_us` are written verbatim with no precision loss.
//!
//! Usage:
//! ```no_run
//! # use sf_output::{Recorder, ContainerFormat};
//! # use sf_encode::{VideoEncoder, VideoEncoderOpts, EncoderId, AudioEncoder};
//! # fn main() -> Result<(), sf_output::OutputError> {
//! let mut venc = VideoEncoder::open_with_encoder(
//!     EncoderId::H264Libx264,
//!     VideoEncoderOpts { width: 1280, height: 720, fps_num: 30, fps_den: 1,
//!         bitrate_bps: 4_000_000, gop: 60, preset: "veryfast".into() },
//! ).map_err(|e| sf_output::OutputError::Init(e.to_string()))?;
//! let aenc = AudioEncoder::open()
//!     .map_err(|e| sf_output::OutputError::Init(e.to_string()))?;
//! let mut rec = Recorder::create("/tmp/out.mp4", ContainerFormat::Mp4Fragmented)?;
//! rec.add_video_stream(&venc.parameters())?;
//! rec.add_audio_stream(&aenc.parameters(), aenc.sample_rate())?;
//! // ... rec.write_video(&packet)? for each EncodedPacket ...
//! let stats = rec.finish()?;
//! # Ok(())
//! # }
//! ```

use super::{ContainerFormat, OutputError, OutputStats};
use ffmpeg_next as ff;
use sf_encode::EncodedPacket;

/// Microsecond stream time base used for all output streams.
const STREAM_TIME_BASE: ff::Rational = ff::Rational(1, 1_000_000);

/// Records encoded packets to a media file.
///
/// Video streams use a microsecond (1/1_000_000) time base. Audio streams use
/// 1/`sample_rate`: the MP4 muxer forces the audio track timescale to the
/// sample rate and consumes packet timestamps raw, so µs timestamps must be
/// rescaled (done inside [`Recorder::write_audio`]).
pub struct Recorder {
    octx: Option<ff::format::context::Output>,
    video_stream: Option<usize>,
    audio_stream: Option<usize>,
    audio_time_base: ff::Rational,
    next_stream: usize,
    header_written: bool,
    fragmented: bool,
    video_packets: u64,
    audio_packets: u64,
    first_pts_us: Option<i64>,
    last_dts_us: Option<i64>,
}

impl Recorder {
    /// Create a recorder for `path` in the given container format.
    ///
    /// Streams are added with [`Recorder::add_video_stream`] /
    /// [`Recorder::add_audio_stream`]; the header is written lazily on the
    /// first packet.
    pub fn create(path: &str, fmt: ContainerFormat) -> Result<Self, OutputError> {
        ff::init().map_err(|e| OutputError::Init(format!("ffmpeg init: {e}")))?;
        let octx = match fmt {
            ContainerFormat::Mkv => ff::format::output_as(&path, "matroska"),
            _ => ff::format::output(&path),
        }
        .map_err(|e| OutputError::Init(format!("open {path}: {e}")))?;
        Ok(Self {
            octx: Some(octx),
            video_stream: None,
            audio_stream: None,
            audio_time_base: STREAM_TIME_BASE,
            next_stream: 0,
            header_written: false,
            fragmented: matches!(fmt, ContainerFormat::Mp4Fragmented),
            video_packets: 0,
            audio_packets: 0,
            first_pts_us: None,
            last_dts_us: None,
        })
    }

    /// Add the video stream, copying codec parameters (incl. extradata)
    /// from the encoder. The stream time base is microseconds. Must be called
    /// before any `write_*`.
    pub fn add_video_stream(
        &mut self,
        params: &ff::codec::parameters::Parameters,
    ) -> Result<(), OutputError> {
        let idx = self.add_stream(params, STREAM_TIME_BASE)?;
        self.video_stream = Some(idx);
        Ok(())
    }

    /// Add the audio stream. `sample_rate` sets the stream time base to
    /// 1/`sample_rate` (required: MP4 forces the audio track timescale to the
    /// sample rate). Optional; must be called before any `write_*`.
    pub fn add_audio_stream(
        &mut self,
        params: &ff::codec::parameters::Parameters,
        sample_rate: u32,
    ) -> Result<(), OutputError> {
        if sample_rate == 0 {
            return Err(OutputError::Init("sample_rate must be non-zero".into()));
        }
        let tb = ff::Rational(1, sample_rate as i32);
        let idx = self.add_stream(params, tb)?;
        self.audio_stream = Some(idx);
        self.audio_time_base = tb;
        Ok(())
    }

    fn add_stream(
        &mut self,
        params: &ff::codec::parameters::Parameters,
        time_base: ff::Rational,
    ) -> Result<usize, OutputError> {
        if self.header_written {
            return Err(OutputError::Header(
                "cannot add a stream after the header was written".into(),
            ));
        }
        let octx = self
            .octx
            .as_mut()
            .ok_or_else(|| OutputError::Init("closed".into()))?;
        // ffmpeg-next 6.x has no `add_stream_with(&Context)`; instead add the
        // stream for the codec, then overwrite its parameters (incl.
        // extradata: SPS/PPS / AudioSpecificConfig) from the encoder.
        let codec = ff::encoder::find(params.id()).ok_or_else(|| {
            OutputError::Init(format!("no encoder for {:?} in registry", params.id()))
        })?;
        let mut stream = octx
            .add_stream(codec)
            .map_err(|e| OutputError::Init(format!("add_stream: {e}")))?;
        stream.set_time_base(time_base);
        stream.set_parameters(params.clone());
        let idx = self.next_stream;
        self.next_stream += 1;
        Ok(idx)
    }

    /// Write one video packet (`packet.stream_index` should be 0).
    pub fn write_video(&mut self, packet: &EncodedPacket) -> Result<(), OutputError> {
        let idx = self.video_stream.ok_or(OutputError::NoVideoStream)?;
        self.write_packet(packet, idx, true)
    }

    /// Write one audio packet (`packet.stream_index` should be 1).
    pub fn write_audio(&mut self, packet: &EncodedPacket) -> Result<(), OutputError> {
        let idx = self
            .audio_stream
            .ok_or_else(|| OutputError::Write("no audio stream added".into()))?;
        self.write_packet(packet, idx, false)
    }

    fn write_packet(
        &mut self,
        packet: &EncodedPacket,
        stream_index: usize,
        is_video: bool,
    ) -> Result<(), OutputError> {
        self.ensure_header()?;
        // Rescale the µs packet clock into the stream's time base
        // (video: µs — identity; audio: 1/sample_rate).
        let tb = if is_video {
            STREAM_TIME_BASE
        } else {
            self.audio_time_base
        };
        let mut fp = ff::codec::packet::Packet::copy(&packet.data);
        fp.set_pts(Some(super::rescale_us_to_tb(packet.pts_us, tb)));
        fp.set_dts(Some(super::rescale_us_to_tb(packet.dts_us, tb)));
        // Packet timestamps are in the stream's time base; av_write_frame
        // expects stream time-base units, so no further rescaling is needed.
        fp.set_stream(stream_index);
        let octx = self
            .octx
            .as_mut()
            .ok_or_else(|| OutputError::Write("closed".into()))?;
        fp.write(octx)
            .map_err(|e| OutputError::Write(format!("stream {stream_index}: {e}")))?;
        if is_video {
            self.video_packets += 1;
        } else {
            self.audio_packets += 1;
        }
        self.first_pts_us.get_or_insert(packet.pts_us);
        self.last_dts_us = Some(packet.dts_us.max(self.last_dts_us.unwrap_or(i64::MIN)));
        Ok(())
    }

    fn ensure_header(&mut self) -> Result<(), OutputError> {
        if self.header_written {
            return Ok(());
        }
        if self.video_stream.is_none() {
            return Err(OutputError::NoVideoStream);
        }
        let octx = self
            .octx
            .as_mut()
            .ok_or_else(|| OutputError::Header("closed".into()))?;
        // Shift timestamps so the earliest DTS lands at zero: H.264 encoders
        // with B-frames emit the first packets with negative DTS (reorder
        // delay), which av_write_frame rejects with EINVAL.
        let mut dict = ff::Dictionary::new();
        dict.set("avoid_negative_ts", "make_zero");
        if self.fragmented {
            dict.set("movflags", "frag_keyframe+empty_moov+default_base_moof");
        }
        octx.write_header_with(dict)
            .map_err(|e| OutputError::Header(format!("{e}")))?;
        self.header_written = true;
        Ok(())
    }

    /// Finalize the file and return statistics.
    ///
    /// Consumes the recorder; dropping it without `finish` leaves a file
    /// without a trailer (fragmented MP4 stays playable up to the last
    /// completed fragment — that is the point of the fragmented default).
    pub fn finish(mut self) -> Result<OutputStats, OutputError> {
        // Writing zero packets is legal: still emit header+trailer so the
        // file is a valid (empty) container.
        self.ensure_header()?;
        let mut octx = self
            .octx
            .take()
            .ok_or_else(|| OutputError::Trailer("closed".into()))?;
        super::write_trailer(&mut octx)?;
        let duration_us = match (self.first_pts_us, self.last_dts_us) {
            (Some(first), Some(last)) => last.saturating_sub(first).max(0) as u64,
            _ => 0,
        };
        Ok(OutputStats {
            video_packets: self.video_packets,
            audio_packets: self.audio_packets,
            duration_us,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sf_encode::{AudioEncoder, EncoderId, VideoEncoder, VideoEncoderOpts};
    use std::process::Command;

    /// Encode 60 video frames + 2 s of audio, interleave by pts, record to
    /// fragmented MP4, then prove playability with the ffprobe CLI.
    #[test]
    fn record_fragmented_mp4_and_probe() {
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
        .expect("open video encoder");
        let mut aenc = AudioEncoder::open().expect("open audio encoder");

        let mut vpkts = Vec::new();
        for i in 0..60u32 {
            // Simple moving-gradient frame.
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let o = (y * w as usize + x) * 4;
                    rgba[o] = ((x as u32 * 255 / w as u32 + i * 4) % 256) as u8;
                    rgba[o + 1] = (y as u32 * 255 / h as u32) as u8;
                    rgba[o + 2] = 128;
                    rgba[o + 3] = 255;
                }
            }
            vpkts.extend(
                venc.encode_rgba(&rgba, i as i64 * 1_000_000 / fps as i64)
                    .expect("encode_rgba"),
            );
        }
        vpkts.extend(venc.flush().expect("video flush"));

        let mut apkts = Vec::new();
        // 2 s of 440 Hz stereo.
        let n = 2 * 48_000usize;
        let mut pcm = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (0.4 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48_000.0).sin()) as f32;
            pcm.push(s);
            pcm.push(s);
        }
        for (ci, chunk) in pcm.chunks(8192).enumerate() {
            let pts_us = ci as i64 * 8192 / 2 * 1_000_000 / 48_000;
            apkts.extend(aenc.encode_pcm(chunk, pts_us).expect("encode_pcm"));
        }
        apkts.extend(aenc.flush().expect("audio flush"));

        // Interleave by DTS (encode order per stream is already DTS-sorted).
        // NOTE: must be DTS, not PTS — with B-frames, PTS order is not DTS
        // order, and the muxer requires monotonically increasing DTS.
        let mut all: Vec<(&EncodedPacket, bool)> = vpkts
            .iter()
            .map(|p| (p, true))
            .chain(apkts.iter().map(|p| (p, false)))
            .collect();
        all.sort_by_key(|(p, _)| (p.dts_us, p.stream_index));

        let path = "/tmp/rec-test.mp4";
        let _ = std::fs::remove_file(path);
        let mut rec = Recorder::create(path, ContainerFormat::Mp4Fragmented).expect("create");
        rec.add_video_stream(&venc.parameters())
            .expect("video stream");
        rec.add_audio_stream(&aenc.parameters(), aenc.sample_rate())
            .expect("audio stream");
        for (p, is_video) in &all {
            if *is_video {
                rec.write_video(p).expect("write_video");
            } else {
                rec.write_audio(p).expect("write_audio");
            }
        }
        let stats = rec.finish().expect("finish");
        eprintln!(
            "recorded: {} video + {} audio packets, duration {} us",
            stats.video_packets, stats.audio_packets, stats.duration_us
        );
        assert_eq!(stats.video_packets as usize, vpkts.len());
        assert_eq!(stats.audio_packets as usize, apkts.len());
        assert!(
            (1_800_000..2_200_000).contains(&stats.duration_us),
            "duration {} us not near 2 s",
            stats.duration_us
        );

        let meta = std::fs::metadata(path).expect("output file exists");
        assert!(meta.len() > 10_000, "suspiciously small file");

        // Prove playability with the ffprobe CLI.
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-show_entries",
                "format=duration:stream=codec_name,nb_read_frames",
                "-of",
                "default=noprint_wrappers=1",
                path,
            ])
            .output()
            .expect("run ffprobe");
        assert!(
            out.status.success(),
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        eprintln!("ffprobe:\n{text}");
        assert!(text.contains("codec_name=h264"), "no h264 stream");
        assert!(text.contains("codec_name=aac"), "no aac stream");
        assert!(
            text.contains("nb_read_frames=60"),
            "video frame count != 60"
        );
        let duration: f64 = text
            .lines()
            .find_map(|l| l.strip_prefix("duration="))
            .and_then(|s| s.parse().ok())
            .expect("duration in ffprobe output");
        assert!(
            (1.8..2.2).contains(&duration),
            "ffprobe duration {duration} not near 2 s"
        );
    }

    #[test]
    fn write_without_video_stream_errors() {
        let mut rec =
            Recorder::create("/tmp/rec-novid.mp4", ContainerFormat::Mp4Flat).expect("create");
        let pkt = EncodedPacket {
            data: vec![0u8; 16],
            pts_us: 0,
            dts_us: 0,
            is_keyframe: false,
            stream_index: 0,
        };
        assert!(matches!(
            rec.write_video(&pkt).unwrap_err(),
            OutputError::NoVideoStream
        ));
    }
}
