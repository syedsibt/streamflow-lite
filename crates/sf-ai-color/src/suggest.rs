//! AI Look Suggest (spec §6.4).
//!
//! Analyzes [`FrameStats`] and scores the looks in a [`LookRegistry`],
//! returning the top-`n` suggestions with a reason each. Suggestions are
//! starting points — applying one lands editable parameters via the normal
//! look path. Scoring is a pure function of the stats and the registry:
//! deterministic, with ties broken by look id.

use sf_filters::LookRegistry;

use crate::stats::FrameStats;

/// One look suggestion: which preset, and why.
pub struct LookSuggestion {
    /// Preset id (`.sflook` file stem), guaranteed present in the registry.
    pub look_id: String,
    /// Display name from the preset.
    pub name: String,
    /// Human-readable reason for the suggestion.
    pub reason: String,
}

/// Score the registry's looks against frame statistics (spec §6.4).
///
/// Heuristics (each adds to a look's score; only ids present in `assets/looks/`
/// are ever referenced — e.g. there is deliberately no `fix-warm-cast`
/// suggestion because that preset does not exist):
/// - dark frame (`p50 < 0.25`) → `fix-low-light`, `bright-room`, `vlog-fresh`;
/// - overexposed (`p99 > 0.97` or `p50 > 0.78`) → `fix-overexposed`;
/// - flat (`p95 − p5 < 0.35`) → `fix-flat-camera`;
/// - desaturated (`mean_chroma < 0.08`) → `vivid-screen`, `youtube-pop`, `concert-vivid`;
/// - already colorful (`mean_chroma > 0.25`) → `teal-orange`, `matte-cinema`;
/// - skin-heavy (`skin_fraction > 0.12`) → `skin-glow`, `talking-head`;
/// - warm cast (`gray_r/gray_b > 1.12`) → `studio-clean`, `broadcast-neutral`
///   (neutral starting points; no warm-cast fixer ships);
/// - cool cast (`gray_b/gray_r > 1.12`) → `church-warm`, `podcast-warm`;
/// - otherwise → `studio-clean`, `talking-head`, `soft-natural` as safe defaults.
///
/// Results sort by score descending, ties broken by look id ascending, so two
/// runs on the same stats always return the same list.
pub fn suggest_looks(stats: &FrameStats, registry: &LookRegistry, n: usize) -> Vec<LookSuggestion> {
    // (look id, score bump, reason)
    let mut scored: Vec<(&str, f32, String)> = Vec::new();
    let mut add = |id: &'static str, score: f32, reason: String| {
        if registry.get(id).is_some() {
            scored.push((id, score, reason));
        }
    };

    let spread = (stats.p95 - stats.p5).max(0.0);

    if stats.p50 < 0.25 {
        add(
            "fix-low-light",
            150.0,
            format!(
                "Low light: median luma {:.2} — lifts exposure and shadows without crushing blacks.",
                stats.p50
            ),
        );
        add(
            "bright-room",
            60.0,
            "Brightens a dim frame while keeping a natural feel.".to_string(),
        );
        add(
            "vlog-fresh",
            40.0,
            "Adds gentle exposure and life to a dark vlog frame.".to_string(),
        );
    }
    if stats.p99 > 0.97 || stats.p50 > 0.78 {
        add(
            "fix-overexposed",
            150.0,
            format!(
                "Overexposed: highlights clip at {:.2} — pulls exposure down and recovers highlight detail.",
                stats.p99
            ),
        );
    }
    if spread < 0.35 {
        add(
            "fix-flat-camera",
            90.0,
            format!(
                "Flat image: p5–p95 spread only {:.2} — adds contrast and punch.",
                spread
            ),
        );
    }
    if stats.mean_chroma < 0.08 {
        add(
            "vivid-screen",
            70.0,
            "Desaturated frame — adds saturation and sharpness for screen content.".to_string(),
        );
        add(
            "youtube-pop",
            60.0,
            "Muted colors — a punchy creator grade for more pop.".to_string(),
        );
        add(
            "concert-vivid",
            45.0,
            "Low color energy — vivid saturation and vibrance.".to_string(),
        );
    }
    if stats.mean_chroma > 0.25 {
        add(
            "teal-orange",
            60.0,
            "Already colorful — a cinematic teal-orange grade to shape the palette.".to_string(),
        );
        add(
            "matte-cinema",
            40.0,
            "Rich color — a filmic matte finish with lifted blacks.".to_string(),
        );
    }
    if stats.skin_fraction > 0.12 {
        add(
            "skin-glow",
            80.0,
            format!(
                "Faces detected ({:.0}% of frame) — warm glow tuned for skin tones.",
                stats.skin_fraction * 100.0
            ),
        );
        add(
            "talking-head",
            70.0,
            "Talking-head framing — clean, camera-ready skin rendering.".to_string(),
        );
    }
    let rb = stats.gray_r.max(1e-4) / stats.gray_b.max(1e-4);
    if rb > 1.12 {
        add(
            "studio-clean",
            50.0,
            "Warm cast detected — a neutral grade as a clean starting point.".to_string(),
        );
        add(
            "broadcast-neutral",
            40.0,
            "Color cast detected — resets toward a neutral broadcast baseline.".to_string(),
        );
    } else if 1.0 / rb > 1.12 {
        add(
            "church-warm",
            50.0,
            "Cool cast detected — gentle warmth to balance it.".to_string(),
        );
        add(
            "podcast-warm",
            40.0,
            "Cool cast detected — a warm creator grade to balance it.".to_string(),
        );
    }

    // Safe defaults so `n` suggestions exist even for a healthy frame.
    add(
        "studio-clean",
        30.0,
        "Healthy frame — a clean, neutral grade as a starting point.".to_string(),
    );
    add(
        "talking-head",
        25.0,
        "Balanced exposure — a camera-ready talking-head polish.".to_string(),
    );
    add(
        "soft-natural",
        20.0,
        "Natural look — soft contrast for an easy, organic feel.".to_string(),
    );

    // Deterministic ordering: score desc, look id asc on ties.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    scored.truncate(n);
    scored
        .into_iter()
        .map(|(id, _, reason)| {
            let name = registry
                .get(id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.to_string());
            LookSuggestion {
                look_id: id.to_string(),
                name,
                reason,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_registry() -> LookRegistry {
        let dir: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", "assets", "looks"]
            .iter()
            .collect();
        LookRegistry::load_dir(&dir).expect("assets/looks must load")
    }

    fn stats_of(rgba: &[u8], w: u32, h: u32) -> FrameStats {
        crate::stats::compute_stats(rgba, w, h)
    }

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            out.extend_from_slice(&[v, v, v, 255]);
        }
        out
    }

    #[test]
    fn registry_loads_real_builtin_looks() {
        let reg = test_registry();
        assert!(reg.len() >= 30, "expected ≥30 built-in looks");
        for id in [
            "fix-low-light",
            "fix-overexposed",
            "fix-flat-camera",
            "vivid-screen",
            "youtube-pop",
            "skin-glow",
            "talking-head",
            "teal-orange",
            "studio-clean",
            "broadcast-neutral",
        ] {
            assert!(reg.get(id).is_some(), "missing built-in look {id}");
        }
        // Pinned: this preset must NOT be referenced — it does not exist.
        assert!(reg.get("fix-warm-cast").is_none());
    }

    #[test]
    fn dark_frame_suggests_fix_low_light() {
        let reg = test_registry();
        let stats = stats_of(&solid(32, 32, 26), 32, 32); // ~0.1 gray
        let suggestions = suggest_looks(&stats, &reg, 3);
        assert_eq!(suggestions.len(), 3);
        let top3: Vec<&str> = suggestions.iter().map(|s| s.look_id.as_str()).collect();
        assert!(
            top3.contains(&"fix-low-light"),
            "fix-low-light not in top 3: {top3:?}"
        );
        let fix = suggestions
            .iter()
            .find(|s| s.look_id == "fix-low-light")
            .unwrap();
        assert!(
            fix.reason.to_lowercase().contains("light"),
            "reason lacks 'light': {}",
            fix.reason
        );
        assert!(!fix.name.is_empty());
    }

    #[test]
    fn suggestions_are_deterministic() {
        let reg = test_registry();
        let stats = stats_of(&solid(32, 32, 200), 32, 32);
        let a = suggest_looks(&stats, &reg, 5);
        let b = suggest_looks(&stats, &reg, 5);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.look_id, y.look_id);
            assert_eq!(x.reason, y.reason);
        }
    }

    #[test]
    fn overexposed_frame_suggests_fix_overexposed() {
        let reg = test_registry();
        let stats = stats_of(&solid(32, 32, 250), 32, 32);
        let suggestions = suggest_looks(&stats, &reg, 3);
        let ids: Vec<&str> = suggestions.iter().map(|s| s.look_id.as_str()).collect();
        assert!(ids.contains(&"fix-overexposed"), "top 3: {ids:?}");
    }

    #[test]
    fn skin_heavy_frame_suggests_skin_looks() {
        let reg = test_registry();
        let mut px = Vec::new();
        for _ in 0..32 * 32 {
            px.extend_from_slice(&[200u8, 150, 120, 255]);
        }
        let stats = stats_of(&px, 32, 32);
        assert!(stats.skin_fraction > 0.5);
        let suggestions = suggest_looks(&stats, &reg, 3);
        let ids: Vec<&str> = suggestions.iter().map(|s| s.look_id.as_str()).collect();
        assert!(
            ids.contains(&"skin-glow") || ids.contains(&"talking-head"),
            "top 3: {ids:?}"
        );
    }

    #[test]
    fn all_suggested_ids_exist_in_registry() {
        let reg = test_registry();
        // Sweep a few representative frames; every suggestion must resolve.
        for v in [26u8, 107, 200, 250] {
            let stats = stats_of(&solid(16, 16, v), 16, 16);
            for s in suggest_looks(&stats, &reg, 5) {
                assert!(reg.get(&s.look_id).is_some(), "dangling id {}", s.look_id);
            }
        }
    }
}
