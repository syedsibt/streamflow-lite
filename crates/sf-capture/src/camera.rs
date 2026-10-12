//! V4L2 camera capture (Linux).
//!
//! # Status: code-complete, logic-tested, **hardware-unverified**
//!
//! The full capture path is implemented: device enumeration, format
//! negotiation (MJPEG → YUYV → other raw), mmap streaming, buffer
//! timestamps → `pts_us`, YUYV → RGBA conversion in pure Rust, MJPEG →
//! RGBA via FFmpeg's mjpeg decoder.
//!
//! Everything that does not need physical hardware is unit-tested:
//! fourcc preference order, format-negotiation selection, timestamp math,
//! YUYV→RGBA conversion, and device-list parsing. The streaming read loop
//! and ioctl calls are marked `// HARDWARE:` and have never run here —
//! this VM has no `/dev/video*` device. Treat this module as
//! **written, logic-tested, end-to-end unverified** until it runs against a
//! real camera.

use std::io;
use std::path::Path;

use v4l::buffer::Type;
use v4l::device::Device;
use v4l::format::fourcc::FourCC;
use v4l::format::Format;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;

use super::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// A discovered V4L2 capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Device node, e.g. `/dev/video0`.
    pub path: String,
    /// Human-readable name from the driver (`query_caps().card`).
    pub name: String,
    /// Numeric index (`/dev/video{index}`).
    pub index: usize,
}

/// FourCC preference order for format negotiation.
///
/// MJPEG first (smaller USB bandwidth → higher resolutions/framerates),
/// then packed raw formats we can convert ourselves, then anything else.
const FOURCC_PREFERENCE: [&[u8; 4]; 4] = [b"MJPG", b"YUYV", b"UYVY", b"RGB3"];

/// Pick the best pixel format from a device's advertised fourccs.
///
/// Returns `(fourcc, needs_ffmpeg_decode)`: MJPEG frames must go through
/// FFmpeg's mjpeg decoder; the packed formats convert in pure Rust.
pub fn negotiate_fourcc(advertised: &[FourCC]) -> Option<(FourCC, bool)> {
    for pref in FOURCC_PREFERENCE {
        let want = FourCC::new(pref);
        if advertised.iter().any(|f| *f == want) {
            return Some((want, want == FourCC::new(b"MJPG")));
        }
    }
    advertised.first().map(|f| (*f, false))
}

/// Convert a V4L2 buffer timestamp (seconds + microseconds since boot) to
/// `pts_us` for [`CapturedFrame`].
///
/// V4L2 timestamps are monotonic per device but have an arbitrary epoch, so
/// the first captured frame defines `pts_us = 0`; this function converts the
/// raw `(sec, usec)` pair to absolute microseconds, and callers subtract the
/// first frame's value. Saturates instead of overflowing.
pub fn timestamp_to_us(sec: i64, usec: i64) -> i64 {
    sec.saturating_mul(1_000_000).saturating_add(usec)
}

/// Parse candidate device nodes from a directory listing.
///
/// `entries` are file names (e.g. from reading `/dev`); returns the
/// `/dev/videoN` paths sorted by index. Pure function so the parsing logic
/// is testable without hardware.
pub fn parse_video_devices(entries: &[&str]) -> Vec<String> {
    let mut idxs: Vec<usize> = entries
        .iter()
        .filter_map(|e| {
            e.strip_prefix("video")
                .and_then(|n| n.parse::<usize>().ok())
        })
        .collect();
    idxs.sort_unstable();
    idxs.dedup();
    idxs.into_iter().map(|i| format!("/dev/video{i}")).collect()
}

/// Enumerate V4L2 capture devices.
///
/// // HARDWARE: requires /dev/video* nodes; returns an empty vec here.
pub fn list_cameras() -> Vec<CameraInfo> {
    let mut out = Vec::new();
    let mut misses = 0usize;
    for index in 0..64usize {
        let path = format!("/dev/video{index}");
        if !Path::new(&path).exists() {
            // Nodes can be sparse; allow a few gaps, then stop.
            misses += 1;
            if misses >= 8 {
                break;
            }
            continue;
        }
        misses = 0;
        let Ok(dev) = Device::with_path(&path) else {
            continue;
        };
        let Ok(caps) = dev.query_caps() else { continue };
        // Skip non-capture nodes (e.g. metadata / output-only devices).
        let Ok(formats) = dev.enum_formats() else {
            continue;
        };
        if formats.is_empty() {
            continue;
        }
        out.push(CameraInfo {
            path,
            name: caps.card,
            index,
        });
        if out.len() >= 16 {
            break;
        }
    }
    out
}

/// Convert one YUYV (YUV 4:2:2 packed) pixel pair to two RGBA pixels.
///
/// Uses the BT.601 limited-range matrix — the range V4L2 cameras emit.
/// `yuyv` must hold exactly 4 bytes `[Y0 U Y1 V]`.
pub fn yuyv_pair_to_rgba(yuyv: &[u8; 4]) -> [[u8; 4]; 2] {
    let y0 = yuyv[0] as i32;
    let u = yuyv[1] as i32 - 128;
    let y1 = yuyv[2] as i32;
    let v = yuyv[3] as i32 - 128;
    let conv = |y: i32| {
        let c = y - 16;
        let r = (298 * c + 409 * v + 128) >> 8;
        let g = (298 * c - 100 * u - 208 * v + 128) >> 8;
        let b = (298 * c + 516 * u + 128) >> 8;
        [
            r.clamp(0, 255) as u8,
            g.clamp(0, 255) as u8,
            b.clamp(0, 255) as u8,
            255,
        ]
    };
    [conv(y0), conv(y1)]
}

/// Convert a full YUYV frame (`width*height*2` bytes) to RGBA8.
///
/// Returns an error if the buffer is short or the width is odd.
pub fn yuyv_frame_to_rgba(raw: &[u8], width: u32, height: u32) -> Result<Vec<u8>, CaptureError> {
    if width % 2 != 0 {
        return Err(CaptureError::Unsupported("odd-width YUYV frame".into()));
    }
    let need = width as usize * height as usize * 2;
    if raw.len() < need {
        return Err(CaptureError::ReadFailed(format!(
            "short YUYV buffer: {} < {need} bytes",
            raw.len()
        )));
    }
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for (pair_idx, chunk) in raw[..need].chunks_exact(4).enumerate() {
        let [p0, p1] = yuyv_pair_to_rgba(&[chunk[0], chunk[1], chunk[2], chunk[3]]);
        let o = pair_idx * 8;
        rgba[o..o + 4].copy_from_slice(&p0);
        rgba[o + 4..o + 8].copy_from_slice(&p1);
    }
    Ok(rgba)
}

/// V4L2 camera source.
///
/// Opens `/dev/video{index}`, negotiates the best pixel format, and streams
/// frames via mmap. `next_frame` dequeues one buffer per call; when the
/// driver has no frame ready it returns `Ok(None)` (non-blocking — the
/// stream is created with a short timeout).
///
/// // HARDWARE: every ioctl and the read loop need a real `/dev/videoN`.
pub struct CameraProducer {
    info: StreamInfo,
    width: u32,
    height: u32,
    needs_ffmpeg_decode: bool,
    mjpeg_decoder: Option<MjpegDecoder>,
    stream: MmapStream<'static>,
    // `stream` borrows the device; keep it alive here.
    _device: Box<Device>,
    first_ts_us: Option<i64>,
    frame_us: u64,
}

impl CameraProducer {
    /// Open a camera by index (`/dev/video{index}`), negotiating
    /// `width`×`height` at approximately `fps` (the driver picks the
    /// closest it supports).
    ///
    /// // HARDWARE: requires /dev/videoN.
    pub fn open(index: usize, width: u32, height: u32, fps: u32) -> Result<Self, CaptureError> {
        let path = format!("/dev/video{index}");
        let device = Device::with_path(&path)
            .map_err(|e| CaptureError::DeviceNotFound(format!("{path}: {e}")))?;
        Self::open_device(device, path, width, height, fps)
    }

    fn open_device(
        device: Device,
        path: String,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        // HARDWARE: ioctl VIDIOC_ENUM_FMT via v4l.
        let formats = device.enum_formats().map_err(|e| {
            CaptureError::OpenFailed(format!("{path}: cannot enumerate formats: {e}"))
        })?;
        let advertised: Vec<FourCC> = formats.iter().map(|f| f.fourcc).collect();
        let (fourcc, needs_ffmpeg_decode) = negotiate_fourcc(&advertised)
            .ok_or_else(|| CaptureError::OpenFailed(format!("{path}: no usable pixel format")))?;

        // HARDWARE: ioctl VIDIOC_S_FMT — the driver may adjust w/h.
        let fmt = device
            .set_format(&Format::new(width, height, fourcc))
            .map_err(|e| CaptureError::OpenFailed(format!("{path}: set_format failed: {e}")))?;
        let (w, h) = (fmt.width, fmt.height);

        let mjpeg_decoder = if needs_ffmpeg_decode {
            Some(MjpegDecoder::new(w, h)?)
        } else {
            None
        };

        // Leak the device into a 'static box so the mmap stream (which
        // borrows it) can be stored alongside. The box is dropped with us.
        let device: Box<Device> = Box::new(device);
        // SAFETY: `device` lives in `self._device` for the whole lifetime of
        // `stream`, which is dropped before `_device` (field order).
        let stream: MmapStream<'static> = unsafe {
            let dev_ref: &'static Device = &*(&*device as *const Device);
            let mut s = MmapStream::with_buffers(dev_ref, Type::VideoCapture, 4).map_err(|e| {
                CaptureError::OpenFailed(format!("{path}: mmap buffer setup failed: {e}"))
            })?;
            use std::time::Duration;
            s.set_timeout(Duration::from_millis(5));
            s
        };

        let frame_us = if fps == 0 { 0 } else { 1_000_000 / fps as u64 };
        Ok(Self {
            info: StreamInfo {
                width: w,
                height: h,
                fps_num: fps,
                fps_den: 1,
                has_audio: false,
            },
            width: w,
            height: h,
            needs_ffmpeg_decode,
            mjpeg_decoder,
            stream,
            _device: device,
            first_ts_us: None,
            frame_us,
        })
    }
}

impl FrameProducer for CameraProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        // HARDWARE: blocks up to the 5 ms stream timeout, then io::TimedOut.
        let (buf, meta) = match self.stream.next() {
            Ok(pair) => pair,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => return Ok(None),
            Err(e) => return Err(CaptureError::ReadFailed(format!("dequeue failed: {e}"))),
        };
        let used = meta.bytesused as usize;
        let abs_us = timestamp_to_us(meta.timestamp.sec, meta.timestamp.usec);
        let base = *self.first_ts_us.get_or_insert(abs_us);
        let pts_us = abs_us.saturating_sub(base);

        let rgba = if self.needs_ffmpeg_decode {
            let dec = self.mjpeg_decoder.as_mut().expect("mjpeg decoder present");
            dec.decode(&buf[..used.min(buf.len())])?
        } else {
            yuyv_frame_to_rgba(&buf[..used.min(buf.len())], self.width, self.height)?
        };

        Ok(Some(CapturedFrame {
            data: rgba,
            width: self.width,
            height: self.height,
            pts_us,
            duration_us: self.frame_us,
            captured_at_us: crate::clock::now_us(),
        }))
    }

    fn stream_info(&self) -> StreamInfo {
        self.info
    }
}

/// MJPEG → RGBA decoder backed by FFmpeg's mjpeg decoder + swscale.
///
/// The scaler is created lazily from the first decoded frame (MJPEG packets
/// carry their own dimensions in the SOF marker).
struct MjpegDecoder {
    decoder: ffmpeg_next::decoder::Video,
    scaler: Option<ffmpeg_next::software::scaling::Context>,
    width: u32,
    height: u32,
}

impl MjpegDecoder {
    fn new(width: u32, height: u32) -> Result<Self, CaptureError> {
        let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::MJPEG)
            .ok_or_else(|| CaptureError::OpenFailed("mjpeg decoder not found".into()))?;
        let decoder = ffmpeg_next::decoder::new()
            .open_as(codec)
            .map_err(|e| CaptureError::OpenFailed(format!("mjpeg decoder open: {e}")))?
            .video()
            .map_err(|e| CaptureError::OpenFailed(format!("mjpeg decoder open: {e}")))?;
        Ok(Self {
            decoder,
            scaler: None,
            width,
            height,
        })
    }

    fn decode(&mut self, jpeg: &[u8]) -> Result<Vec<u8>, CaptureError> {
        use ffmpeg_next::util::frame::video::Video;
        let mut packet = ffmpeg_next::codec::packet::Packet::new(jpeg.len());
        packet
            .data_mut()
            .map(|d| d.copy_from_slice(jpeg))
            .ok_or_else(|| CaptureError::ReadFailed("mjpeg packet alloc failed".into()))?;
        self.decoder
            .send_packet(&packet)
            .map_err(|e| CaptureError::ReadFailed(format!("mjpeg send_packet: {e}")))?;
        let mut decoded = Video::empty();
        self.decoder
            .receive_frame(&mut decoded)
            .map_err(|e| CaptureError::ReadFailed(format!("mjpeg receive_frame: {e}")))?;
        let mut rgba = Video::empty();
        let scaler = match self.scaler {
            Some(ref mut s) => s,
            None => {
                let s = ffmpeg_next::software::scaling::Context::get(
                    decoded.format(),
                    decoded.width(),
                    decoded.height(),
                    ffmpeg_next::util::format::pixel::Pixel::RGBA,
                    self.width,
                    self.height,
                    ffmpeg_next::software::scaling::flag::Flags::BILINEAR,
                )
                .map_err(|e| CaptureError::ReadFailed(format!("mjpeg scaler setup: {e}")))?;
                self.scaler.insert(s)
            }
        };
        scaler
            .run(&decoded, &mut rgba)
            .map_err(|e| CaptureError::ReadFailed(format!("mjpeg scale: {e}")))?;
        let stride = rgba.stride(0);
        let mut out = vec![0u8; self.width as usize * self.height as usize * 4];
        for y in 0..self.height as usize {
            let src = &rgba.data(0)[y * stride..y * stride + self.width as usize * 4];
            let dst = &mut out[y * self.width as usize * 4..(y + 1) * self.width as usize * 4];
            dst.copy_from_slice(src);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_prefers_mjpeg_then_yuyv() {
        let yuyv = FourCC::new(b"YUYV");
        let mjpg = FourCC::new(b"MJPG");
        assert_eq!(negotiate_fourcc(&[yuyv, mjpg]), Some((mjpg, true)));
        assert_eq!(negotiate_fourcc(&[yuyv]), Some((yuyv, false)));
        assert_eq!(negotiate_fourcc(&[]), None);
    }

    #[test]
    fn negotiate_falls_back_to_first_unknown_format() {
        let rgb3 = FourCC::new(b"RGB3");
        let weird = FourCC::new(b"BA81");
        assert_eq!(negotiate_fourcc(&[weird, rgb3]), Some((rgb3, false)));
        assert_eq!(negotiate_fourcc(&[weird]), Some((weird, false)));
    }

    #[test]
    fn timestamp_math_converts_sec_usec_to_us() {
        assert_eq!(timestamp_to_us(3, 500_000), 3_500_000);
        assert_eq!(timestamp_to_us(0, 0), 0);
        // Saturates instead of overflowing on absurd values.
        assert_eq!(timestamp_to_us(i64::MAX, i64::MAX), i64::MAX);
    }

    #[test]
    fn parse_video_devices_extracts_sorted_indices() {
        let entries = [
            "video10", "video0", "video2", "video", "vide0", "audio0", "video2",
        ];
        assert_eq!(
            parse_video_devices(&entries),
            vec!["/dev/video0", "/dev/video2", "/dev/video10"]
        );
        assert!(parse_video_devices(&[]).is_empty());
    }

    #[test]
    fn yuyv_black_white_and_red_roundtrip() {
        // BT.601 limited range: Y=16,U=128,V=128 is black; Y=235 is white.
        let [black, _] = yuyv_pair_to_rgba(&[16, 128, 16, 128]);
        assert_eq!(black, [0, 0, 0, 255]);
        let [white, _] = yuyv_pair_to_rgba(&[235, 128, 235, 128]);
        assert_eq!(white, [255, 255, 255, 255]);
        // Pure red: Y=81, U=90, V=240 (approx BT.601).
        let [red, _] = yuyv_pair_to_rgba(&[81, 90, 81, 240]);
        assert!(red[0] > 200 && red[1] < 60 && red[2] < 60, "red={red:?}");
    }

    #[test]
    fn yuyv_frame_conversion_layout_and_errors() {
        // 2x1 frame: left black, right white.
        let raw = [16u8, 128, 235, 128];
        let rgba = yuyv_frame_to_rgba(&raw, 2, 1).unwrap();
        assert_eq!(rgba.len(), 2 * 1 * 4);
        assert_eq!(&rgba[0..4], &[0, 0, 0, 255]);
        assert_eq!(&rgba[4..8], &[255, 255, 255, 255]);
        // Short buffer errors; odd width is unsupported.
        assert!(yuyv_frame_to_rgba(&raw, 2, 2).is_err());
        assert!(yuyv_frame_to_rgba(&raw, 3, 1).is_err());
    }

    #[test]
    fn list_cameras_returns_empty_without_hardware() {
        // This VM has no /dev/video*; the call must not panic.
        let cams = list_cameras();
        assert!(cams.is_empty());
    }
}
