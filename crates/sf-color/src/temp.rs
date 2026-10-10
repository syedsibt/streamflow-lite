//! White-balance gains from color temperature (Kelvin) plus green/magenta tint.
//!
//! Uses the Tanner–Helland blackbody approximation to estimate the color of a
//! blackbody radiator, then derives per-channel gains from it.
//!
//! # Formula contract (shared with the WGSL grade shader)
//!
//! ```text
//! t = kelvin / 100
//! r = t <= 66 ? 255 : 329.698727446 * (t - 60)^-0.1332047592
//! g = t <= 66 ? 99.4708025861 * ln(t) - 161.1195681661
//!             : 288.1221695283 * (t - 60)^-0.0755148492
//! b = t >= 66 ? 255 : t <= 19 ? 0
//!             : 138.5177312231 * ln(t - 10) - 305.0447927307
//! clamp each channel to 0..255                    ->  bb(kelvin)
//!
//! rel[i] = bb(kelvin)[i] / max(bb(6500)[i], 1e-3)  (per channel)
//! gains  = rel / rel[green]                        (green held at 1.0)
//!
//! tint >= 0:  gains[green] *= 1 + tint * 0.003
//! tint <  0:  gains[red]   *= 1 - tint * 0.003
//!             gains[blue]  *= 1 - tint * 0.003
//! ```
//!
//! This is the *creative* temperature convention used by the Color Studio UI:
//! the image is gained **toward** the blackbody color at `kelvin`, relative to
//! D65. Lower Kelvin warms the image (red gain > 1 > blue gain), higher Kelvin
//! cools it. Green is normalized to 1.0 so temperature never changes overall
//! exposure, and `kelvin_to_gains(6500, 0)` is exactly `[1, 1, 1]` (identity).
//!
//! # Note on the spec formula
//!
//! An earlier draft of this spec wrote `gains = bb(6500) / bb(kelvin)`. That
//! formula cannot satisfy the required behavior: red saturates at 255 for every
//! `t <= 66`, so the red gain would be exactly 1.0 at 3200 K while the blue
//! gain would exceed 1.0 — a *cooling* camera-WB correction, failing the
//! `warm[0] > 1.0 > warm[2]` contract. The formula above is the corrected
//! contract; the WGSL shader must implement it identically.

/// Tanner–Helland blackbody approximation.
///
/// Returns the blackbody color at `kelvin` as `(r, g, b)` in 0..255.
fn blackbody_rgb(kelvin: f32) -> [f32; 3] {
    let t = kelvin / 100.0;
    let r = if t <= 66.0 {
        255.0
    } else {
        329.698727446 * (t - 60.0).powf(-0.1332047592)
    };
    let g = if t <= 66.0 {
        99.4708025861 * t.ln() - 161.1195681661
    } else {
        288.1221695283 * (t - 60.0).powf(-0.0755148492)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.5177312231 * (t - 10.0).ln() - 305.0447927307
    };
    [
        r.clamp(0.0, 255.0),
        g.clamp(0.0, 255.0),
        b.clamp(0.0, 255.0),
    ]
}

/// Per-channel white-balance gains for `kelvin` (2500..10000 K UI range) and
/// `tint` (-100..100, positive = green, negative = magenta).
///
/// `kelvin_to_gains(6500.0, 0.0)` returns exactly `[1.0, 1.0, 1.0]`.
pub fn kelvin_to_gains(kelvin: f32, tint: f32) -> [f32; 3] {
    const EPS: f32 = 1e-3; // guards degenerate denominators outside the UI range
    let src = blackbody_rgb(kelvin);
    let dst = blackbody_rgb(6500.0);

    // Creative convention: gain toward the blackbody color at `kelvin`,
    // relative to D65, with green normalized to 1.0.
    let mut rel = [0.0f32; 3];
    for i in 0..3 {
        rel[i] = src[i] / dst[i].max(EPS);
    }
    let g = rel[1].max(EPS);
    let mut gains = [rel[0] / g, 1.0, rel[2] / g];

    if tint >= 0.0 {
        gains[1] *= 1.0 + tint * 0.003;
    } else {
        gains[0] *= 1.0 - tint * 0.003;
        gains[2] *= 1.0 - tint * 0.003;
    }
    gains
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d65_is_identity() {
        let g = kelvin_to_gains(6500.0, 0.0);
        for (i, v) in g.iter().enumerate() {
            assert!((v - 1.0).abs() < 1e-6, "channel {i}: {v}");
        }
    }

    #[test]
    fn low_kelvin_warms_high_kelvin_cools() {
        let warm = kelvin_to_gains(3200.0, 0.0);
        assert!(
            warm[0] > 1.0 && 1.0 > warm[2],
            "3200 K should warm: {warm:?}"
        );
        assert!(
            (warm[1] - 1.0).abs() < 1e-6,
            "green stays 1.0 without tint: {warm:?}"
        );
        let cool = kelvin_to_gains(8000.0, 0.0);
        assert!(
            cool[0] < 1.0 && cool[2] > 1.0,
            "8000 K should cool: {cool:?}"
        );
    }

    #[test]
    fn tint_shifts_green_magenta() {
        let pos = kelvin_to_gains(6500.0, 50.0);
        assert!((pos[1] - 1.15).abs() < 1e-6, "{pos:?}");
        assert!((pos[0] - 1.0).abs() < 1e-6 && (pos[2] - 1.0).abs() < 1e-6);
        let neg = kelvin_to_gains(6500.0, -40.0);
        assert!((neg[0] - 1.12).abs() < 1e-6, "{neg:?}");
        assert!((neg[2] - 1.12).abs() < 1e-6, "{neg:?}");
        assert!((neg[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn temperature_is_monotone() {
        // Warmer (lower K) => strictly larger red/blue ratio across the range.
        let mut prev = f32::INFINITY;
        for k in [2500.0, 3200.0, 4500.0, 6500.0, 8000.0, 10000.0] {
            let g = kelvin_to_gains(k, 0.0);
            let ratio = g[0] / g[2];
            assert!(ratio < prev, "k={k}: ratio {ratio} not decreasing");
            prev = ratio;
        }
    }
}
