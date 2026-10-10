//! Auto Drift Guard (spec §6.5).
//!
//! Watches an input's [`FrameStats`] over time during a live session and
//! **suggests** a re-match when the lighting drifts — it never mutates any
//! grade. The UI turns a [`DriftSuggestion`] into a toast
//! ("Cam 2 exposure drift ho gaya, re-match?") and, via the `color.drift_suggest`
//! IPC event, offers one-click re-match.
//!
//! By construction this module cannot change a grade: it only ever reads
//! `&FrameStats` and emits suggestion structs.

use crate::stats::FrameStats;

/// Exposure drifts past ±0.5 EV from the baseline median.
const EXPOSURE_DRIFT_EV: f32 = 0.5;
/// Neutral-channel ratio shifts past 20% from baseline count as WB drift.
const WB_DRIFT_RATIO: f32 = 0.20;

/// What kind of lighting drift was detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftKind {
    /// Median luma moved more than ±0.5 EV from baseline.
    Exposure,
    /// Neutral-region channel ratios shifted more than 20% from baseline.
    WhiteBalance,
}

/// A drift suggestion for one input — display it, never auto-apply.
pub struct DriftSuggestion {
    /// Target input id.
    pub input_id: String,
    /// Which drift was detected.
    pub kind: DriftKind,
    /// Human-readable message for the UI toast.
    pub message: String,
}

/// Per-input drift guard with hysteresis.
///
/// Created per input via [`DriftGuard::new`]; feed it fresh stats with
/// [`DriftGuard::update`]. The first update primes the baseline. After a
/// suggestion is emitted, no further suggestions are produced until
/// [`DriftGuard::rebaseline`] is called (hysteresis) — the user either
/// re-matched (which rebaselines) or dismissed the toast.
pub struct DriftGuard {
    input_id: String,
    base: Option<FrameStats>,
    suggested: bool,
}

impl DriftGuard {
    /// Create a guard for `input_id`. No baseline yet — the first
    /// [`DriftGuard::update`] primes it and returns `None`.
    pub fn new(input_id: impl Into<String>) -> Self {
        Self {
            input_id: input_id.into(),
            base: None,
            suggested: false,
        }
    }

    /// The input id this guard watches.
    pub fn input_id(&self) -> &str {
        &self.input_id
    }

    /// (Re)set the baseline to `stats` and clear the hysteresis latch, so
    /// future drift can be suggested again.
    pub fn rebaseline(&mut self, stats: &FrameStats) {
        self.base = Some(stats.clone());
        self.suggested = false;
    }

    /// Check `stats` against the baseline.
    ///
    /// Returns `Some` suggestion on the first drift detected past a threshold
    /// (exposure checked before white balance), `None` otherwise — including
    /// every call after a suggestion until [`DriftGuard::rebaseline`].
    /// Never touches any grade.
    pub fn update(&mut self, stats: &FrameStats) -> Option<DriftSuggestion> {
        let Some(base) = self.base.clone() else {
            // Self-prime on first use: nothing to compare against yet.
            self.base = Some(stats.clone());
            return None;
        };
        if self.suggested {
            return None; // hysteresis: wait for rebaseline
        }

        // --- Exposure: |log2(p50 / base_p50)| > 0.5 EV ---
        let ev = (stats.p50.max(1e-4) / base.p50.max(1e-4)).log2();
        if ev.abs() > EXPOSURE_DRIFT_EV {
            self.suggested = true;
            let dir = if ev > 0.0 { "brighter" } else { "darker" };
            return Some(DriftSuggestion {
                input_id: self.input_id.clone(),
                kind: DriftKind::Exposure,
                message: format!(
                    "{} exposure drift ho gaya ({dir}, {ev:+.1} EV vs baseline) — re-match?",
                    self.input_id
                ),
            });
        }

        // --- White balance: neutral R/B or G/B ratio shifted > 20% ---
        let ratio = |s: &FrameStats| {
            let (r, g, b) = (s.gray_r.max(1e-4), s.gray_g.max(1e-4), s.gray_b.max(1e-4));
            (r / b, g / b)
        };
        let (base_rb, base_gb) = ratio(&base);
        let (cur_rb, cur_gb) = ratio(stats);
        let shift_rb = ((cur_rb - base_rb) / base_rb).abs();
        let shift_gb = ((cur_gb - base_gb) / base_gb).abs();
        if shift_rb > WB_DRIFT_RATIO || shift_gb > WB_DRIFT_RATIO {
            self.suggested = true;
            return Some(DriftSuggestion {
                input_id: self.input_id.clone(),
                kind: DriftKind::WhiteBalance,
                message: format!(
                    "{} white balance drift ho gaya (neutral ratios shifted {:.0}%) — re-match?",
                    self.input_id,
                    shift_rb.max(shift_gb) * 100.0
                ),
            });
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::compute_stats;
    use sf_color::ColorGrade;

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&[r, g, b, 255]);
        }
        v
    }

    fn stats_of(r: u8, g: u8, b: u8) -> FrameStats {
        compute_stats(&solid(32, 32, r, g, b), 32, 32)
    }

    #[test]
    fn stable_stats_produce_no_suggestion() {
        let mut guard = DriftGuard::new("cam2");
        let base = stats_of(107, 107, 107);
        assert!(guard.update(&base).is_none()); // primes baseline
        assert!(guard.update(&stats_of(107, 107, 107)).is_none());
        assert!(guard.update(&stats_of(110, 108, 106)).is_none()); // small wobble
    }

    #[test]
    fn halved_median_suggests_exposure_and_touches_no_grade() {
        let mut guard = DriftGuard::new("cam2");
        let grade_before = ColorGrade::default();
        let mut grade = grade_before.clone();

        guard.update(&stats_of(107, 107, 107)); // baseline ≈ 0.42
                                                // p50 halved ≈ −1 EV of drift (> 0.5 EV threshold).
        let suggestion = guard.update(&stats_of(54, 54, 54));
        let suggestion = suggestion.expect("expected an exposure suggestion");
        assert_eq!(suggestion.kind, DriftKind::Exposure);
        assert_eq!(suggestion.input_id, "cam2");
        assert!(
            suggestion.message.contains("re-match"),
            "message: {}",
            suggestion.message
        );

        // The guard never mutates any grade: assert by construction (no grade
        // parameter exists) and by value.
        let _ = &mut grade;
        assert_eq!(grade, grade_before);
    }

    #[test]
    fn wb_shift_suggests_white_balance() {
        let mut guard = DriftGuard::new("cam1");
        guard.update(&stats_of(107, 107, 107));
        // Same luma, strong red cast: R/B ratio shifts ~40% (> 20%).
        let suggestion = guard.update(&stats_of(120, 107, 85));
        let suggestion = suggestion.expect("expected a WB suggestion");
        assert_eq!(suggestion.kind, DriftKind::WhiteBalance);
        assert!(suggestion.message.contains("re-match"));
    }

    #[test]
    fn hysteresis_blocks_repeats_until_rebaseline() {
        let mut guard = DriftGuard::new("cam2");
        guard.update(&stats_of(107, 107, 107));
        assert!(guard.update(&stats_of(54, 54, 54)).is_some());
        // Still drifted, but no repeat without rebaseline.
        assert!(guard.update(&stats_of(54, 54, 54)).is_none());
        assert!(guard.update(&stats_of(50, 50, 50)).is_none());
        // Rebaseline clears the latch: new drift can be suggested again.
        guard.rebaseline(&stats_of(54, 54, 54));
        assert!(guard.update(&stats_of(27, 27, 27)).is_some());
    }

    #[test]
    fn exposure_checked_before_white_balance() {
        let mut guard = DriftGuard::new("cam2");
        guard.update(&stats_of(107, 107, 107));
        // Both drifted: exposure wins the ordering.
        let s = guard.update(&stats_of(54, 54, 40)).unwrap();
        assert_eq!(s.kind, DriftKind::Exposure);
    }
}
