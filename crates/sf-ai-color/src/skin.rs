//! Skin-tone protection masks (spec §6.3).
//!
//! Two evidence sources: a hue mask around the vectorscope skin-tone guide
//! ([`sf_scopes::SKIN_TONE_ANGLE_DEG`], 123°) and, when the ONNX runtime has a
//! model loaded, the [`FaceMask`] from an [`InferenceSession`]. Both feed
//! soft (0..1) per-pixel weights — never hard binary masks.

use sf_ai_runtime::FaceMask;
use sf_color::ColorGrade;

use crate::stats::{rgb_to_hue_deg, skin_hue_weight};

/// Chroma below this is achromatic: hue is meaningless, weight is 0.
const MIN_CHROMA: f32 = 0.02;

/// Soft per-pixel skin evidence mask, weights `0.0..=1.0`.
#[derive(Clone, Debug, PartialEq)]
pub struct SkinMask {
    /// Mask width in pixels.
    pub width: u32,
    /// Mask height in pixels.
    pub height: u32,
    /// Row-major per-pixel weights.
    pub weights: Vec<f32>,
}

impl SkinMask {
    /// Mean weight over the mask — the overall skin presence `0.0..=1.0`.
    pub fn mean_weight(&self) -> f32 {
        if self.weights.is_empty() {
            return 0.0;
        }
        self.weights.iter().sum::<f32>() / self.weights.len() as f32
    }
}

/// Skin mask from the hue heuristic alone: per-pixel vectorscope-angle
/// distance to the 123° skin-tone guide with the raised-cosine falloff from
/// [`skin_hue_weight`] — soft edges by construction, no hard thresholds.
pub fn skin_mask_hue(rgba: &[u8], width: u32, height: u32) -> SkinMask {
    let n = width as usize * height as usize;
    assert_eq!(rgba.len(), n * 4, "skin_mask_hue: rgba length mismatch");
    let mut weights = Vec::with_capacity(n);
    for px in rgba.chunks_exact(4) {
        let r = px[0] as f32 / 255.0;
        let g = px[1] as f32 / 255.0;
        let b = px[2] as f32 / 255.0;
        let chroma = r.max(g).max(b) - r.min(g).min(b);
        weights.push(if chroma < MIN_CHROMA {
            0.0
        } else {
            skin_hue_weight(rgb_to_hue_deg(r, g, b))
        });
    }
    SkinMask {
        width,
        height,
        weights,
    }
}

/// Combined skin mask: the ONNX [`FaceMask`] when available, else the hue
/// path.
///
/// When a face mask is present it is nearest-neighbor resampled to the frame
/// size (the session runs at its own native resolution) and combined with
/// the hue weights by **union** (`max`): either detector's evidence protects
/// the pixel. Rationale — the two detectors fail differently (the ONNX model
/// misses unusual poses/lighting; the hue heuristic misses desaturated or
/// strongly graded skin), and for a *protection* mask a false positive only
/// means slightly less saturation somewhere, while a false negative means a
/// face gets an unwanted color shift. Union errs toward protection.
///
/// With `face: None` this is exactly [`skin_mask_hue`].
pub fn skin_mask_combined(
    rgba: &[u8],
    width: u32,
    height: u32,
    face: Option<&FaceMask>,
) -> SkinMask {
    let mut mask = skin_mask_hue(rgba, width, height);
    let Some(face) = face else {
        return mask;
    };
    if face.weights.len() != face.width as usize * face.height as usize
        || face.width == 0
        || face.height == 0
    {
        return mask; // malformed mask: fall back to the hue path
    }
    for y in 0..height {
        let fy =
            (y as u64 * face.height as u64 / height as u64).min(face.height as u64 - 1) as usize;
        for x in 0..width {
            let fx =
                (x as u64 * face.width as u64 / width as u64).min(face.width as u64 - 1) as usize;
            let fw = face.weights[fy * face.width as usize + fx].clamp(0.0, 1.0);
            let idx = (y * width + x) as usize;
            mask.weights[idx] = mask.weights[idx].max(fw);
        }
    }
    mask
}

/// Overall protection strength for a mask: the mean weight (`0.0..=1.0`).
///
/// A strength near 1 means the frame is dominated by skin (e.g. a talking
/// head); near 0 means no skin evidence. Callers pass this to
/// [`attenuate_for_skin`].
pub fn protection_strength(mask: &SkinMask) -> f32 {
    mask.mean_weight().clamp(0.0, 1.0)
}

/// Scale a grade's saturation/vibrance deltas toward neutral in proportion to
/// skin `strength` (`0.0..=1.0`).
///
/// `saturation = 100 + (saturation − 100)·(1 − strength)`,
/// `vibrance *= (1 − strength)`.
///
/// This is the **editable-parameter counterpart** of the GPU grade pass's
/// skin-protect heuristic, and it composes with it as follows: the GPU
/// heuristic (engaged by `skin_protect: true` in the uniform) limits
/// saturation/vibrance *spatially inside* detected skin at render time and is
/// the final guard; `attenuate_for_skin` instead bakes the attenuation into
/// the parameters themselves (e.g. when auto-applying a strong look onto a
/// skin-heavy scene), so what the user sees on the sliders is what renders.
/// Use one or the other for a given change — stacking both compounds the
/// protection.
///
/// Respects the existing `skin_protect` flag semantics: when the flag is
/// off, the grade carries no skin-protection intent and this function is a
/// no-op.
pub fn attenuate_for_skin(grade: &mut ColorGrade, strength: f32) {
    if !grade.skin_protect {
        return;
    }
    let k = (1.0 - strength.clamp(0.0, 1.0)).max(0.0);
    grade.saturation = 100.0 + (grade.saturation - 100.0) * k;
    grade.vibrance *= k;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&[r, g, b, 255]);
        }
        v
    }

    #[test]
    fn hue_mask_detects_skin_patch() {
        // (200,150,120): the C4 vectorscope test's skin patch.
        let mask = skin_mask_hue(&solid(16, 16, 200, 150, 120), 16, 16);
        assert_eq!(mask.weights.len(), 256);
        assert!(
            mask.mean_weight() > 0.7,
            "skin patch mean weight = {}",
            mask.mean_weight()
        );
    }

    #[test]
    fn hue_mask_rejects_pure_blue() {
        let mask = skin_mask_hue(&solid(16, 16, 0, 0, 255), 16, 16);
        assert!(
            mask.mean_weight() < 0.05,
            "blue mean weight = {}",
            mask.mean_weight()
        );
    }

    #[test]
    fn hue_mask_has_no_hard_edges() {
        // A gradient sweeping skin → blue must pass through the falloff band,
        // producing weights strictly between 0 and 1 (no binary jump).
        let w = 64u32;
        let mut px = Vec::with_capacity((w * 4) as usize);
        for x in 0..w {
            let t = x as f32 / (w - 1) as f32;
            let r = (200.0 * (1.0 - t)) as u8;
            let g = (150.0 * (1.0 - t)) as u8;
            let b = (120.0 * (1.0 - t) + 255.0 * t) as u8;
            px.extend_from_slice(&[r, g, b, 255]);
        }
        let mask = skin_mask_hue(&px, w, 1);
        let soft = mask.weights.iter().filter(|&&w| w > 0.0 && w < 1.0).count();
        assert!(
            soft > 0,
            "no soft-falloff weights found; weights = {:?}",
            &mask.weights[..]
        );
        // And the endpoints behave: skin end strong, blue end zero.
        assert!(mask.weights[0] > 0.7);
        assert_eq!(mask.weights[(w - 1) as usize], 0.0);
    }

    #[test]
    fn combined_with_face_mask_unions_evidence() {
        // Blue frame: hue path says 0 everywhere. An all-ones face mask must
        // still protect (union), at matching resolution.
        let rgba = solid(8, 8, 0, 0, 255);
        let face = FaceMask {
            width: 8,
            height: 8,
            weights: vec![1.0; 64],
        };
        let mask = skin_mask_combined(&rgba, 8, 8, Some(&face));
        assert!(
            mask.mean_weight() > 0.99,
            "union mean = {}",
            mask.mean_weight()
        );
    }

    #[test]
    fn combined_resamples_mismatched_face_mask() {
        // 2×2 face mask over an 8×8 blue frame: nearest-neighbor resample,
        // union with the (zero) hue weights.
        let rgba = solid(8, 8, 0, 0, 255);
        let face = FaceMask {
            width: 2,
            height: 2,
            weights: vec![0.0, 1.0, 1.0, 0.0],
        };
        let mask = skin_mask_combined(&rgba, 8, 8, Some(&face));
        // Top-left quadrant 0, top-right quadrant 1, etc.
        assert_eq!(mask.weights[0], 0.0);
        assert_eq!(mask.weights[7], 1.0);
        assert_eq!(mask.weights[8 * 7], 1.0);
        assert_eq!(mask.weights[8 * 7 + 7], 0.0);
    }

    #[test]
    fn combined_without_face_equals_hue_path() {
        let rgba = solid(8, 8, 200, 150, 120);
        assert_eq!(
            skin_mask_combined(&rgba, 8, 8, None),
            skin_mask_hue(&rgba, 8, 8)
        );
    }

    #[test]
    fn protection_strength_is_mean_weight() {
        let mask = skin_mask_hue(&solid(16, 16, 200, 150, 120), 16, 16);
        assert!((protection_strength(&mask) - mask.mean_weight()).abs() < 1e-6);
        let empty = SkinMask {
            width: 0,
            height: 0,
            weights: vec![],
        };
        assert_eq!(protection_strength(&empty), 0.0);
    }

    #[test]
    fn attenuate_scales_deltas_toward_neutral() {
        let mut grade = ColorGrade::default();
        grade.skin_protect = true;
        grade.saturation = 140.0;
        grade.vibrance = 40.0;

        // Full strength: back to neutral.
        attenuate_for_skin(&mut grade, 1.0);
        assert!((grade.saturation - 100.0).abs() < 1e-5);
        assert!(grade.vibrance.abs() < 1e-5);

        // Half strength: halfway.
        let mut grade = ColorGrade::default();
        grade.skin_protect = true;
        grade.saturation = 140.0;
        grade.vibrance = 40.0;
        attenuate_for_skin(&mut grade, 0.5);
        assert!((grade.saturation - 120.0).abs() < 1e-5);
        assert!((grade.vibrance - 20.0).abs() < 1e-5);

        // Zero strength: untouched.
        let mut grade = ColorGrade::default();
        grade.skin_protect = true;
        grade.saturation = 140.0;
        attenuate_for_skin(&mut grade, 0.0);
        assert_eq!(grade.saturation, 140.0);
    }

    #[test]
    fn attenuate_is_noop_when_skin_protect_off() {
        // Respects the flag semantics: no skin-protection intent → no change.
        let mut grade = ColorGrade::default();
        grade.saturation = 140.0;
        grade.vibrance = 40.0;
        attenuate_for_skin(&mut grade, 1.0);
        assert_eq!(grade.saturation, 140.0);
        assert_eq!(grade.vibrance, 40.0);
    }

    #[test]
    fn attenuate_never_overshoots_neutral() {
        // A desaturating grade (below 100) must move toward 100, not past it.
        let mut grade = ColorGrade::default();
        grade.skin_protect = true;
        grade.saturation = 60.0;
        attenuate_for_skin(&mut grade, 0.5);
        assert!((grade.saturation - 80.0).abs() < 1e-5);
    }
}
