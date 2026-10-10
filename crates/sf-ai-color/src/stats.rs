//! Per-frame statistics for AI color analysis (spec §6).
//!
//! [`compute_stats`] reduces an 8-bit RGBA frame to the small statistic set
//! every C6 feature consumes: luma percentiles, channel means, mean chroma,
//! skin fraction, and neutral-region ("gray") channel means for white balance.
//! All analysis is CPU-side on the (already downscaled, if the caller wishes)
//! frame; the GPU is never involved.

use sf_scopes::SKIN_TONE_ANGLE_DEG;

/// Rec.709 luma coefficients (kept explicit — never `dot()` — per the C4
/// decision, so CPU and shader math share one op order).
const LUMA_R: f32 = 0.2126;
const LUMA_G: f32 = 0.7152;
const LUMA_B: f32 = 0.0722;

/// Rec.709 chroma scale factors, as used by the vectorscope (`sf-scopes`).
const KB: f32 = 0.0722;
const KR: f32 = 0.2126;

/// Chroma below this is treated as achromatic for the skin gate.
const SKIN_MIN_CHROMA: f32 = 0.02;

/// A pixel counts as "neutral" for white balance when its chroma is below
/// this and its luma is safely away from clipping.
const GRAY_MAX_CHROMA: f32 = 0.08;
const GRAY_MIN_LUMA: f32 = 0.05;
const GRAY_MAX_LUMA: f32 = 0.95;

/// Per-frame statistics consumed by auto color, camera match, look suggest,
/// and the drift guard. Luma is Rec.709 on the display-referred (gamma)
/// frame, in `0.0..=1.0`.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameStats {
    /// Number of pixels analyzed.
    pub pixels: u64,
    /// Luma percentiles (`0.0..=1.0`).
    pub p1: f32,
    pub p5: f32,
    pub p50: f32,
    pub p95: f32,
    pub p99: f32,
    /// Channel means over all pixels (`0.0..=1.0`).
    pub mean_r: f32,
    pub mean_g: f32,
    pub mean_b: f32,
    /// Mean per-pixel chroma `max(r,g,b) - min(r,g,b)`.
    pub mean_chroma: f32,
    /// Mean [`skin_hue_weight`] over the frame — the fraction of the frame
    /// that looks like skin (`0.0..=1.0`).
    pub skin_fraction: f32,
    /// Channel means over low-chroma "neutral" pixels, for white balance.
    /// Falls back to the global means when the frame has no neutral pixels.
    pub gray_r: f32,
    pub gray_g: f32,
    pub gray_b: f32,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            pixels: 0,
            p1: 0.0,
            p5: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            mean_r: 0.0,
            mean_g: 0.0,
            mean_b: 0.0,
            mean_chroma: 0.0,
            skin_fraction: 0.0,
            gray_r: 0.0,
            gray_g: 0.0,
            gray_b: 0.0,
        }
    }
}

/// Vectorscope chroma angle of an RGB pixel, in degrees `0.0..360.0`.
///
/// This is **not** HSV hue: it is `atan2(Cr, Cb)` with Rec.709 `Cb`/`Cr`,
/// the same angle the vectorscope plots and the skin-tone guide
/// ([`SKIN_TONE_ANGLE_DEG`], 123°) marks. `r`, `g`, `b` are `0.0..=1.0`.
/// Achromatic pixels have no meaningful angle and map to `0.0`.
pub fn rgb_to_hue_deg(r: f32, g: f32, b: f32) -> f32 {
    let y = LUMA_R * r + LUMA_G * g + LUMA_B * b;
    let cb = (b - y) / (2.0 * (1.0 - KB));
    let cr = (r - y) / (2.0 * (1.0 - KR));
    if cb.abs() < 1e-6 && cr.abs() < 1e-6 {
        return 0.0;
    }
    let mut deg = cr.atan2(cb).to_degrees();
    if deg < 0.0 {
        deg += 360.0;
    }
    deg
}

/// Skin-tone weight for a vectorscope hue angle: `1.0` at the skin-tone guide
/// ([`SKIN_TONE_ANGLE_DEG`], 123°) with a raised-cosine falloff to `0.0` at
/// ±40°.
///
/// The raised cosine (`0.5·(1 + cos(π·d/40))`) is C¹-smooth with zero slope at
/// both the peak and the edge, so masks built from it have no hard edges —
/// neighboring pixels never jump discontinuously between 0 and 1.
pub fn skin_hue_weight(hue_deg: f32) -> f32 {
    const FALLOFF_DEG: f32 = 40.0;
    let mut d = (hue_deg - SKIN_TONE_ANGLE_DEG) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d < -180.0 {
        d += 360.0;
    }
    let d = d.abs();
    if d >= FALLOFF_DEG {
        0.0
    } else {
        0.5 * (1.0 + (std::f32::consts::PI * d / FALLOFF_DEG).cos())
    }
}

/// Compute [`FrameStats`] for an RGBA8 frame (`rgba.len() == 4*w*h`).
///
/// Percentiles use nearest-rank on the sorted luma values. Panics on a
/// length mismatch; an empty frame yields zeroed stats.
pub fn compute_stats(rgba: &[u8], width: u32, height: u32) -> FrameStats {
    let n = width as u64 * height as u64;
    assert_eq!(
        rgba.len() as u64,
        n * 4,
        "compute_stats: rgba length {} != 4*{}*{}",
        rgba.len(),
        width,
        height
    );
    if n == 0 {
        return FrameStats::default();
    }

    let mut lumas: Vec<f32> = Vec::with_capacity(n as usize);
    let (mut sum_r, mut sum_g, mut sum_b) = (0.0f64, 0.0f64, 0.0f64);
    let (mut sum_chroma, mut sum_skin) = (0.0f64, 0.0f64);
    let (mut gray_r, mut gray_g, mut gray_b) = (0.0f64, 0.0f64, 0.0f64);
    let mut gray_n: u64 = 0;

    for px in rgba.chunks_exact(4) {
        let r = px[0] as f32 / 255.0;
        let g = px[1] as f32 / 255.0;
        let b = px[2] as f32 / 255.0;
        let y = LUMA_R * r + LUMA_G * g + LUMA_B * b;
        let chroma = r.max(g).max(b) - r.min(g).min(b);
        lumas.push(y);
        sum_r += r as f64;
        sum_g += g as f64;
        sum_b += b as f64;
        sum_chroma += chroma as f64;
        let skin_w = if chroma < SKIN_MIN_CHROMA {
            0.0
        } else {
            skin_hue_weight(rgb_to_hue_deg(r, g, b))
        };
        sum_skin += skin_w as f64;
        if chroma < GRAY_MAX_CHROMA && y >= GRAY_MIN_LUMA && y <= GRAY_MAX_LUMA {
            gray_r += r as f64;
            gray_g += g as f64;
            gray_b += b as f64;
            gray_n += 1;
        }
    }

    lumas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |q: f32| -> f32 {
        let idx = ((q / 100.0) * (lumas.len() - 1) as f32).round() as usize;
        lumas[idx.min(lumas.len() - 1)]
    };
    let nf = n as f64;
    let (gr, gg, gb) = if gray_n > 0 {
        let gn = gray_n as f64;
        (
            (gray_r / gn) as f32,
            (gray_g / gn) as f32,
            (gray_b / gn) as f32,
        )
    } else {
        // No neutral pixels (e.g. a fully saturated frame): fall back to the
        // global means so WB still has a basis, however rough.
        (
            (sum_r / nf) as f32,
            (sum_g / nf) as f32,
            (sum_b / nf) as f32,
        )
    };

    FrameStats {
        pixels: n,
        p1: pct(1.0),
        p5: pct(5.0),
        p50: pct(50.0),
        p95: pct(95.0),
        p99: pct(99.0),
        mean_r: (sum_r / nf) as f32,
        mean_g: (sum_g / nf) as f32,
        mean_b: (sum_b / nf) as f32,
        mean_chroma: (sum_chroma / nf) as f32,
        skin_fraction: (sum_skin / nf) as f32,
        gray_r: gr,
        gray_g: gg,
        gray_b: gb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&[r, g, b, 255]);
        }
        v
    }

    #[test]
    fn percentiles_of_uniform_frame() {
        let s = compute_stats(&solid_frame(64, 64, 26, 26, 26), 64, 64);
        assert_eq!(s.pixels, 4096);
        let y = 26.0 / 255.0;
        for p in [s.p1, s.p5, s.p50, s.p95, s.p99] {
            assert!((p - y).abs() < 1e-6, "p={p} y={y}");
        }
        assert!((s.mean_r - y).abs() < 1e-6);
        assert_eq!(s.mean_chroma, 0.0);
        assert_eq!(s.skin_fraction, 0.0);
        // No neutral pixels? A 0.10-luma gray IS neutral (chroma 0, luma in
        // range) — gray_* must equal the global means either way.
        assert!((s.gray_r - y).abs() < 1e-6);
        assert!((s.gray_g - y).abs() < 1e-6);
        assert!((s.gray_b - y).abs() < 1e-6);
    }

    #[test]
    fn gray_falls_back_to_global_means_when_no_neutral_pixels() {
        // Fully saturated red: chroma is huge, so no pixel qualifies as
        // neutral; gray_* must fall back to the global means.
        let s = compute_stats(&solid_frame(32, 32, 255, 0, 0), 32, 32);
        assert!((s.gray_r - s.mean_r).abs() < 1e-6);
        assert!((s.gray_g - s.mean_g).abs() < 1e-6);
        assert!((s.gray_b - s.mean_b).abs() < 1e-6);
        assert!(s.gray_r > 0.99 && s.gray_g < 0.01 && s.gray_b < 0.01);
    }

    #[test]
    fn skin_patch_angle_lands_near_guide() {
        // The C4 vectorscope test's skin patch: must sit near the 123° guide.
        let deg = rgb_to_hue_deg(200.0 / 255.0, 150.0 / 255.0, 120.0 / 255.0);
        assert!(
            (deg - 128.2).abs() < 0.5,
            "skin patch vectorscope angle {deg} not near 128°"
        );
        let w = skin_hue_weight(deg);
        assert!(w > 0.85, "weight at patch angle = {w}");
    }

    #[test]
    fn hue_weight_shape() {
        assert_eq!(skin_hue_weight(123.0), 1.0);
        assert_eq!(skin_hue_weight(123.0 + 40.0), 0.0);
        assert_eq!(skin_hue_weight(123.0 - 40.0), 0.0);
        assert_eq!(skin_hue_weight(0.0), 0.0);
        let mid = skin_hue_weight(123.0 + 20.0);
        assert!(mid > 0.0 && mid < 1.0, "mid-falloff weight = {mid}");
        // Wraps around 0/360.
        assert_eq!(skin_hue_weight(123.0 + 360.0), 1.0);
    }

    #[test]
    fn achromatic_pixels_have_no_skin_weight() {
        assert_eq!(rgb_to_hue_deg(0.5, 0.5, 0.5), 0.0);
        let s = compute_stats(&solid_frame(16, 16, 128, 128, 128), 16, 16);
        assert_eq!(s.skin_fraction, 0.0);
    }

    #[test]
    fn empty_frame_yields_zeros() {
        let s = compute_stats(&[], 0, 0);
        assert_eq!(s.pixels, 0);
        assert_eq!(s.p50, 0.0);
    }
}
