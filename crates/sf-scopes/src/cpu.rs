//! CPU reference implementations of every scope (f32).
//!
//! These are the exact math contract the WGSL shaders in `src/shaders/`
//! follow: same formulas, same operation order, same quantization, so that
//! GPU-vs-CPU agreement tests can assert lockstep. The shaders must not be
//! "fixed" independently — if the math changes, change both sides together.

use super::{
    FalseColorImage, HistogramData, VectorscopeData, WaveformData, ZebraOverlay, SCOPE_H, SCOPE_W,
    VSCOPE_BINS,
};

/// Rec.709 luma weights.
///
/// Written as an explicit expansion in the same operand order as the WGSL
/// `luma()` helper in the scope shaders (which deliberately avoids `dot()`
/// — see the shader comment). Rust never fuses these into FMA, so the f32
/// result is bit-identical on non-fusing backends.
pub fn luma_of(c: [f32; 3]) -> f32 {
    c[0] * 0.2126 + c[1] * 0.7152 + c[2] * 0.0722
}

/// Box downscale of an RGBA-stripped f32 frame to 256x144.
///
/// Each output pixel averages 4 taps at the 1/4 and 3/4 points of its source
/// rect (floored to texel centers, clamped) — exactly what
/// `shaders/downscale.wgsl` does.
pub fn downscale_box(src: &[[f32; 3]], sw: u32, sh: u32) -> Vec<[f32; 3]> {
    assert_eq!(src.len(), (sw * sh) as usize, "src size mismatch");
    let (dw, dh) = (SCOPE_W as f32, SCOPE_H as f32);
    let mut out = Vec::with_capacity((SCOPE_W * SCOPE_H) as usize);
    for y in 0..SCOPE_H {
        for x in 0..SCOPE_W {
            let fx0 = x as f32 * sw as f32 / dw;
            let fx1 = (x + 1) as f32 * sw as f32 / dw;
            let fy0 = y as f32 * sh as f32 / dh;
            let fy1 = (y + 1) as f32 * sh as f32 / dh;
            let mut acc = [0.0f32; 3];
            for oy in [0.25f32, 0.75] {
                for ox in [0.25f32, 0.75] {
                    let sx = fx0 + ox * (fx1 - fx0);
                    let sy = fy0 + oy * (fy1 - fy0);
                    let ix = sx.floor().clamp(0.0, sw as f32 - 1.0) as usize;
                    let iy = sy.floor().clamp(0.0, sh as f32 - 1.0) as usize;
                    let p = src[iy * sw as usize + ix];
                    acc[0] += p[0];
                    acc[1] += p[1];
                    acc[2] += p[2];
                }
            }
            out.push([acc[0] * 0.25, acc[1] * 0.25, acc[2] * 0.25]);
        }
    }
    out
}

/// Waveform scan over a 256x144 f32 frame: per-column min/max of luma and RGB.
pub fn waveform(src: &[[f32; 3]]) -> WaveformData {
    assert_eq!(src.len(), (SCOPE_W * SCOPE_H) as usize);
    let mut d = WaveformData::empty();
    for y in 0..SCOPE_H as usize {
        for x in 0..SCOPE_W as usize {
            let c = src[y * SCOPE_W as usize + x];
            let l = luma_of(c);
            d.luma_min[x] = d.luma_min[x].min(l);
            d.luma_max[x] = d.luma_max[x].max(l);
            d.r_min[x] = d.r_min[x].min(c[0]);
            d.r_max[x] = d.r_max[x].max(c[0]);
            d.g_min[x] = d.g_min[x].min(c[1]);
            d.g_max[x] = d.g_max[x].max(c[1]);
            d.b_min[x] = d.b_min[x].min(c[2]);
            d.b_max[x] = d.b_max[x].max(c[2]);
        }
    }
    d
}

/// Vectorscope: 128x128 bins over normalized (Cb, Cr), Rec.709.
///
/// `Cb = (B - Y) / (2(1 - Kb))`, `Cr = (R - Y) / (2(1 - Kr))`, binned from
/// the nominal [-0.5, 0.5] range and clamped to the grid edges — identical
/// to `shaders/vectorscope.wgsl`.
pub fn vectorscope(src: &[[f32; 3]]) -> VectorscopeData {
    assert_eq!(src.len(), (SCOPE_W * SCOPE_H) as usize);
    let n = VSCOPE_BINS as usize;
    let mut bins = vec![0u32; n * n];
    for &c in src {
        let y = luma_of(c);
        let cb = (c[2] - y) / (2.0 * (1.0 - 0.0722));
        let cr = (c[0] - y) / (2.0 * (1.0 - 0.2126));
        let bx = ((cb + 0.5) * VSCOPE_BINS as f32)
            .floor()
            .clamp(0.0, (VSCOPE_BINS - 1) as f32) as usize;
        let by = ((cr + 0.5) * VSCOPE_BINS as f32)
            .floor()
            .clamp(0.0, (VSCOPE_BINS - 1) as f32) as usize;
        bins[by * n + bx] += 1;
    }
    VectorscopeData { bins }
}

/// Histogram: 256 bins each for R, G, B and luma.
///
/// Bin `i` counts values in `[i/256, (i+1)/256)`; 1.0 lands in bin 255 —
/// identical to `shaders/histogram.wgsl`.
pub fn histogram(src: &[[f32; 3]]) -> HistogramData {
    assert_eq!(src.len(), (SCOPE_W * SCOPE_H) as usize);
    let bin = |v: f32| (v * 256.0).floor().clamp(0.0, 255.0) as usize;
    let mut d = HistogramData {
        r: [0; 256],
        g: [0; 256],
        b: [0; 256],
        luma: [0; 256],
    };
    for &c in src {
        d.r[bin(c[0])] += 1;
        d.g[bin(c[1])] += 1;
        d.b[bin(c[2])] += 1;
        d.luma[bin(luma_of(c))] += 1;
    }
    d
}

/// False-color mapping of one luma value (stops from middle gray).
///
/// See [`FalseColorImage`] for the band table. Matches
/// `shaders/falsecolor.wgsl` exactly.
pub fn false_color_pixel(l: f32) -> [u8; 3] {
    let s = (l.max(1e-4) / 0.18).log2();
    if s >= 2.42 {
        [255, 0, 0]
    } else if s >= 1.42 {
        [255, 255, 0]
    } else if s >= 0.5 {
        [0, 255, 0]
    } else if s >= -0.5 {
        [128, 128, 128]
    } else if s >= -1.5 {
        [0, 255, 255]
    } else {
        [0, 0, 255]
    }
}

/// False-color exposure image over a 256x144 f32 frame.
pub fn false_color(src: &[[f32; 3]]) -> FalseColorImage {
    assert_eq!(src.len(), (SCOPE_W * SCOPE_H) as usize);
    let mut rgb = Vec::with_capacity((SCOPE_W * SCOPE_H * 3) as usize);
    for &c in src {
        rgb.extend_from_slice(&false_color_pixel(luma_of(c)));
    }
    FalseColorImage {
        width: SCOPE_W,
        height: SCOPE_H,
        rgb,
    }
}

/// Zebra overlay over a 256x144 f32 frame.
///
/// Pixels with luma above `hi` or below `lo` get diagonal stripes
/// (`(x + y) % 16 < 8`): white at 0.75 alpha on stripe cells, black at 0.75
/// alpha between them; everything else is fully transparent. Matches
/// `shaders/zebra.wgsl` exactly.
pub fn zebra(src: &[[f32; 3]], hi: f32, lo: f32) -> ZebraOverlay {
    assert_eq!(src.len(), (SCOPE_W * SCOPE_H) as usize);
    let mut rgba = Vec::with_capacity((SCOPE_W * SCOPE_H * 4) as usize);
    for (i, &c) in src.iter().enumerate() {
        let l = luma_of(c);
        let x = (i % SCOPE_W as usize) as u32;
        let y = (i / SCOPE_W as usize) as u32;
        if l > hi || l < lo {
            let stripe = (x + y) % 16 < 8;
            let v = if stripe { 255u8 } else { 0u8 };
            rgba.extend_from_slice(&[v, v, v, 191]);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    ZebraOverlay {
        width: SCOPE_W,
        height: SCOPE_H,
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScopeKind;

    fn solid_256x144(v: [f32; 3]) -> Vec<[f32; 3]> {
        vec![v; (SCOPE_W * SCOPE_H) as usize]
    }

    #[test]
    fn luma_weights_match_grade_shader() {
        // Rec.709: pure primaries give their weights back.
        assert!((luma_of([1.0, 0.0, 0.0]) - 0.2126).abs() < 1e-6);
        assert!((luma_of([0.0, 1.0, 0.0]) - 0.7152).abs() < 1e-6);
        assert!((luma_of([0.0, 0.0, 1.0]) - 0.0722).abs() < 1e-6);
        assert!((luma_of([1.0, 1.0, 1.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn downscale_identity_on_1to1() {
        // 256x144 -> 256x144: every tap lands inside its own texel.
        let src: Vec<[f32; 3]> = (0..(256 * 144))
            .map(|i| [(i % 256) as f32 / 255.0, (i / 256) as f32 / 143.0, 0.5])
            .collect();
        let out = downscale_box(&src, 256, 144);
        for (a, b) in out.iter().zip(src.iter()) {
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 1e-6, "1:1 downscale must be exact");
            }
        }
    }

    #[test]
    fn downscale_averages_2x2() {
        // 2x2 -> 256x144 upscale: each dst rect sits inside one src texel.
        let src = vec![[0.2, 0.4, 0.6]; 4];
        let out = downscale_box(&src, 2, 2);
        assert_eq!(out.len(), 256 * 144);
        for p in &out {
            assert!((p[0] - 0.2).abs() < 1e-6);
        }
    }

    #[test]
    fn waveform_solid_frame() {
        let d = waveform(&solid_256x144([0.25, 0.5, 0.75]));
        for x in 0..256 {
            assert_eq!(d.luma_min[x], d.luma_max[x]);
            assert!((d.luma_min[x] - luma_of([0.25, 0.5, 0.75])).abs() < 1e-6);
            assert_eq!((d.r_min[x], d.r_max[x]), (0.25, 0.25));
        }
    }

    #[test]
    fn vectorscope_counts_all_pixels() {
        let d = vectorscope(&solid_256x144([0.8, 0.2, 0.2]));
        let total: u32 = d.bins.iter().sum();
        assert_eq!(total, 256 * 144);
        // A red patch must land in exactly one bin.
        assert_eq!(d.bins.iter().filter(|&&b| b > 0).count(), 1);
    }

    #[test]
    fn histogram_solid_frame_single_bins() {
        let d = histogram(&solid_256x144([1.0, 0.0, 0.5]));
        assert_eq!(d.r[255], 256 * 144);
        assert_eq!(d.g[0], 256 * 144);
        assert_eq!(d.b[128], 256 * 144);
        assert_eq!(d.r.iter().sum::<u32>(), 256 * 144);
    }

    #[test]
    fn false_color_band_table() {
        // Interior of each band (well clear of boundaries).
        assert_eq!(false_color_pixel(1.0), [255, 0, 0]); // s = 2.47
        assert_eq!(false_color_pixel(0.9), [255, 255, 0]); // s = 2.32
        assert_eq!(false_color_pixel(0.36), [0, 255, 0]); // s = 1.0
        assert_eq!(false_color_pixel(0.18), [128, 128, 128]); // s = 0
        assert_eq!(false_color_pixel(0.10), [0, 255, 255]); // s = -0.85
        assert_eq!(false_color_pixel(0.01), [0, 0, 255]); // s = -4.17
        assert_eq!(false_color_pixel(0.0), [0, 0, 255]); // clamped
    }

    #[test]
    fn zebra_stripe_pattern_and_thresholds() {
        // Overexposed white frame: every pixel striped.
        let z = zebra(&solid_256x144([1.0, 1.0, 1.0]), 0.95, 0.05);
        assert_eq!(z.rgba.len(), 256 * 144 * 4);
        // (0,0): stripe cell -> white @191; (8,0): gap -> black @191.
        assert_eq!(&z.rgba[0..4], &[255, 255, 255, 191]);
        assert_eq!(&z.rgba[8 * 4..8 * 4 + 4], &[0, 0, 0, 191]);
        // (0,1): (0+1)%16<8 -> stripe.
        assert_eq!(&z.rgba[256 * 4..256 * 4 + 4], &[255, 255, 255, 191]);
        // Mid-gray frame: fully transparent.
        let z2 = zebra(&solid_256x144([0.5, 0.5, 0.5]), 0.95, 0.05);
        assert!(z2.rgba.iter().all(|&b| b == 0));
        // Underexposed: striped too.
        let z3 = zebra(&solid_256x144([0.02, 0.02, 0.02]), 0.95, 0.05);
        assert_eq!(&z3.rgba[0..4], &[255, 255, 255, 191]);
    }

    #[test]
    fn scope_kind_parse_roundtrip() {
        for k in ScopeKind::all() {
            assert_eq!(ScopeKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ScopeKind::parse("nope"), None);
        assert_eq!(ScopeKind::parse("Waveform"), None);
    }
}
