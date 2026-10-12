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

/// RGB (0..1) -> HSV, with hue in 0..1 (standard hexcone HSV).
///
/// Earlier versions omitted the `/6` on the in-sector hue fraction. That
/// only agreed with standard HSV on the primaries (which is all the old
/// test checked) — for every other color the HSV round-trip in the curves
/// stage rotated hues, e.g. an orange jacket turned green and skin turned
/// cyan as soon as any curve was edited. Mirrors the WGSL `rgb_to_hsv`.
///
/// ```text
/// mx = max(r,g,b); mn = min(r,g,b); d = mx - mn; h = 0
/// if d > 1e-6:
///     if mx == r: h = (g - b) / d / 6
///     elif mx == g: h = ((b - r) / d + 2) / 6
///     else: h = ((r - g) / d + 4) / 6
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
            h = (g - b) / d / 6.0;
        } else if mx == g {
            h = ((b - r) / d + 2.0) / 6.0;
        } else {
            h = ((r - g) / d + 4.0) / 6.0;
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
///  2. if LUT enabled and position == Before (display-referred, see 11):
///         d = enc(c); c = dec(mix(d, trilinear(lut, clamp(d,0,1)), intensity))
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
///  9b. skin-only warmth / saturation / brightness through the skin mask
/// 10. curves (if any non-identity), on display-encoded values d = enc(c)
///     (so a curve point at 0.5 means mid-grey, as on screen): per-channel,
///     master, luma delta, hue-vs-sat in HSV; then c = dec(d)
/// 10b. faded film (display-encoded): black lift + shadow desaturation
/// 11. if LUT enabled and position == After — creative `.cube` LUTs expect
///     display-referred input, so the LUT is sampled on enc(c) and blended
///     there: d = enc(c); c = dec(mix(d, trilinear(lut, clamp(d,0,1)), i))
/// 12. sharpness: SKIPPED (spatial op, GPU-only)
/// 13. c = pow(max(c,0), 1/2.4); c = clamp(c, 0, 1)         // output encode
/// ```
/// Skin-tone mask heuristic (shared with the shader): reddish-orange hue,
/// mid luminance.
fn skin_mask(c: [f32; 3], l: f32) -> f32 {
    smoothstep(0.05, 0.25, c[0] - c[1]) * smoothstep(0.02, 0.12, c[1] - c[2]) * smoothstep(0.15, 0.45, l)
}

/// Linear -> display (gamma 2.4, the MVP transfer), negative-safe.
fn enc3(c: [f32; 3]) -> [f32; 3] {
    [
        c[0].max(0.0).powf(1.0 / 2.4),
        c[1].max(0.0).powf(1.0 / 2.4),
        c[2].max(0.0).powf(1.0 / 2.4),
    ]
}

/// Display -> linear (gamma 2.4), negative-safe.
fn dec3(d: [f32; 3]) -> [f32; 3] {
    [
        d[0].max(0.0).powf(2.4),
        d[1].max(0.0).powf(2.4),
        d[2].max(0.0).powf(2.4),
    ]
}

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
        let d = enc3(c);
        let clamped = [
            d[0].clamp(0.0, 1.0),
            d[1].clamp(0.0, 1.0),
            d[2].clamp(0.0, 1.0),
        ];
        let s = trilinear_sample(&lf.data, lf.size, clamped);
        dec3([
            mixf(d[0], s[0], lut_intensity),
            mixf(d[1], s[1], lut_intensity),
            mixf(d[2], s[2], lut_intensity),
        ])
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
        skin_mask(c, l2)
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

    // 9b. Skin-only adjustments (natural ranges: ±8 % warmth, ±35 %
    //     saturation, ±25 % brightness at the slider ends).
    if u.skin_warmth != 0.0 || u.skin_saturation != 0.0 || u.skin_brightness != 0.0 {
        let ls = dot3(c, LUM);
        let m = skin_mask(c, ls);
        c[0] *= 1.0 + 0.08 * u.skin_warmth * m;
        c[2] *= 1.0 - 0.08 * u.skin_warmth * m;
        let ls2 = dot3(c, LUM);
        let sk = 1.0 + 0.35 * u.skin_saturation * m;
        for i in 0..3 {
            c[i] = mixf(ls2, c[i], sk);
        }
        let bk = 1.0 + 0.25 * u.skin_brightness * m;
        for i in 0..3 {
            c[i] *= bk;
        }
    }

    // 10. Curves on display-encoded values: per-channel, master, luma
    //     delta, hue-vs-sat.
    if !grade.curves.is_identity() {
        let d = enc3(c);
        let r = baked.sample(0, 3, baked.sample(0, 0, d[0]));
        let g = baked.sample(0, 3, baked.sample(0, 1, d[1]));
        let b = baked.sample(0, 3, baked.sample(0, 2, d[2]));
        let mut d = [r, g, b];
        let l4 = dot3(d, LUM);
        let dl = baked.sample(1, 0, l4) - l4;
        d = [d[0] + dl, d[1] + dl, d[2] + dl];
        let hsv = rgb_to_hsv(d);
        let s2 = hsv[1] * baked.sample(1, 1, hsv[0]);
        c = dec3(hsv_to_rgb([hsv[0], s2, hsv[2]]));
    }

    // 10b. Faded film (display-encoded): lift + soften blacks, mute
    //      shadow color.
    if u.faded_film > 0.0 {
        let k = u.faded_film;
        let mut d = enc3(c);
        let ld = dot3(d, LUM).clamp(0.0, 1.0);
        let shadow = (1.0 - ld) * (1.0 - ld);
        let desat = 1.0 - 0.35 * k * shadow;
        for i in 0..3 {
            d[i] = mixf(ld, d[i], desat);
            let x = d[i].clamp(0.0, 1.0);
            d[i] += 0.6 * k * 0.2 * (1.0 - x) * (1.0 - x) * (1.0 - x);
        }
        c = dec3(d);
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
        assert_eq!(rgb_to_hsv([1.0, 0.0, 0.0]), [0.0, 1.0, 1.0]);
        let y = rgb_to_hsv([1.0, 1.0, 0.0]);
        assert!((y[0] - 1.0 / 6.0).abs() < 1e-6, "yellow hue {y:?}");
        let g = rgb_to_hsv([0.0, 1.0, 0.0]);
        assert!((g[0] - 1.0 / 3.0).abs() < 1e-6 && g[1] == 1.0 && g[2] == 1.0);
        let b = rgb_to_hsv([0.0, 0.0, 1.0]);
        assert!((b[0] - 2.0 / 3.0).abs() < 1e-6 && b[1] == 1.0 && b[2] == 1.0);
        let m = rgb_to_hsv([1.0, 0.0, 1.0]);
        assert!((m[0] - 5.0 / 6.0).abs() < 1e-6, "magenta hue {m:?}");
        assert_eq!(rgb_to_hsv([0.5, 0.5, 0.5]), [0.0, 0.0, 0.5]);
        assert_eq!(rgb_to_hsv([0.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);
        assert_eq!(hsv_to_rgb([0.0, 1.0, 1.0]), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn hsv_roundtrip_preserves_any_color() {
        // Regression: orange / skin / teal must survive RGB -> HSV -> RGB.
        for c in [
            [0.9, 0.4, 0.1],
            [0.85, 0.62, 0.5],
            [0.1, 0.6, 0.55],
            [0.3, 0.2, 0.8],
            [0.7, 0.7, 0.2],
            [0.95, 0.1, 0.5],
        ] {
            let back = hsv_to_rgb(rgb_to_hsv(c));
            for i in 0..3 {
                assert!((back[i] - c[i]).abs() < 1e-5, "{c:?} -> {back:?}");
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

    #[test]
    fn lut_is_sampled_on_display_values() {
        // A 1D LUT that maps display 0.5 -> 0.5 and is identity: a mid-grey
        // pixel must come back unchanged. Sampling on linear values (the old
        // bug) would read the LUT at 0.5^2.4 ≈ 0.19 — fine for identity, so
        // use a LUT that is identity below 0.4 and white above: mid-grey
        // (display 0.5) must turn white.
        let mut text = String::from("LUT_1D_SIZE 11\n");
        for i in 0..11 {
            let v = i as f32 / 10.0;
            let o = if v < 0.45 { v } else { 1.0 };
            text.push_str(&format!("{o} {o} {o}\n"));
        }
        let cube = crate::lut::parse_cube(&text).unwrap();
        let mut g = ColorGrade::default();
        g.lut = Some(crate::grade::LutRef {
            file: "x.cube".into(),
            intensity: 1.0,
            position: LutPosition::After,
        });
        let out = apply_grade_cpu(&g, &g.curves.bake(), Some(&cube), [0.5, 0.5, 0.5]);
        assert!(out[0] > 0.95, "LUT must see display value 0.5, got {out:?}");
    }

    #[test]
    fn lut_intensity_blends_proportionally() {
        let mut text = String::from("LUT_1D_SIZE 2\n");
        text.push_str("1 1 1\n0 0 0\n"); // invert
        let cube = crate::lut::parse_cube(&text).unwrap();
        let mut g = ColorGrade::default();
        let mut run = |i: f32| {
            g.lut = Some(crate::grade::LutRef {
                file: "inv.cube".into(),
                intensity: i,
                position: LutPosition::After,
            });
            apply_grade_cpu(&g, &g.curves.bake(), Some(&cube), [0.2, 0.2, 0.2])[0]
        };
        assert!((run(0.0) - 0.2).abs() < 1e-4);
        assert!((run(1.0) - 0.8).abs() < 1e-3);
        assert!((run(0.5) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn curve_midpoint_means_display_mid_grey() {
        // Pull the master curve's 0.5 point to 0.6: display mid-grey must
        // land at ~0.6 on screen.
        let mut g = ColorGrade::default();
        g.curves.master.set_points(vec![[0.0, 0.0], [0.5, 0.6], [1.0, 1.0]]).unwrap();
        let out = apply_grade_cpu(&g, &g.curves.bake(), None, [0.5, 0.5, 0.5]);
        assert!((out[0] - 0.6).abs() < 0.01, "{out:?}");
    }

    #[test]
    fn editing_rgb_curve_does_not_desaturate() {
        // Regression: the default hue-vs-sat curve used to multiply
        // saturation by hue as soon as any curve was edited.
        let mut g = ColorGrade::default();
        g.curves.r.set_points(vec![[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]]).unwrap();
        g.curves.r.points.push([0.75, 0.75]);
        g.curves.r.points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap());
        let red = [0.8, 0.1, 0.1];
        let out = apply_grade_cpu(&g, &g.curves.bake(), None, red);
        for c in 0..3 {
            assert!((out[c] - red[c]).abs() < 0.02, "{out:?}");
        }
    }

    #[test]
    fn faded_film_lifts_blacks_keeps_whites() {
        let mut g = ColorGrade::default();
        g.faded_film = 100.0;
        let black = apply_grade_cpu(&g, &g.curves.bake(), None, [0.0, 0.0, 0.0]);
        let white = apply_grade_cpu(&g, &g.curves.bake(), None, [1.0, 1.0, 1.0]);
        assert!(black[0] > 0.08 && black[0] < 0.16, "{black:?}");
        assert!(white[0] > 0.99, "{white:?}");
    }

    #[test]
    fn skin_controls_touch_skin_only() {
        let mut g = ColorGrade::default();
        g.skin_warmth = 100.0;
        g.skin_brightness = 50.0;
        let skin = [0.85, 0.62, 0.50];
        let sky = [0.35, 0.55, 0.85];
        let s_out = apply_grade_cpu(&g, &g.curves.bake(), None, skin);
        let k_out = apply_grade_cpu(&g, &g.curves.bake(), None, sky);
        assert!(s_out[0] > skin[0] && s_out[2] < skin[2] + 0.01, "skin {s_out:?}");
        for c in 0..3 {
            assert!((k_out[c] - sky[c]).abs() < 1e-3, "sky changed {k_out:?}");
        }
    }
}
