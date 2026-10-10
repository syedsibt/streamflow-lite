//! CPU reference implementation of the fused grade pass.
//!
//! [`apply_grade_cpu`] implements the **identical** formula chain as the WGSL
//! grade shader in `sf-render`, step by step. Any change to the chain must be
//! made in both places. The steps below are numbered so the shader author can
//! match them one-to-one.
//!
//! Conventions shared with the shader:
//!
//! * All math is `f32`. `mix(a, b, t) = a + (b - a) * t`.
//! * `smoothstep(e0, e1, x)`: `t = clamp((x - e0) / (e1 - e0), 0, 1)` then
//!   `t * t * (3 - 2 * t)`.
//! * Luma weights are Rec.709: `(0.2126, 0.7152, 0.0722)`.
//! * Normalized parameters are taken from [`ColorGrade::to_uniform`], so the
//!   CPU reference always uses exactly the values the GPU receives.
//!
//! # Deliberate differences from the GPU pass
//!
//! * **Range fix** (limited -> full): skipped; the MVP always uses full range.
//! * **Sharpness**: not modeled — it is a spatial (unsharp mask) operation and
//!   has no per-pixel CPU equivalent in this reference. GPU-only.

use crate::curves::BakedCurves;
use crate::grade::{ColorGrade, LutPosition};
use crate::lut::{trilinear_sample, CubeFile};

/// Rec.709 luma weights.
const LUM: [f32; 3] = [0.2126, 0.7152, 0.0722];

fn mixf(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn fract(x: f32) -> f32 {
    x - x.floor()
}

/// RGB (0..1) -> HSV, with hue in 0..1.
///
/// NOTE: the hue encoding follows the shared pseudocode exactly and is *not*
/// standard colorimetric HSV — the red-max branch omits the usual `/6`, so
/// e.g. yellow maps to hue 0 rather than 1/6. The WGSL shader implements the
/// same branches, so CPU and GPU agree; do not "fix" one side independently.
///
/// Branch structure (mirrors the WGSL):
/// ```text
/// mx = max(r,g,b); mn = min(r,g,b); d = mx - mn; h = 0
/// if d > 1e-6:
///     if mx == r: h = fract((g - b) / d + 1.0)
///     elif mx == g: h = (b - r) / d + 1/3
///     else: h = (r - g) / d + 2/3
/// s = mx > 1e-6 ? d / mx : 0
/// return (fract(h), s, mx)
/// ```
pub fn rgb_to_hsv(rgb: [f32; 3]) -> [f32; 3] {
    let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = mx - mn;
    let mut h = 0.0;
    if d > 1e-6 {
        if mx == r {
            h = fract((g - b) / d + 1.0);
        } else if mx == g {
            h = (b - r) / d + 1.0 / 3.0;
        } else {
            h = (r - g) / d + 2.0 / 3.0;
        }
    }
    let s = if mx > 1e-6 { d / mx } else { 0.0 };
    [fract(h), s, mx]
}

/// HSV (hue 0..1) -> RGB.
///
/// Branch structure (mirrors the WGSL):
/// ```text
/// k = fract(h + (0, 2/3, 1/3)) * 6 - 3
/// p = clamp(abs(k) - 1, 0, 1)
/// return v * mix((1,1,1), p, s)
/// ```
pub fn hsv_to_rgb(hsv: [f32; 3]) -> [f32; 3] {
    let (h, s, v) = (hsv[0], hsv[1], hsv[2]);
    let offsets = [0.0, 2.0 / 3.0, 1.0 / 3.0];
    let mut out = [0.0f32; 3];
    for i in 0..3 {
        let k = fract(h + offsets[i]) * 6.0 - 3.0;
        let p = (k.abs() - 1.0).clamp(0.0, 1.0);
        out[i] = v * mixf(1.0, p, s);
    }
    out
}

/// Apply the full grade chain to one pixel (RGB, 0..1) on the CPU.
///
/// Returns the input **bit-exact** when [`ColorGrade::is_identity`] is true,
/// mirroring the engine bypassing the color pass entirely.
///
/// `baked` is the [`CurveSet::bake`](crate::curves::CurveSet::bake) output;
/// `lut` is the parsed [`CubeFile`] referenced by `grade.lut` (if any).
///
/// Chain (each step mirrors the WGSL shader):
/// ```text
///  0. identity bypass -> return pixel unchanged
///  1. c = clamp(pixel, 0, 1); c = pow(c, 2.4)              // linearize
///  2. if LUT enabled and position == Before:
///         c = mix(c, trilinear(lut, clamp(c,0,1)), intensity)
///  3. c *= 2^exposure
///  4. c *= wb_gain                                        // temperature+tint
///  5. c += lift.rgb + lift.master
///     c  = pow(max(c,0), 1/gamma_w.rgb); c = pow(max(c,0), 1/gamma_w.master)
///     c *= gain.rgb * gain.master
///     c += offset.rgb + offset.master
///  6. tone: contrast pivot 0.18; brightness; highlights/shadows/whites/blacks
///     weighted by smoothstep windows of luma l (computed after step 5)
///  7. c = pow(max(c,0), gamma)                             // basic gamma
///  8. saturation around luma l2, with skin-tone protection mask m
///  9. vibrance scaled by (1 - chroma), skin-limited when boosted
/// 10. curves (if any non-identity): per-channel, master, luma delta,
///     hue-vs-sat in HSV
/// 11. if LUT enabled and position == After:
///         c = mix(c, trilinear(lut, clamp(c,0,1)), intensity)
/// 12. sharpness: SKIPPED (spatial op, GPU-only)
/// 13. c = pow(max(c,0), 1/2.4); c = clamp(c, 0, 1)         // output encode
/// ```
pub fn apply_grade_cpu(
    grade: &ColorGrade,
    baked: &BakedCurves,
    lut: Option<&CubeFile>,
    pixel: [f32; 3],
) -> [f32; 3] {
    // 0. Identity bypass — mirrors the engine skipping the color pass.
    if grade.is_identity() {
        return pixel;
    }
    // Normalized parameters: exactly what the GPU uniform carries.
    let u = grade.to_uniform();

    let lut_intensity = grade
        .lut
        .as_ref()
        .map_or(0.0, |l| l.intensity.clamp(0.0, 1.0));
    let lut_active = lut_intensity > 0.0 && lut.is_some();
    let lut_before = grade
        .lut
        .as_ref()
        .map_or(false, |l| l.position == LutPosition::Before);
    let apply_lut = |c: [f32; 3]| -> [f32; 3] {
        let lf = lut.expect("lut_active implies lut.is_some()");
        let clamped = [
            c[0].clamp(0.0, 1.0),
            c[1].clamp(0.0, 1.0),
            c[2].clamp(0.0, 1.0),
        ];
        let s = trilinear_sample(&lf.data, lf.size, clamped);
        [
            mixf(c[0], s[0], lut_intensity),
            mixf(c[1], s[1], lut_intensity),
            mixf(c[2], s[2], lut_intensity),
        ]
    };

    // 1. Clamp + linearize (MVP: gamma 2.4 only; range fix skipped).
    let mut c = [
        pixel[0].clamp(0.0, 1.0).powf(2.4),
        pixel[1].clamp(0.0, 1.0).powf(2.4),
        pixel[2].clamp(0.0, 1.0).powf(2.4),
    ];

    // 2. LUT before the grade.
    if lut_active && lut_before {
        c = apply_lut(c);
    }

    // 3. Exposure.
    let e = 2f32.powf(u.exposure_ev);
    c = [c[0] * e, c[1] * e, c[2] * e];

    // 4. White balance.
    c = [
        c[0] * u.wb_gain[0],
        c[1] * u.wb_gain[1],
        c[2] * u.wb_gain[2],
    ];

    // 5. Lift / gamma / gain / offset wheels ([r,g,b] then master).
    for i in 0..3 {
        c[i] += u.lift[i] + u.lift[3];
    }
    for i in 0..3 {
        c[i] = c[i].max(0.0).powf(1.0 / u.gamma_w[i]);
    }
    for i in 0..3 {
        c[i] = c[i].max(0.0).powf(1.0 / u.gamma_w[3]);
    }
    for i in 0..3 {
        c[i] *= u.gain[i] * u.gain[3];
    }
    for i in 0..3 {
        c[i] += u.offset[i] + u.offset[3];
    }

    // 6. Tone controls. Luma windows are evaluated on l (post-wheels luma).
    let l = dot3(c, LUM);
    let ck = 1.0 + u.contrast;
    for i in 0..3 {
        c[i] = (c[i] - 0.18) * ck + 0.18;
    }
    let badd = u.brightness * 0.25;
    let hadd = u.highlights * 0.5 * smoothstep(0.5, 1.0, l);
    let sadd = u.shadows * 0.5 * (1.0 - smoothstep(0.0, 0.5, l));
    let wadd = u.whites * 0.5 * smoothstep(0.55, 1.0, l);
    let kadd = u.blacks * 0.5 * (1.0 - smoothstep(0.0, 0.45, l));
    for i in 0..3 {
        c[i] += badd + hadd + sadd + wadd + kadd;
    }

    // 7. Basic gamma.
    for i in 0..3 {
        c[i] = c[i].max(0.0).powf(u.basic_gamma);
    }

    // 8. Saturation around luma, with skin-tone protection.
    let l2 = dot3(c, LUM);
    let skin_m = if grade.skin_protect {
        smoothstep(0.05, 0.25, c[0] - c[1])
            * smoothstep(0.02, 0.12, c[1] - c[2])
            * smoothstep(0.15, 0.45, l2)
    } else {
        0.0
    };
    let mut sat_eff = u.saturation;
    if grade.skin_protect && sat_eff > 1.0 {
        sat_eff = mixf(sat_eff, 1.0 + (sat_eff - 1.0) * 0.4, skin_m);
    }
    for i in 0..3 {
        c[i] = mixf(l2, c[i], sat_eff);
    }

    // 9. Vibrance: scale falls off as chroma rises; skin-limited when boosted.
    let l3 = dot3(c, LUM);
    let chroma = c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2]);
    let mut vscale = 1.0 + u.vibrance * (1.0 - chroma.clamp(0.0, 1.0));
    if grade.skin_protect && u.vibrance > 0.0 {
        vscale = mixf(vscale, 1.0 + (vscale - 1.0) * 0.4, skin_m);
    }
    for i in 0..3 {
        c[i] = mixf(l3, c[i], vscale);
    }

    // 10. Curves: per-channel, then master, then luma delta, then hue-vs-sat.
    if !grade.curves.is_identity() {
        let r = baked.sample(0, 3, baked.sample(0, 0, c[0]));
        let g = baked.sample(0, 3, baked.sample(0, 1, c[1]));
        let b = baked.sample(0, 3, baked.sample(0, 2, c[2]));
        c = [r, g, b];
        let l4 = dot3(c, LUM);
        let dl = baked.sample(1, 0, l4) - l4;
        c = [c[0] + dl, c[1] + dl, c[2] + dl];
        let hsv = rgb_to_hsv(c);
        let s2 = hsv[1] * baked.sample(1, 1, hsv[0]);
        c = hsv_to_rgb([hsv[0], s2, hsv[2]]);
    }

    // 11. LUT after the grade.
    if lut_active && !lut_before {
        c = apply_lut(c);
    }

    // 12. Sharpness: not modeled (spatial op, GPU-only).

    // 13. Output encode + clamp.
    [
        c[0].max(0.0).powf(1.0 / 2.4).clamp(0.0, 1.0),
        c[1].max(0.0).powf(1.0 / 2.4).clamp(0.0, 1.0),
        c[2].max(0.0).powf(1.0 / 2.4).clamp(0.0, 1.0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grade::ColorGrade;

    fn graded(grade: &ColorGrade, pixel: [f32; 3]) -> [f32; 3] {
        let baked = grade.curves.bake();
        apply_grade_cpu(grade, &baked, None, pixel)
    }

    #[test]
    fn identity_grade_returns_input_bit_exact() {
        let grade = ColorGrade::default();
        assert!(grade.is_identity());
        let baked = grade.curves.bake();
        for pixel in [
            [0.0f32, 0.0, 0.0],
            [0.25, 0.25, 0.25],
            [0.5, 0.5, 0.5],
            [0.75, 0.75, 0.75],
            [1.0, 1.0, 1.0],
            [0.2, 0.5, 0.8],
        ] {
            assert_eq!(
                apply_grade_cpu(&grade, &baked, None, pixel),
                pixel,
                "pixel {pixel:?}"
            );
        }
    }

    #[test]
    fn exposure_plus_one_ev() {
        let grade = ColorGrade {
            exposure: 1.0,
            ..ColorGrade::default()
        };
        let out = graded(&grade, [0.5, 0.5, 0.5]);
        // Linearize, double in linear, re-encode; everything else is identity.
        let expected = (2.0f32 * 0.5f32.powf(2.4)).powf(1.0 / 2.4);
        for (i, v) in out.iter().enumerate() {
            assert!(
                (v - expected).abs() < 1e-4,
                "channel {i}: {v} vs expected {expected}"
            );
        }
    }

    #[test]
    fn exposure_minus_one_ev_darkens() {
        let grade = ColorGrade {
            exposure: -1.0,
            ..ColorGrade::default()
        };
        let out = graded(&grade, [0.5, 0.5, 0.5]);
        let expected = (0.5f32 * 0.5f32.powf(2.4)).powf(1.0 / 2.4);
        for v in out {
            assert!((v - expected).abs() < 1e-4, "{v} vs {expected}");
        }
        assert!(expected < 0.5);
    }

    #[test]
    fn contrast_pivot_holds_mid_grey() {
        // Contrast pivots at 0.18 in linear; a pixel sitting exactly at the
        // pivot is unchanged by contrast alone.
        let pivot_srgb = 0.18f32.powf(1.0 / 2.4);
        let grade = ColorGrade {
            contrast: 50.0,
            ..ColorGrade::default()
        };
        let out = graded(&grade, [pivot_srgb, pivot_srgb, pivot_srgb]);
        for v in out {
            assert!((v - pivot_srgb).abs() < 1e-3, "{v} vs {pivot_srgb}");
        }
    }

    #[test]
    fn temperature_warms_and_cools() {
        let warm = ColorGrade {
            temperature: 3200.0,
            ..ColorGrade::default()
        };
        let out = graded(&warm, [0.5, 0.5, 0.5]);
        assert!(out[0] > out[2], "3200 K should warm: {out:?}");

        let cool = ColorGrade {
            temperature: 9000.0,
            ..ColorGrade::default()
        };
        let out = graded(&cool, [0.5, 0.5, 0.5]);
        assert!(out[2] > out[0], "9000 K should cool: {out:?}");
    }

    #[test]
    fn hsv_known_values_match_spec_branches() {
        // Exact branch structure from the shared pseudocode (hue is NOT
        // divided by 6 in the red-max branch — this is intentional and the
        // WGSL shader implements the same branches, so both sides agree).
        assert_eq!(rgb_to_hsv([1.0, 0.0, 0.0]), [0.0, 1.0, 1.0]);
        let g = rgb_to_hsv([0.0, 1.0, 0.0]);
        assert!((g[0] - 1.0 / 3.0).abs() < 1e-6 && g[1] == 1.0 && g[2] == 1.0);
        let b = rgb_to_hsv([0.0, 0.0, 1.0]);
        assert!((b[0] - 2.0 / 3.0).abs() < 1e-6 && b[1] == 1.0 && b[2] == 1.0);
        assert_eq!(rgb_to_hsv([0.5, 0.5, 0.5]), [0.0, 0.0, 0.5]);
        assert_eq!(rgb_to_hsv([0.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);

        assert_eq!(hsv_to_rgb([0.0, 1.0, 1.0]), [1.0, 0.0, 0.0]);
        let g = hsv_to_rgb([1.0 / 3.0, 1.0, 1.0]);
        for i in 0..3 {
            assert!((g[i] - [0.0, 1.0, 0.0][i]).abs() < 1e-6, "{g:?}");
        }
        let b = hsv_to_rgb([2.0 / 3.0, 1.0, 1.0]);
        for i in 0..3 {
            assert!((b[i] - [0.0, 0.0, 1.0][i]).abs() < 1e-6, "{b:?}");
        }
        // Achromatic colors round-trip exactly through both functions.
        for v in [0.0, 0.25, 0.9] {
            let back = hsv_to_rgb(rgb_to_hsv([v, v, v]));
            for i in 0..3 {
                assert!((back[i] - v).abs() < 1e-6, "{back:?}");
            }
        }
    }

    #[test]
    fn output_is_always_clamped() {
        let grade = ColorGrade {
            exposure: 3.0,
            saturation: 200.0,
            ..ColorGrade::default()
        };
        let out = graded(&grade, [1.0, 1.0, 1.0]);
        for v in out {
            assert!((0.0..=1.0).contains(&v), "{v}");
        }
        // Extreme lift + gain can't produce NaN either.
        let grade = ColorGrade {
            lift: [10.0, 10.0, 10.0, 0.0],
            gain: [5.0, 5.0, 5.0, 5.0],
            ..ColorGrade::default()
        };
        for v in graded(&grade, [0.9, 0.1, 0.4]) {
            assert!(v.is_finite() && (0.0..=1.0).contains(&v), "{v}");
        }
    }
}
