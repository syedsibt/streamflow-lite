//! Still images (PNG, JPEG, WebP, BMP, TIFF, GIF first frame) decoded to
//! straight-alpha RGBA8 through FFmpeg's image decoders. An image is a
//! source of its own — decoded once, never run through the video pipeline.

use ffmpeg_next as ff;

use super::CaptureError;

/// Longest side kept in memory; larger images are scaled down once at load
/// (a 4K canvas never needs more).
pub const MAX_IMAGE_SIDE: u32 = 4096;

#[derive(Clone, Debug)]
pub struct StillImage {
    pub width: u32,
    pub height: u32,
    /// RGBA8, straight (not premultiplied) alpha, row-major, no padding.
    pub data: Vec<u8>,
    /// Any pixel with alpha < 255.
    pub has_alpha: bool,
}

/// Decodes the first picture of an image file.
pub fn decode_image_rgba(path: &str) -> Result<StillImage, CaptureError> {
    super::demux::ensure_init();
    let mut ictx = ff::format::input(&path)
        .map_err(|e| CaptureError::OpenFailed(format!("cannot open image '{path}': {e}")))?;
    let stream = ictx
        .streams()
        .best(ff::media::Type::Video)
        .ok_or_else(|| CaptureError::OpenFailed(format!("'{path}' is not an image")))?;
    let idx = stream.index();
    let mut dec = ff::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| CaptureError::OpenFailed(format!("image codec: {e}")))?
        .decoder()
        .video()
        .map_err(|e| CaptureError::OpenFailed(format!("image decoder: {e}")))?;
    let mut frame = ff::util::frame::video::Video::empty();
    let mut got = false;
    for (s, pkt) in ictx.packets() {
        if s.index() != idx {
            continue;
        }
        if dec.send_packet(&pkt).is_ok() && dec.receive_frame(&mut frame).is_ok() {
            got = true;
            break;
        }
    }
    if !got {
        let _ = dec.send_eof();
        got = dec.receive_frame(&mut frame).is_ok();
    }
    if !got {
        return Err(CaptureError::ReadFailed(format!("could not decode an image from '{path}'")));
    }
    let (w, h) = (frame.width(), frame.height());
    if w == 0 || h == 0 {
        return Err(CaptureError::ReadFailed(format!("'{path}' has no pixels")));
    }
    let k = (MAX_IMAGE_SIDE as f64 / w.max(h) as f64).min(1.0);
    let (ow, oh) = (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1));
    let mut sws = ff::software::scaling::Context::get(
        frame.format(),
        w,
        h,
        ff::format::Pixel::RGBA,
        ow,
        oh,
        ff::software::scaling::Flags::BICUBIC,
    )
    .map_err(|e| CaptureError::ReadFailed(format!("image convert setup: {e}")))?;
    let mut rgba = ff::util::frame::video::Video::empty();
    sws.run(&frame, &mut rgba)
        .map_err(|e| CaptureError::ReadFailed(format!("image convert: {e}")))?;
    let stride = rgba.stride(0);
    let w4 = ow as usize * 4;
    let src = rgba.data(0);
    let mut data = vec![0u8; w4 * oh as usize];
    for y in 0..oh as usize {
        data[y * w4..y * w4 + w4].copy_from_slice(&src[y * stride..y * stride + w4]);
    }
    let has_alpha = data.chunks_exact(4).any(|p| p[3] < 255);
    Ok(StillImage { width: ow, height: oh, data, has_alpha })
}

/// Resizes straight-alpha RGBA with premultiplied filtering, so transparent
/// edges don't pick up the (invisible) colour of transparent pixels.
pub fn resize_rgba_premultiplied(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw.max(1) as usize, sh.max(1) as usize, dw.max(1) as usize, dh.max(1) as usize);
    let mut out = vec![0u8; dw * dh * 4];
    if src.len() < sw * sh * 4 {
        return out;
    }
    // Area-average when shrinking, bilinear when enlarging (per axis).
    let sx = sw as f32 / dw as f32;
    let sy = sh as f32 / dh as f32;
    for y in 0..dh {
        let (y0, y1) = if sy > 1.0 {
            ((y as f32 * sy) as usize, (((y + 1) as f32 * sy).ceil() as usize).min(sh).max((y as f32 * sy) as usize + 1))
        } else {
            let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
            (fy as usize, (fy as usize + 2).min(sh))
        };
        for x in 0..dw {
            let (x0, x1) = if sx > 1.0 {
                ((x as f32 * sx) as usize, (((x + 1) as f32 * sx).ceil() as usize).min(sw).max((x as f32 * sx) as usize + 1))
            } else {
                let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
                (fx as usize, (fx as usize + 2).min(sw))
            };
            // Weights: box for shrink; tent for enlarge.
            let (fxc, fyc) = (((x as f32 + 0.5) * sx - 0.5).max(0.0), ((y as f32 + 0.5) * sy - 0.5).max(0.0));
            let mut acc = [0f32; 4];
            let mut wsum = 0f32;
            for yy in y0..y1 {
                let wy = if sy > 1.0 { 1.0 } else { (1.0 - (yy as f32 - fyc).abs()).max(0.0) };
                for xx in x0..x1 {
                    let wx = if sx > 1.0 { 1.0 } else { (1.0 - (xx as f32 - fxc).abs()).max(0.0) };
                    let wgt = wx * wy;
                    if wgt <= 0.0 {
                        continue;
                    }
                    let p = &src[(yy * sw + xx) * 4..(yy * sw + xx) * 4 + 4];
                    let a = p[3] as f32 / 255.0;
                    acc[0] += p[0] as f32 * a * wgt;
                    acc[1] += p[1] as f32 * a * wgt;
                    acc[2] += p[2] as f32 * a * wgt;
                    acc[3] += a * wgt;
                    wsum += wgt;
                }
            }
            let d = &mut out[(y * dw + x) * 4..(y * dw + x) * 4 + 4];
            if wsum > 0.0 && acc[3] > 0.0 {
                let a = acc[3] / wsum;
                for c in 0..3 {
                    d[c] = (acc[c] / acc[3]).round().clamp(0.0, 255.0) as u8;
                }
                d[3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &std::path::Path) {
        // 4x2: left half opaque red, right half fully transparent green.
        let mut px = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                px.extend_from_slice(if x < 2 { &[255, 0, 0, 255] } else { &[0, 255, 0, 0] });
            }
        }
        let raw = path.with_extension("rgba");
        std::fs::write(&raw, &px).unwrap();
        let st = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", "4x2", "-i"])
            .arg(&raw)
            .args(["-frames:v", "1", "-pix_fmt", "rgba"])
            .arg(path)
            .status()
            .unwrap();
        assert!(st.success());
    }

    #[test]
    fn png_keeps_transparency() {
        let p = std::env::temp_dir().join("sf-still-alpha.png");
        write_png(&p);
        let img = decode_image_rgba(p.to_str().unwrap()).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert!(img.has_alpha);
        assert_eq!(&img.data[0..4], &[255, 0, 0, 255]);
        assert_eq!(img.data[3 * 4 + 3], 0, "transparent pixel stays transparent");
    }

    #[test]
    fn jpeg_and_webp_decode() {
        for ext in ["jpg", "webp", "bmp"] {
            let p = std::env::temp_dir().join(format!("sf-still.{ext}"));
            let st = std::process::Command::new("ffmpeg")
                .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:duration=0.1", "-frames:v", "1"])
                .arg(&p)
                .status()
                .unwrap();
            if !st.success() {
                eprintln!("ffmpeg cannot write .{ext} here; skipped");
                continue;
            }
            let img = decode_image_rgba(p.to_str().unwrap()).unwrap_or_else(|e| panic!("{ext}: {e}"));
            assert_eq!((img.width, img.height), (320, 180), "{ext}");
            assert!(!img.has_alpha, "{ext}");
        }
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(decode_image_rgba("/nonexistent/logo.png").is_err());
    }

    #[test]
    fn premultiplied_resize_has_no_dark_fringe() {
        // Opaque white next to transparent BLACK: a naive filter greys the edge.
        let mut px = Vec::new();
        for x in 0..8 {
            px.extend_from_slice(if x < 4 { &[255, 255, 255, 255] } else { &[0, 0, 0, 0] });
        }
        let out = resize_rgba_premultiplied(&px, 8, 1, 3, 1);
        for p in out.chunks_exact(4) {
            if p[3] > 0 {
                assert_eq!(&p[..3], &[255, 255, 255], "edge colour must stay white: {p:?}");
            }
        }
        let up = resize_rgba_premultiplied(&px, 8, 1, 16, 1);
        assert_eq!(up.len(), 64);
    }
}
