//! What a camera / capture card can actually deliver.
//!
//! DirectShow reports each output mode of a device's capture pin as
//! "format + size range + frame-rate range". FFmpeg prints that list when a
//! `dshow` input is opened with `list_options=true`:
//!
//! ```text
//!  Pin "Capture" (alternative pin name "0")
//!   vcodec=mjpeg  min s=1920x1080 fps=5 max s=1920x1080 fps=30
//!   pixel_format=yuyv422  min s=640x480 fps=5 max s=640x480 fps=30 (tv, bt470bg/bt709/unknown, topleft)
//! ```
//!
//! This module turns that text into [`CameraMode`]s. It is plain parsing
//! (no FFmpeg, no Windows), so it is unit-tested on every platform.

/// One mode a device advertises.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraMode {
    /// `mjpeg`, `h264` (compressed) or a raw pixel format such as
    /// `yuyv422`, `nv12`, `bgr24`.
    pub format: String,
    pub min_width: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub min_fps: f32,
    pub max_fps: f32,
}

impl CameraMode {
    /// A fixed-size mode (the usual case for USB cameras).
    pub fn is_fixed_size(&self) -> bool {
        self.min_width == self.max_width && self.min_height == self.max_height
    }

    /// The mode can deliver `w`×`h`.
    pub fn fits_size(&self, w: u32, h: u32) -> bool {
        (self.min_width..=self.max_width).contains(&w) && (self.min_height..=self.max_height).contains(&h)
    }

    /// The mode can deliver `fps` (with a little slack for drivers that
    /// report 29.97 or 30.0003 for "30").
    pub fn fits_fps(&self, fps: f32) -> bool {
        fps >= self.min_fps - 0.6 && fps <= self.max_fps + 0.6
    }

    /// Compressed formats are decoded on the CPU; raw formats need more USB
    /// bandwidth but no decode.
    pub fn is_compressed(&self) -> bool {
        matches!(self.format.as_str(), "mjpeg" | "h264" | "hevc")
    }
}

fn size_after(s: &str, key: &str) -> Option<(u32, u32)> {
    let rest = &s[s.find(key)? + key.len()..];
    let tok = rest.split_whitespace().next()?;
    let (w, h) = tok.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn fps_after(s: &str) -> Option<f32> {
    let rest = &s[s.find("fps=")? + 4..];
    rest.split_whitespace().next()?.parse().ok()
}

/// Parse FFmpeg's `dshow` `list_options` output into modes (duplicates
/// removed, largest picture and highest rate first).
pub fn parse_dshow_options(log: &str) -> Vec<CameraMode> {
    let mut out: Vec<CameraMode> = Vec::new();
    for line in log.lines() {
        let Some(min_at) = line.find("min s=") else {
            continue;
        };
        let head = &line[..min_at];
        let format = ["vcodec=", "pixel_format="].iter().find_map(|k| {
            let i = head.find(k)?;
            head[i + k.len()..].split_whitespace().next().map(str::to_string)
        });
        let Some(format) = format.filter(|f| !f.is_empty() && f != "(null)") else {
            continue;
        };
        let tail = &line[min_at..];
        let Some(max_at) = tail.find("max s=") else {
            continue;
        };
        let (min_part, max_part) = tail.split_at(max_at);
        let (Some((min_w, min_h)), Some(min_fps), Some((max_w, max_h)), Some(max_fps)) =
            (size_after(min_part, "min s="), fps_after(min_part), size_after(max_part, "max s="), fps_after(max_part))
        else {
            continue;
        };
        if min_w == 0 || min_h == 0 || max_w == 0 || max_h == 0 || !(max_fps > 0.0) {
            continue;
        }
        let m = CameraMode {
            format,
            min_width: min_w,
            min_height: min_h,
            max_width: max_w,
            max_height: max_h,
            min_fps: min_fps.min(max_fps),
            max_fps: max_fps.max(min_fps),
        };
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out.sort_by(|a, b| {
        (b.max_width * b.max_height)
            .cmp(&(a.max_width * a.max_height))
            .then(b.max_fps.partial_cmp(&a.max_fps).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.format.cmp(&b.format))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "DirectShow video device options (from video devices)\n Pin \"Capture\" (alternative pin name \"0\")\n  vcodec=mjpeg  min s=1920x1080 fps=5 max s=1920x1080 fps=30\n  vcodec=mjpeg  min s=1920x1080 fps=5 max s=1920x1080 fps=30 (tv, bt470bg/bt709/unknown, topleft)\n  vcodec=mjpeg  min s=1280x720 fps=5 max s=1280x720 fps=60.0002\n  pixel_format=yuyv422  min s=640x480 fps=5 max s=640x480 fps=30\n  pixel_format=nv12  min s=320x240 fps=7.5 max s=1920x1080 fps=29.97\n  unknown compression type 0x3231564E\n";

    #[test]
    fn parses_formats_sizes_and_rates() {
        let m = parse_dshow_options(LOG);
        assert_eq!(m.len(), 4, "duplicate (colour-info) line removed: {m:?}");
        assert_eq!(m[0].format, "mjpeg");
        assert_eq!((m[0].max_width, m[0].max_height, m[0].max_fps), (1920, 1080, 30.0));
        let nv12 = m.iter().find(|x| x.format == "nv12").unwrap();
        assert!(!nv12.is_fixed_size());
        assert!(nv12.fits_size(1280, 720));
        assert!(nv12.fits_fps(29.97) && nv12.fits_fps(30.0) && !nv12.fits_fps(60.0));
        let hd60 = m.iter().find(|x| x.max_width == 1280).unwrap();
        assert!(hd60.fits_fps(60.0) && hd60.is_compressed());
        let yuy = m.iter().find(|x| x.format == "yuyv422").unwrap();
        assert!(yuy.is_fixed_size() && !yuy.is_compressed());
    }

    #[test]
    fn ignores_noise_and_empty_logs() {
        assert!(parse_dshow_options("").is_empty());
        assert!(parse_dshow_options("Could not enumerate video devices\n  min s=bad").is_empty());
    }
}
