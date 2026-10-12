//! AI Camera Match (spec §6.2).
//!
//! Matches a target camera to a reference camera from median statistics, and
//! solves the pro-mode 24-patch ColorChecker 3×3 matrix by least squares.
//! Everything returns **editable** [`ColorGrade`] parameters or suggestions —
//! the engine applies them through the normal grade path (with
//! [`interpolate_grade`] for the 0.5 s smooth apply).

use sf_color::{kelvin_to_gains, ColorGrade};

use crate::stats::FrameStats;

/// Result of [`camera_match`]: the corrective grade plus explanations.
pub struct MatchResult {
    /// Grade that, applied to the target, approximates the reference.
    pub grade: ColorGrade,
    /// Human-readable explanation per correction.
    pub explanation: Vec<String>,
}

/// Fit a color temperature to green-normalized WB gains (shared with
/// [`crate::auto`]; duplicated here to keep both features self-contained).
fn fit_temperature(need_r: f32, need_b: f32) -> f32 {
    let mut best_k = 6500.0f32;
    let mut best_err = f32::MAX;
    let mut k = 2500.0f32;
    while k <= 10000.0 {
        let g = kelvin_to_gains(k, 0.0);
        let err = (g[0] - need_r).powi(2) + (g[2] - need_b).powi(2);
        if err < best_err {
            best_err = err;
            best_k = k;
        }
        k += 50.0;
    }
    best_k
}

/// Match `target` to `reference` from median statistics (spec §6.2).
///
/// Corrections, all clamped to their UI ranges:
/// - **Exposure:** `log2(ref.p50 / tgt.p50)`, ±3 EV.
/// - **White balance:** per-channel neutral-region ratios
///   (`ref.gray_c / tgt.gray_c`, green-normalized) → best-fit temperature;
///   tint from the shift in green imbalance
///   (`gray_g / √(gray_r·gray_b)`) between the two frames.
/// - **Contrast:** p5–p95 spread ratio, computed *after normalizing out the
///   exposure difference* (so it corrects true contrast mismatch, not the
///   spread change that exposure alone explains):
///   `contrast = 50·ln(ref_spread / (tgt_spread · ref.p50/tgt.p50))`.
/// - **Saturation:** `100 · (ref.mean_chroma / tgt.mean_chroma) ·
///   (tgt.p50 / ref.p50)`, exposure-normalized the same way, clamped 0..200.
///
/// A corrective grade: applying it to the target approximates the reference.
/// Every correction gets an explanation string.
pub fn camera_match(reference: &FrameStats, target: &FrameStats) -> MatchResult {
    let mut grade = ColorGrade::default();
    let mut explanation = Vec::new();
    let eps = 1e-4;

    // --- Exposure ---
    let exposure = (reference.p50.max(eps) / target.p50.max(eps))
        .log2()
        .clamp(-3.0, 3.0);
    grade.exposure = exposure;
    explanation.push(format!(
        "Exposure {exposure:+.2} EV: target median {:.3} → reference median {:.3}.",
        target.p50, reference.p50
    ));

    // --- White balance ---
    let ratio = |a: f32, b: f32| a.max(eps) / b.max(eps);
    let gr = ratio(reference.gray_r, target.gray_r);
    let gg = ratio(reference.gray_g, target.gray_g);
    let gb = ratio(reference.gray_b, target.gray_b);
    let temperature = fit_temperature(gr / gg, gb / gg).clamp(2500.0, 10000.0);
    let imbalance = |s: &FrameStats| {
        let (r, g, b) = (s.gray_r.max(eps), s.gray_g.max(eps), s.gray_b.max(eps));
        g / (r * b).sqrt()
    };
    let tint_shift = imbalance(reference) / imbalance(target).max(eps);
    let tint = ((1.0 - tint_shift) / 0.003).clamp(-100.0, 100.0);
    grade.temperature = temperature;
    grade.tint = tint;
    explanation.push(format!(
        "White balance: neutral-region channel ratios R/G/B {gr:.3}/{gg:.3}/{gb:.3} → {temperature:.0} K, tint {tint:+.0}."
    ));

    // --- Contrast (exposure-normalized spread ratio) ---
    let ref_spread = (reference.p95 - reference.p5).max(eps);
    let tgt_spread = (target.p95 - target.p5).max(eps);
    let ev_ratio = reference.p50.max(eps) / target.p50.max(eps);
    let spread_ratio = ref_spread / (tgt_spread * ev_ratio).max(eps);
    let contrast = (50.0 * spread_ratio.ln()).clamp(-100.0, 100.0);
    grade.contrast = contrast;
    explanation.push(format!(
        "Contrast {contrast:+.0}: exposure-normalized p5–p95 spread ratio {spread_ratio:.2}."
    ));

    // --- Saturation (exposure-normalized chroma ratio) ---
    let saturation = (100.0 * (reference.mean_chroma.max(eps) / target.mean_chroma.max(eps))
        / ev_ratio)
        .clamp(0.0, 200.0);
    grade.saturation = saturation;
    explanation.push(format!(
        "Saturation {saturation:.0}%: exposure-normalized mean-chroma ratio {:.2}.",
        reference.mean_chroma.max(eps) / (target.mean_chroma.max(eps) * ev_ratio).max(eps)
    ));

    MatchResult { grade, explanation }
}

/// Smoothly interpolate between two grades — the 0.5 s smooth-apply helper.
///
/// `t` is clamped to `0..=1` and eased with smoothstep (`s = t²(3−2t)`) so the
/// UI's timed fade starts and ends gently. All numeric UI fields and the
/// wheel arrays lerp; curves, LUT and `skin_protect` snap at `s ≥ 0.5`
/// (they have no meaningful in-between).
pub fn interpolate_grade(from: &ColorGrade, to: &ColorGrade, t: f32) -> ColorGrade {
    let t = t.clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    let lerp = |a: f32, b: f32| a + (b - a) * s;
    let lerp4 = |a: [f32; 4], b: [f32; 4]| {
        [
            lerp(a[0], b[0]),
            lerp(a[1], b[1]),
            lerp(a[2], b[2]),
            lerp(a[3], b[3]),
        ]
    };
    ColorGrade {
        exposure: lerp(from.exposure, to.exposure),
        brightness: lerp(from.brightness, to.brightness),
        contrast: lerp(from.contrast, to.contrast),
        saturation: lerp(from.saturation, to.saturation),
        vibrance: lerp(from.vibrance, to.vibrance),
        temperature: lerp(from.temperature, to.temperature),
        tint: lerp(from.tint, to.tint),
        highlights: lerp(from.highlights, to.highlights),
        shadows: lerp(from.shadows, to.shadows),
        whites: lerp(from.whites, to.whites),
        blacks: lerp(from.blacks, to.blacks),
        gamma: lerp(from.gamma, to.gamma),
        sharpness: lerp(from.sharpness, to.sharpness),
        lift: lerp4(from.lift, to.lift),
        gamma_wheel: lerp4(from.gamma_wheel, to.gamma_wheel),
        gain: lerp4(from.gain, to.gain),
        offset: lerp4(from.offset, to.offset),
        curves: if s < 0.5 {
            from.curves.clone()
        } else {
            to.curves.clone()
        },
        lut: if s < 0.5 {
            from.lut.clone()
        } else {
            to.lut.clone()
        },
        skin_protect: if s < 0.5 {
            from.skin_protect
        } else {
            to.skin_protect
        },
        faded_film: lerp(from.faded_film, to.faded_film),
        skin_warmth: lerp(from.skin_warmth, to.skin_warmth),
        skin_saturation: lerp(from.skin_saturation, to.skin_saturation),
        skin_brightness: lerp(from.skin_brightness, to.skin_brightness),
    }
}

/// Solve the least-squares 3×3 color matrix mapping `src` patches to `dst`
/// patches (pro-mode ColorChecker, spec §6.2).
///
/// Minimizes `Σ‖M·srcᵢ − dstᵢ‖²` via the normal equations
/// `M = (Σ dstᵢ·srcᵢᵀ)·(Σ srcᵢ·srcᵢᵀ)⁻¹`, with the inverse from Gaussian
/// elimination with partial pivoting. Pure CPU.
///
/// Panics when fewer than 3 patch pairs are given (a 3×3 has 9 unknowns and
/// needs ≥3 non-degenerate pairs) or when the patches are degenerate
/// (singular normal matrix).
pub fn solve_color_matrix(src: &[[f32; 3]], dst: &[[f32; 3]]) -> [[f32; 3]; 3] {
    assert!(
        src.len() >= 3 && src.len() == dst.len(),
        "solve_color_matrix needs ≥3 patch pairs, got {}",
        src.len()
    );
    // A = Σ dst·srcᵀ, B = Σ src·srcᵀ
    let mut a = [[0.0f32; 3]; 3];
    let mut b = [[0.0f32; 3]; 3];
    for (s, d) in src.iter().zip(dst.iter()) {
        for i in 0..3 {
            for j in 0..3 {
                a[i][j] += d[i] * s[j];
                b[i][j] += s[i] * s[j];
            }
        }
    }
    let b_inv = invert_3x3(b);
    let mut m = [[0.0f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            m[i][j] = a[i][0] * b_inv[0][j] + a[i][1] * b_inv[1][j] + a[i][2] * b_inv[2][j];
        }
    }
    m
}

/// 3×3 inverse by Gaussian elimination with partial pivoting.
fn invert_3x3(m: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let mut aug = [
        [m[0][0], m[0][1], m[0][2], 1.0, 0.0, 0.0],
        [m[1][0], m[1][1], m[1][2], 0.0, 1.0, 0.0],
        [m[2][0], m[2][1], m[2][2], 0.0, 0.0, 1.0],
    ];
    for col in 0..3 {
        let mut pivot = col;
        for row in col + 1..3 {
            if aug[row][col].abs() > aug[pivot][col].abs() {
                pivot = row;
            }
        }
        assert!(
            aug[pivot][col].abs() > 1e-12,
            "solve_color_matrix: degenerate patches (singular normal matrix)"
        );
        aug.swap(col, pivot);
        let div = aug[col][col];
        for j in 0..6 {
            aug[col][j] /= div;
        }
        for row in 0..3 {
            if row != col {
                let factor = aug[row][col];
                for j in 0..6 {
                    aug[row][j] -= factor * aug[col][j];
                }
            }
        }
    }
    [
        [aug[0][3], aug[0][4], aug[0][5]],
        [aug[1][3], aug[1][4], aug[1][5]],
        [aug[2][3], aug[2][4], aug[2][5]],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::compute_stats;

    /// Synthetic stats pair: target = reference with exactly +1 EV (median
    /// doubled) and a 1.2× red gain. Declared directly so the expected
    /// corrections are exact (no gamma-space confounds from real frames).
    fn ref_and_shifted() -> (FrameStats, FrameStats) {
        let reference = FrameStats {
            pixels: 2304,
            p1: 0.15,
            p5: 0.20,
            p50: 0.30,
            p95: 0.50,
            p99: 0.60,
            mean_r: 0.30,
            mean_g: 0.30,
            mean_b: 0.30,
            mean_chroma: 0.10,
            skin_fraction: 0.0,
            gray_r: 0.30,
            gray_g: 0.30,
            gray_b: 0.30,
        };
        let target = FrameStats {
            pixels: 2304,
            p1: 0.30,
            p5: 0.40,
            p50: 0.60,
            p95: 1.00,
            p99: 1.00,
            mean_r: 0.72,
            mean_g: 0.60,
            mean_b: 0.60,
            mean_chroma: 0.20,
            skin_fraction: 0.0,
            gray_r: 0.72,
            gray_g: 0.60,
            gray_b: 0.60,
        };
        (reference, target)
    }

    #[test]
    fn match_recovers_known_exposure_and_wb_shift() {
        let (reference, target) = ref_and_shifted();
        let res = camera_match(&reference, &target);
        // +1 EV on the target → correction ≈ −1 EV.
        assert!(
            (res.grade.exposure + 1.0).abs() < 1e-4,
            "exposure = {}",
            res.grade.exposure
        );
        // 1.2× red gain on the target → cooling correction (temp > 6500 K).
        assert!(
            res.grade.temperature > 6500.0,
            "temperature = {}",
            res.grade.temperature
        );
        // Exposure-normalized spread ratio is exactly 1 → neutral contrast.
        assert!(
            res.grade.contrast.abs() < 1e-3,
            "contrast = {}",
            res.grade.contrast
        );
        // Exposure-normalized chroma ratio is exactly 1 → 100% saturation.
        assert!(
            (res.grade.saturation - 100.0).abs() < 1e-3,
            "saturation = {}",
            res.grade.saturation
        );
        assert_eq!(res.explanation.len(), 4);
    }

    #[test]
    fn identical_frames_match_to_identity() {
        let mut base = Vec::new();
        for _ in 0..48 * 48 {
            base.extend_from_slice(&[80u8, 64, 56, 255]);
        }
        let stats = compute_stats(&base, 48, 48);
        let res = camera_match(&stats, &stats);
        assert!(res.grade.exposure.abs() < 1e-4);
        assert!((res.grade.temperature - 6500.0).abs() < 50.0);
        assert!(res.grade.tint.abs() < 2.0);
        assert!(res.grade.contrast.abs() < 1.0);
        assert!((res.grade.saturation - 100.0).abs() < 1.0);
    }

    #[test]
    fn solve_color_matrix_recovers_known_matrix() {
        let m_true = [[1.1f32, 0.0, 0.0], [0.0, 0.9, 0.0], [0.0, 0.0, 1.0]];
        let src = [
            [0.10, 0.20, 0.30],
            [0.40, 0.50, 0.60],
            [0.70, 0.10, 0.90],
            [0.30, 0.80, 0.20],
            [0.55, 0.35, 0.75],
        ];
        let dst: Vec<[f32; 3]> = src
            .iter()
            .map(|s| {
                [
                    m_true[0][0] * s[0] + m_true[0][1] * s[1] + m_true[0][2] * s[2],
                    m_true[1][0] * s[0] + m_true[1][1] * s[1] + m_true[1][2] * s[2],
                    m_true[2][0] * s[0] + m_true[2][1] * s[1] + m_true[2][2] * s[2],
                ]
            })
            .collect();
        let m = solve_color_matrix(&src, &dst);
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (m[i][j] - m_true[i][j]).abs() < 1e-4,
                    "m[{i}][{j}] = {} vs {}",
                    m[i][j],
                    m_true[i][j]
                );
            }
        }
    }

    #[test]
    fn solve_color_matrix_recovers_nondiagonal_matrix() {
        // A matrix with cross-talk, solved from 6 patches.
        let m_true = [[0.9f32, 0.1, 0.05], [0.05, 1.05, -0.1], [-0.05, 0.1, 1.1]];
        let src = [
            [0.12, 0.34, 0.56],
            [0.78, 0.21, 0.43],
            [0.65, 0.87, 0.09],
            [0.31, 0.52, 0.74],
            [0.95, 0.15, 0.35],
            [0.22, 0.66, 0.88],
        ];
        let dst: Vec<[f32; 3]> = src
            .iter()
            .map(|s| {
                [
                    m_true[0][0] * s[0] + m_true[0][1] * s[1] + m_true[0][2] * s[2],
                    m_true[1][0] * s[0] + m_true[1][1] * s[1] + m_true[1][2] * s[2],
                    m_true[2][0] * s[0] + m_true[2][1] * s[1] + m_true[2][2] * s[2],
                ]
            })
            .collect();
        let m = solve_color_matrix(&src, &dst);
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (m[i][j] - m_true[i][j]).abs() < 1e-3,
                    "m[{i}][{j}] = {} vs {}",
                    m[i][j],
                    m_true[i][j]
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "≥3 patch pairs")]
    fn solve_color_matrix_rejects_too_few_patches() {
        solve_color_matrix(&[[0.1, 0.2, 0.3]], &[[0.1, 0.2, 0.3]]);
    }

    #[test]
    fn interpolate_grade_endpoints_and_easing() {
        let from = ColorGrade::default();
        let mut to = ColorGrade::default();
        to.exposure = 2.0;
        to.saturation = 150.0;
        to.temperature = 5500.0;

        let at0 = interpolate_grade(&from, &to, 0.0);
        assert_eq!(at0.exposure, 0.0);
        let at1 = interpolate_grade(&from, &to, 1.0);
        assert_eq!(at1.exposure, 2.0);
        assert_eq!(at1.saturation, 150.0);

        // smoothstep(0.25) = 0.15625 — eased, not linear.
        let mid = interpolate_grade(&from, &to, 0.25);
        let s = 0.25f32 * 0.25 * (3.0 - 2.0 * 0.25);
        assert!((mid.exposure - 2.0 * s).abs() < 1e-6);
        assert!(mid.exposure < 0.5, "eased value must lag linear at t=0.25");

        // t = 0.5 → s = 0.5 exactly: true midpoint.
        let half = interpolate_grade(&from, &to, 0.5);
        assert!((half.exposure - 1.0).abs() < 1e-6);
        assert!((half.temperature - 6000.0).abs() < 1e-3);

        // Out-of-range t clamps.
        let clamped = interpolate_grade(&from, &to, 2.0);
        assert_eq!(clamped.exposure, 2.0);
    }
}
