//! AI Auto Color — one-click correction (spec §6.1).
//!
//! Analyzes [`FrameStats`] and returns an **editable** [`ColorGrade`] plus a
//! human-readable explanation per change. Nothing is applied automatically;
//! the UI lands the grade on the Color Studio sliders for the user to tweak.

use sf_color::{kelvin_to_gains, ColorGrade};

use crate::stats::FrameStats;

/// Target median luma for a well-exposed frame (spec §6.1: ~0.42).
const TARGET_MEDIAN: f32 = 0.42;
/// Target p5–p95 spread of a healthy-contrast frame.
const TARGET_SPREAD: f32 = 0.65;
/// Frames flatter than this carry no contrast information; leave contrast alone.
const FLAT_SPREAD: f32 = 0.03;

/// Result of [`auto_color`]: the suggested grade plus one explanation string
/// per decision, in UI language.
pub struct AutoColorResult {
    /// Suggested grade in UI units, ready to land on the sliders.
    pub grade: ColorGrade,
    /// Human-readable explanation per change (never empty).
    pub explanation: Vec<String>,
}

/// Fit a color temperature whose WB gains best neutralize the frame.
///
/// `need_r`/`need_b` are the green-normalized gains that would equalize the
/// neutral region (`gray_g/gray_r`, `gray_g/gray_b`). Searches the UI range
/// 2500..10000 K for the temperature whose [`kelvin_to_gains`] output is
/// closest — self-consistent with the grade pipeline, since the produced
/// `temperature` value feeds exactly that function.
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

/// Green/magenta tint (UI −100..100) that neutralizes a green imbalance `m`
/// (`gray_g / sqrt(gray_r·gray_b)`).
///
/// From the `temp` formula contract: negative tint boosts red+blue by
/// `1 − tint·0.003`, which relative to green equals a green cut of the same
/// factor — so `1/(1 − tint·0.003) = 1/m`.
fn tint_for_green_imbalance(m: f32) -> f32 {
    ((1.0 - m) / 0.003).clamp(-100.0, 100.0)
}

/// One-click auto color from frame statistics (spec §6.1).
///
/// - **Exposure:** `log2(target_median / p50)`, clamped to ±3 EV.
///   *Skin-aware:* the target is pulled partway toward the current median in
///   proportion to [`FrameStats::skin_fraction`] —
///   `target = 0.42·(1−pull) + p50·pull` with `pull = min(0.6, 1.5·skin)` —
///   so a face-heavy frame that is only slightly dark gets a gentler lift
///   and faces don't blow out. A skin-free frame keeps the full correction.
/// - **Contrast:** from the p5–p95 spread vs [`TARGET_SPREAD`]:
///   `contrast = 100·(0.65 − spread)`, clamped to −100..100 UI units.
///   Frames flatter than [`FLAT_SPREAD`] (e.g. a uniform test card) carry no
///   contrast information and are left at 0.
/// - **White balance:** the neutral-region channel ratios are converted to
///   the green-normalized gains that would equalize them; temperature is the
///   best-fit Kelvin for those gains (see [`fit_temperature`]), tint
///   neutralizes the residual green/magenta imbalance around 6500 K / 0.
///
/// Every applied change appends an explanation string.
pub fn auto_color(s: &FrameStats) -> AutoColorResult {
    let mut grade = ColorGrade::default();
    let mut explanation = Vec::new();

    // --- Exposure (skin-aware) ---
    let p50 = s.p50.max(1e-4);
    let pull = (s.skin_fraction * 1.5).min(0.6);
    let target = TARGET_MEDIAN * (1.0 - pull) + p50 * pull;
    let exposure = (target / p50).log2().clamp(-3.0, 3.0);
    grade.exposure = exposure;
    if exposure.abs() < 0.05 {
        explanation.push(format!(
            "Exposure: median luma {p50:.3} already at target {TARGET_MEDIAN} — no change."
        ));
    } else {
        let mut msg =
            format!("Exposure {exposure:+.2} EV: median luma {p50:.3} → target {target:.3}.");
        if pull > 0.01 {
            msg.push_str(&format!(
                " Skin-aware: {:.0}% of the frame looks like skin, so the target was pulled {:.0}% toward the current median to protect faces.",
                s.skin_fraction * 100.0,
                pull * 100.0
            ));
        }
        explanation.push(msg);
    }

    // --- Contrast ---
    let spread = (s.p95 - s.p5).max(0.0);
    if spread < FLAT_SPREAD {
        explanation.push(
            "Contrast: frame is nearly flat (p5–p95 spread under 0.03) — no contrast information, left at 0."
                .to_string(),
        );
    } else {
        let contrast = (100.0 * (TARGET_SPREAD - spread)).clamp(-100.0, 100.0);
        grade.contrast = contrast;
        if contrast.abs() < 1.0 {
            explanation.push(format!(
                "Contrast: p5–p95 spread {spread:.2} already near the {TARGET_SPREAD} target — no change."
            ));
        } else {
            explanation.push(format!(
                "Contrast {contrast:+.0}: p5–p95 spread {spread:.2} vs target {TARGET_SPREAD}."
            ));
        }
    }

    // --- White balance ---
    let (gr, gg, gb) = (s.gray_r.max(1e-4), s.gray_g.max(1e-4), s.gray_b.max(1e-4));
    let temperature = fit_temperature(gg / gr, gg / gb).clamp(2500.0, 10000.0);
    let green_imbalance = gg / (gr * gb).sqrt();
    let tint = tint_for_green_imbalance(green_imbalance);
    grade.temperature = temperature;
    grade.tint = tint;
    let temp_delta = temperature - 6500.0;
    if temp_delta.abs() < 75.0 && tint.abs() < 2.0 {
        explanation.push(format!(
            "White balance: neutral region R/G/B {gr:.3}/{gg:.3}/{gb:.3} is already neutral — staying at 6500 K, tint 0."
        ));
    } else {
        let cast = if temp_delta < 0.0 { "warm" } else { "cool" };
        explanation.push(format!(
            "White balance: neutral region R/G/B {gr:.3}/{gg:.3}/{gb:.3} has a {cast} cast → {temperature:.0} K, tint {tint:+.0}."
        ));
    }

    AutoColorResult { grade, explanation }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::compute_stats;

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            out.extend_from_slice(&[v, v, v, 255]);
        }
        out
    }

    #[test]
    fn dark_frame_gets_expected_exposure_lift() {
        // Uniform 0.1 gray: skin-free, so the full log2(0.42/0.1) correction.
        let stats = compute_stats(&solid(64, 64, 26), 64, 64);
        let res = auto_color(&stats);
        let expected = (0.42f32 / 0.1).log2();
        assert!(
            (res.grade.exposure - expected).abs() < 0.1,
            "exposure {} vs expected {expected}",
            res.grade.exposure
        );
        assert!(!res.explanation.is_empty());
        assert!(res.explanation.iter().any(|e| e.contains("Exposure")));
    }

    #[test]
    fn correct_frame_gets_near_zero_changes() {
        // Uniform 0.42 gray: exposure ≈ 0, contrast gated by the flat-frame
        // rule, WB already neutral.
        let v = (0.42f32 * 255.0).round() as u8;
        let stats = compute_stats(&solid(64, 64, v), 64, 64);
        let res = auto_color(&stats);
        assert!(
            res.grade.exposure.abs() < 0.05,
            "exposure = {}",
            res.grade.exposure
        );
        assert_eq!(res.grade.contrast, 0.0);
        assert!((res.grade.temperature - 6500.0).abs() < 75.0);
        assert!(res.grade.tint.abs() < 2.0);
        assert!(!res.explanation.is_empty());
    }

    #[test]
    fn warm_cast_frame_gets_cooling_temperature() {
        // Red-heavy gray (R/B ≈ 1.43): neutralizing it means COOLING the
        // image, i.e. a temperature ABOVE 6500 K in this creative convention
        // (higher K = cooler gains).
        let mut px = Vec::new();
        for _ in 0..32 * 32 {
            px.extend_from_slice(&[128u8, 110, 90, 255]);
        }
        let stats = compute_stats(&px, 32, 32);
        let res = auto_color(&stats);
        assert!(
            res.grade.temperature > 6500.0,
            "temperature = {}",
            res.grade.temperature
        );
        assert!(res.explanation.iter().any(|e| e.contains("White balance")));
    }

    #[test]
    fn skin_heavy_frame_pulls_target_toward_median() {
        // Frame of skin-tone pixels, slightly dark (p50 < 0.42): the
        // skin-aware pull must produce a SMALLER lift than the naive
        // log2(0.42/p50), so faces don't blow out.
        let mut px = Vec::new();
        for _ in 0..32 * 32 {
            px.extend_from_slice(&[120u8, 88, 70, 255]); // skin hue, darkish
        }
        let stats = compute_stats(&px, 32, 32);
        assert!(
            stats.skin_fraction > 0.3,
            "skin_fraction = {}",
            stats.skin_fraction
        );
        assert!(stats.p50 < 0.42, "p50 = {}", stats.p50);
        let res = auto_color(&stats);
        let naive = (0.42f32 / stats.p50.max(1e-4)).log2();
        assert!(naive > 0.0, "test needs a positive naive lift, got {naive}");
        assert!(
            res.grade.exposure < naive,
            "skin-aware {} should be < naive {naive}",
            res.grade.exposure
        );
        assert!(
            res.grade.exposure > 0.0,
            "should still lift, got {}",
            res.grade.exposure
        );
        assert!(res.explanation.iter().any(|e| e.contains("Skin-aware")));
    }

    #[test]
    fn temperature_fit_is_self_consistent() {
        // A perfect 6500 K neutral frame must fit back to ~6500 K.
        assert!((fit_temperature(1.0, 1.0) - 6500.0).abs() < 25.0);
        // A frame needing a red cut (need_r < 1) must fit cooler (> 6500 K).
        assert!(fit_temperature(0.83, 1.0) > 6500.0);
        // A frame needing a red boost (need_r > 1) must fit warmer (< 6500 K).
        assert!(fit_temperature(1.2, 1.0) < 6500.0);
    }
}
