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

use std::time::{SystemTime, UNIX_EPOCH};

use ffmpeg_next as ff;

use super::demux::{ensure_init, DemuxDecoder};
use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// Max frames decoded in one paced pull to catch up with the clock.
/// Beyond this the clock resyncs instead of burning CPU.
const MAX_CATCHUP_DECODE: usize = 8;
/// If the newest decoded frame is still this far behind the clock after
/// the catch-up budget, resync the clock (drop the backlog).
const RESYNC_BEHIND_US: i64 = 100_000;

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

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
    /// Wall-clock (µs) when the current play segment started.
    play_start_us: Option<u64>,
    /// Media time (µs) at the start of the current play segment.
    play_offset_us: u64,
    /// While paused the video freezes and audio goes silent.
    paused: bool,
    /// A frame decoded ahead of the clock, held for the next pull.
    pending: Option<CapturedFrame>,
    /// Last frame handed out (shown while paused or when stalled).
    last: Option<CapturedFrame>,
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
        Ok(Self {
            demux,
            info,
            play_start_us: None,
            play_offset_us: 0,
            paused: false,
            pending: None,
            last: None,
        })
    }

    /// Pause/resume playback. While paused the video freezes on the last
    /// shown frame and the audio goes silent; resume continues from the
    /// same media position.
    pub fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        let now = now_us();
        if paused {
            if let Some(start) = self.play_start_us {
                self.play_offset_us = self
                    .play_offset_us
                    .saturating_add(now.saturating_sub(start));
            }
            self.play_start_us = None;
        } else {
            self.play_start_us = Some(now);
        }
        self.paused = paused;
    }

    /// Whether playback is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Pull the next audio chunk (48000 Hz stereo interleaved `f32`).
    /// Returns `Ok(None)` when the file has no audio, audio is exhausted,
    /// or playback is paused.
    pub fn next_audio(&mut self) -> Result<Option<DecodedAudio>, CaptureError> {
        if self.paused {
            return Ok(None);
        }
        Ok(self.demux.next_audio()?.map(|a| DecodedAudio {
            channels: AUDIO_CHANNELS,
            sample_rate: AUDIO_SAMPLE_RATE,
            pts_us: a.pts_us,
            samples: a.samples,
        }))
    }

    /// Drain all remaining video frames (convenience for tests/tools).
    /// Bypasses the playback clock: decodes straight through the file.
    pub fn drain_video(&mut self) -> Result<Vec<CapturedFrame>, CaptureError> {
        let mut out = Vec::new();
        loop {
            match self.demux.next_video() {
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
    /// PTS-paced frame pull: returns the frame whose presentation time
    /// matches the playback clock, so the file plays at its native speed
    /// no matter how often — or by how many consumers — this is called.
    ///
    /// Previously every call consumed one frame, so the 30 Hz media tick
    /// plus the ~10 Hz Color Studio preview advanced a 30 fps file at
    /// ~40 fps (visibly too fast), and a 60 Hz output tick decoded twice
    /// as many frames as needed (CPU spike).
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.paused {
            // Frozen: the caller falls back to its cached last frame.
            return Ok(None);
        }
        let now = now_us();
        let start = match self.play_start_us {
            Some(s) => s,
            None => {
                self.play_start_us = Some(now);
                now
            }
        };
        let target_us = self
            .play_offset_us
            .saturating_add(now.saturating_sub(start)) as i64;

        // A frame decoded ahead of the clock by an earlier pull is shared
        // by all consumers — hand it out when it becomes due.
        let mut current: Option<CapturedFrame> = None;
        if let Some(p) = self.pending.take() {
            if p.pts_us <= target_us {
                current = Some(p);
            } else {
                self.pending = Some(p);
                return Ok(self.last.clone());
            }
        }

        // Decode forward (bounded) until we pass the clock.
        for _ in 0..MAX_CATCHUP_DECODE {
            match self.demux.next_video() {
                Ok(Some(f)) => {
                    if f.pts_us <= target_us {
                        current = Some(f);
                    } else {
                        self.pending = Some(f);
                        break;
                    }
                }
                Ok(None) => break,
                // EndOfStream propagates: the backend freezes on the last
                // frame (existing behavior).
                Err(e) => return Err(e),
            }
        }
        // If the whole catch-up budget still leaves us behind the clock,
        // resync the clock to the newest frame instead of melting CPU.
        if let Some(ref c) = current {
            if target_us - c.pts_us > RESYNC_BEHIND_US {
                self.play_offset_us = c.pts_us.max(0) as u64;
                self.play_start_us = Some(now);
            }
            self.last = Some(c.clone());
        }
        Ok(current.or_else(|| self.last.clone()))
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
        let path_str = std::env::temp_dir()
            .join("sf-capture-test.mp4")
            .to_string_lossy()
            .into_owned();
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
                path_str.as_str(),
            ])
            .status()
            .expect("ffmpeg CLI must be installed");
        assert!(st.success(), "ffmpeg failed to generate test file");
        path_str
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

    #[test]
    fn newest_drain_decodes_only_last_frame() {
        // Exercises DemuxDecoder::next_video_newest (the live capture
        // path): packets are discarded without decoding, only the newest
        // packet is decoded. Uses MJPEG (one packet == one frame), like a
        // real capture device.
        use crate::demux::DemuxDecoder;
        use std::path::PathBuf;
        use std::process::Command;
        let path = std::env::temp_dir().join("sf-capture-test-mjpeg.avi");
        let path_str = path.to_string_lossy().into_owned();
        let st = Command::new("ffmpeg")
            .args([
                "-y", "-v", "error",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30:duration=1",
                "-c:v", "mjpeg", "-q:v", "5",
                path_str.as_str(),
            ])
            .status()
            .expect("ffmpeg CLI must be installed");
        assert!(st.success(), "ffmpeg failed to generate mjpeg file");
        let ictx = ff::format::input(&PathBuf::from(&path_str)).expect("open mjpeg file");
        let mut demux = DemuxDecoder::from_input(ictx, 33_333).expect("demux");
        demux.set_live(true);
        // Drains the whole 1s file (EOF ends the packet loop), decodes only
        // the newest packet: exactly one frame comes out.
        let f = demux.next_video_newest().expect("drain").expect("frame");
        assert_eq!((f.width, f.height), (320, 240));
        // At EOF there are no more packets: clean None, not an error.
        assert!(demux.next_video_newest().expect("eof").is_none());
    }

    #[test]
    fn pause_freezes_video_and_audio_then_resumes() {
        let path = make_test_file();
        let mut p = MediaFileProducer::open(&path).expect("open test file");
        assert!(!p.is_paused());
        // First paced pull: clock just started, the earliest frame is due.
        let f0 = p.next_frame().expect("paced pull").expect("first frame");
        assert!(f0.pts_us < 100_000, "first frame pts_us={}", f0.pts_us);
        p.set_paused(true);
        assert!(p.is_paused());
        assert!(p.next_frame().expect("paused pull").is_none());
        assert!(p.next_audio().expect("paused audio").is_none());
        p.set_paused(false);
        assert!(!p.is_paused());
        // Resumes from the same position: a frame is due immediately.
        let f1 = p.next_frame().expect("resumed pull").expect("frame");
        assert!(f1.pts_us >= f0.pts_us);
    }
}
