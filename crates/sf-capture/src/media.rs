//! Media file capture (mp4/mkv/…).
//!
//! # Status: **verified here** (Linux, FFmpeg 6.1 dev libs)
//!
//! [`MediaFileProducer`] opens a file with FFmpeg, decodes the best video
//! stream to RGBA8 via swscale, and exposes the audio stream resampled to
//! the normalized mixer format.
//!
//! # Audio normalization
//!
//! [`MediaFileProducer::next_audio`] returns [`DecodedAudio`] with
//! **48000 Hz, stereo, interleaved `f32`** regardless of the source format
//! ([`AUDIO_SAMPLE_RATE`] / [`AUDIO_CHANNELS`]). The engine's audio mixer
//! expects exactly this layout, so capture normalizes once at the source.

use ffmpeg_next as ff;

use super::demux::{ensure_init, DemuxDecoder};
use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// Normalized audio sample rate (Hz) produced by [`MediaFileProducer::next_audio`].
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// Normalized audio channel count (stereo) produced by [`MediaFileProducer::next_audio`].
pub const AUDIO_CHANNELS: u32 = 2;

/// One decoded audio chunk: interleaved `f32` samples, always
/// [`AUDIO_CHANNELS`] channels at [`AUDIO_SAMPLE_RATE`] Hz.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// Interleaved f32 samples (`len == frames * AUDIO_CHANNELS`).
    pub samples: Vec<f32>,
    pub channels: u32,
    pub sample_rate: u32,
    /// Presentation timestamp in microseconds.
    pub pts_us: i64,
}

impl DecodedAudio {
    /// Number of sample frames (per channel).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// Duration of this chunk in microseconds at the normalized rate.
    pub fn duration_us(&self) -> u64 {
        self.frames() as u64 * 1_000_000 / self.sample_rate as u64
    }
}

/// File-backed [`FrameProducer`]: decodes a media file's video to RGBA8.
///
/// Video frames come out of [`FrameProducer::next_frame`]; audio chunks
/// come out of [`next_audio`](Self::next_audio). Both are driven by the
/// same demuxer, so interleaved calls stay in sync. When the file is
/// exhausted, `next_frame` returns `Err(CaptureError::EndOfStream)` and
/// `next_audio` returns `Ok(None)`.
pub struct MediaFileProducer {
    demux: DemuxDecoder,
    info: StreamInfo,
}

impl MediaFileProducer {
    /// Open a media file. Finds the best video stream; audio is optional.
    pub fn open(path: &str) -> Result<Self, CaptureError> {
        ensure_init();
        let path_buf = std::path::PathBuf::from(path);
        let ictx = ff::format::input(&path_buf)
            .map_err(|e| CaptureError::OpenFailed(format!("cannot open {path}: {e}")))?;
        let has_audio = ictx.streams().best(ff::media::Type::Audio).is_some();
        // Nominal frame duration from the container's average frame rate.
        let nominal_frame_us = ictx
            .streams()
            .best(ff::media::Type::Video)
            .and_then(|s| {
                let r = s.avg_frame_rate();
                if r.numerator() > 0 && r.denominator() > 0 {
                    Some(1_000_000u64 * r.denominator() as u64 / r.numerator() as u64)
                } else {
                    None
                }
            })
            .unwrap_or(0);
        let demux = DemuxDecoder::from_input(ictx, nominal_frame_us)?;
        let info = demux.stream_info(has_audio);
        Ok(Self { demux, info })
    }

    /// Pull the next audio chunk (48000 Hz stereo interleaved `f32`).
    /// Returns `Ok(None)` when the file has no audio or audio is exhausted.
    pub fn next_audio(&mut self) -> Result<Option<DecodedAudio>, CaptureError> {
        Ok(self.demux.next_audio()?.map(|a| DecodedAudio {
            channels: AUDIO_CHANNELS,
            sample_rate: AUDIO_SAMPLE_RATE,
            pts_us: a.pts_us,
            samples: a.samples,
        }))
    }

    /// Drain all remaining video frames (convenience for tests/tools).
    pub fn drain_video(&mut self) -> Result<Vec<CapturedFrame>, CaptureError> {
        let mut out = Vec::new();
        loop {
            match self.next_frame() {
                Ok(Some(f)) => out.push(f),
                Ok(None) => continue,
                Err(CaptureError::EndOfStream) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

impl FrameProducer for MediaFileProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        self.demux.next_video()
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Generate the 2 s A/V test file with the FFmpeg CLI.
    /// 320×240 @ 30 fps testsrc (60 frames) + 440 Hz sine, yuv420p + aac.
    fn make_test_file() -> String {
        let path = "/tmp/sf-capture-test.mp4";
        let st = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=30:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                path,
            ])
            .status()
            .expect("ffmpeg CLI must be installed");
        assert!(st.success(), "ffmpeg failed to generate test file");
        path.to_string()
    }

    #[test]
    fn media_file_decodes_exact_frame_count_with_correct_pts() {
        let path = make_test_file();
        let mut p = MediaFileProducer::open(&path).expect("open test file");
        let info = p.stream_info();
        assert_eq!((info.width, info.height), (320, 240));
        assert!(info.has_audio, "test file must have an audio stream");

        let frames = p.drain_video().expect("decode all frames");
        // VERIFIED MEASUREMENT (2026-10-09, FFmpeg 6.1): exactly 60 frames.
        assert_eq!(frames.len(), 60, "expected 60 frames for 2 s @ 30 fps");

        let mut prev = -1i64;
        for (i, f) in frames.iter().enumerate() {
            assert_eq!((f.width, f.height), (320, 240));
            assert_eq!(f.data.len(), 320 * 240 * 4);
            assert!(f.pts_us > prev, "PTS must strictly increase (frame {i})");
            if i > 0 {
                let gap = (f.pts_us - prev) as u64;
                // 1/30 s = 33333.33 µs; container rounding gives 33333/33334.
                assert!(
                    (33_000..=34_000).contains(&gap),
                    "frame {i}: PTS gap {gap} µs out of range"
                );
            }
            prev = f.pts_us;
        }
        // Total video duration ≈ 2 s (last PTS + one frame duration).
        let total_us = prev + frames[0].duration_us as i64;
        assert!(
            (1_950_000..=2_050_000).contains(&total_us),
            "total video duration {total_us} µs, expected ≈2 s"
        );
        // Non-black content: testsrc has bright bars.
        let max: u8 = *frames[30].data.iter().max().unwrap();
        assert!(max > 200, "testsrc frame should contain bright pixels");
    }

    #[test]
    fn media_file_audio_is_normalized_to_48k_stereo_f32() {
        let path = make_test_file();
        let mut p = MediaFileProducer::open(&path).expect("open test file");

        let mut total_samples = 0usize;
        let mut prev_pts = -1i64;
        let mut chunks = 0usize;
        while let Some(a) = p.next_audio().expect("audio decode") {
            assert_eq!(a.sample_rate, AUDIO_SAMPLE_RATE);
            assert_eq!(a.channels, AUDIO_CHANNELS);
            assert_eq!(a.sample_rate, 48_000);
            assert!(a.pts_us >= prev_pts, "audio PTS must not go backwards");
            prev_pts = a.pts_us;
            total_samples += a.samples.len();
            chunks += 1;
            // Sanity: sine wave has real energy.
            let peak = a.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.01, "sine chunk {chunks} is silent");
        }
        assert!(chunks > 0, "expected audio chunks");
        // 2 s @ 48 kHz stereo = 192000 interleaved f32; AAC priming/delay
        // adds a little — accept ±5%.
        let total_s = total_samples as f64 / (48_000.0 * 2.0);
        assert!(
            (1.9..=2.15).contains(&total_s),
            "audio total {total_s:.3} s, expected ≈2 s ({total_samples} samples)"
        );
    }

    #[test]
    fn media_file_open_missing_file_errors() {
        assert!(matches!(
            MediaFileProducer::open("/tmp/sf-capture-does-not-exist-xyz.mp4"),
            Err(CaptureError::OpenFailed(_))
        ));
    }

    #[test]
    fn decoded_audio_duration_math() {
        let a = DecodedAudio {
            samples: vec![0.0; 48_000 * 2], // 1 s stereo
            channels: 2,
            sample_rate: 48_000,
            pts_us: 0,
        };
        assert_eq!(a.frames(), 48_000);
        assert_eq!(a.duration_us(), 1_000_000);
    }
}
