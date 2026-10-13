//! Core color-grade parameter types.
//!
//! [`ColorGrade`] stores **UI units**, exactly as they appear in `.sflook`
//! JSON: exposure in EV; brightness/contrast/vibrance/tint/highlights/shadows/
//! whites/blacks in -100..100; saturation in 0..200 (%); temperature in
//! 2500..10000 K; gamma in 0.5..2.5; sharpness in 0..100. The wheels (`lift`,
//! `gamma_wheel`, `gain`, `offset`) are raw `[r, g, b, master]` values.
//!
//! [`ColorGrade::to_uniform`] normalizes those into the 160-byte
//! [`GradeUniform`] consumed by the wgpu grade pass (see `sf-render`). The CPU
//! reference in [`crate::cpu`] derives its math from the same normalized
//! values, so the two implementations cannot drift apart.

use serde::{Deserialize, Serialize};

use crate::curves::CurveSet;
use crate::temp::kelvin_to_gains;

/// Where the 3D LUT is applied in the grade chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LutPosition {
    /// Applied right after linearization, before exposure / WB / grade.
    Before,
    /// Applied after the grade and curves, before output encoding.
    After,
}

impl Default for LutPosition {
    /// `.sflook` files may omit `position`; applying after the grade is the
    /// conventional default.
    fn default() -> Self {
        LutPosition::After
    }
}

fn default_lut_intensity() -> f32 {
    1.0
}

/// Reference to a `.cube` file, as stored in `.sflook` JSON and the project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LutRef {
    /// Project-relative path, e.g. `"color/luts/warm.cube"`.
    #[serde(default)]
    pub file: String,
    /// Blend amount, 0.0..1.0.
    #[serde(default = "default_lut_intensity")]
    pub intensity: f32,
    /// Position in the grade chain.
    #[serde(default)]
    pub position: LutPosition,
}

/// Serde defaults matching the neutral grade: a missing field must mean
/// "no change", not 0.0 (which would be black / grayscale / 0 K for
/// saturation, temperature, gamma, and the multiplicative wheels).
fn default_saturation() -> f32 {
    100.0
}
fn default_temperature() -> f32 {
    6500.0
}
fn default_gamma() -> f32 {
    1.0
}
fn default_ones() -> [f32; 4] {
    [1.0; 4]
}

/// Full per-input color grade in UI units.
///
/// Every field has a serde default, so partial `.sflook` JSON deserializes
/// with missing fields taking their neutral values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorGrade {
    /// Exposure in EV (-3..3).
    #[serde(default)]
    pub exposure: f32,
    /// -100..100.
    #[serde(default)]
    pub brightness: f32,
    /// -100..100.
    #[serde(default)]
    pub contrast: f32,
    /// 0..200 (%).
    #[serde(default = "default_saturation")]
    pub saturation: f32,
    /// -100..100.
    #[serde(default)]
    pub vibrance: f32,
    /// 2500..10000 K.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// -100..100 (positive = green, negative = magenta).
    #[serde(default)]
    pub tint: f32,
    /// -100..100.
    #[serde(default)]
    pub highlights: f32,
    /// -100..100.
    #[serde(default)]
    pub shadows: f32,
    /// -100..100.
    #[serde(default)]
    pub whites: f32,
    /// -100..100.
    #[serde(default)]
    pub blacks: f32,
    /// 0.5..2.5.
    #[serde(default = "default_gamma")]
    pub gamma: f32,
    /// 0..100.
    #[serde(default)]
    pub sharpness: f32,
    /// `[r, g, b, master]` lift offsets.
    #[serde(default)]
    pub lift: [f32; 4],
    /// `[r, g, b, master]` gamma exponents.
    #[serde(default = "default_ones")]
    pub gamma_wheel: [f32; 4],
    /// `[r, g, b, master]` gain multipliers.
    #[serde(default = "default_ones")]
    pub gain: [f32; 4],
    /// `[r, g, b, master]` offset additions.
    #[serde(default)]
    pub offset: [f32; 4],
    /// Spline curves.
    #[serde(default)]
    pub curves: CurveSet,
    /// Optional 3D LUT.
    #[serde(default)]
    pub lut: Option<LutRef>,
    /// Limit saturation/vibrance shifts inside skin tones.
    #[serde(default)]
    pub skin_protect: bool,
    /// Creative "faded film": lifts and softens the blacks, gently muting
    /// shadow color (0..100).
    #[serde(default)]
    pub faded_film: f32,
    /// Skin-only warmth shift, -100..100 (applied through the skin mask —
    /// the rest of the image is untouched).
    #[serde(default)]
    pub skin_warmth: f32,
    /// Skin-only saturation, -100..100.
    #[serde(default)]
    pub skin_saturation: f32,
    /// Skin-only brightness, -100..100.
    #[serde(default)]
    pub skin_brightness: f32,
}

impl Default for ColorGrade {
    /// The neutral grade: every control at its identity value.
    ///
    /// Note: `temperature` defaults to 6500 K (D65), which yields identity
    /// white-balance gains; "camera native" is a UI concept, not a parameter.
    fn default() -> Self {
        Self {
            exposure: 0.0,
            brightness: 0.0,
            contrast: 0.0,
            saturation: 100.0,
            vibrance: 0.0,
            temperature: 6500.0,
            tint: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            gamma: 1.0,
            sharpness: 0.0,
            lift: [0.0, 0.0, 0.0, 0.0],
            gamma_wheel: [1.0, 1.0, 1.0, 1.0],
            gain: [1.0, 1.0, 1.0, 1.0],
            offset: [0.0, 0.0, 0.0, 0.0],
            curves: CurveSet::default(),
            lut: None,
            skin_protect: false,
            faded_film: 0.0,
            skin_warmth: 0.0,
            skin_saturation: 0.0,
            skin_brightness: 0.0,
        }
    }
}

impl ColorGrade {
    /// Deltas-compose an expanded look onto this grade.
    ///
    /// `over` is an expanded look (from `sf_filters::apply_look`); each of
    /// its fields carries that look's *delta from identity*, and this method
    /// folds the delta into `self`. Field rules (the "identity" each delta
    /// is measured against):
    ///
    /// * Additive around 0: `exposure`, `brightness`, `contrast`,
    ///   `vibrance`, `tint`, `highlights`, `shadows`, `whites`, `blacks`,
    ///   `sharpness` — `self += over - 0`.
    /// * `saturation`: additive around 100 — `self += over - 100`.
    /// * `temperature`: additive around 6500 K — `self += over - 6500`.
    /// * `gamma`: multiplicative — `self *= over` (identity 1.0).
    /// * `lift`, `offset`: additive per channel; `gain`, `gamma_wheel`:
    ///   multiplicative per channel (identities `[0;4]` / `[1;4]`).
    /// * `curves`: last non-identity look wins (`over` replaces `self` when
    ///   `over.curves` is not identity; an identity look leaves `self`
    ///   untouched).
    /// * `lut`: last look carrying one wins (same rule as curves).
    /// * `skin_protect`: OR — `self |= over`.
    ///
    /// Consequence: layering `apply_look(preset, i)` onto the identity
    /// grade reproduces the expanded look exactly, and layering two looks
    /// sums their deltas (e.g. two +0.5 EV looks ≈ +1.0 EV).
    pub fn layer_look(&mut self, over: &ColorGrade) {
        self.exposure += over.exposure;
        self.brightness += over.brightness;
        self.contrast += over.contrast;
        self.vibrance += over.vibrance;
        self.tint += over.tint;
        self.highlights += over.highlights;
        self.shadows += over.shadows;
        self.whites += over.whites;
        self.blacks += over.blacks;
        self.sharpness += over.sharpness;
        self.saturation += over.saturation - 100.0;
        self.temperature += over.temperature - 6500.0;
        self.gamma *= over.gamma;
        for i in 0..4 {
            self.lift[i] += over.lift[i];
            self.offset[i] += over.offset[i];
            self.gain[i] *= over.gain[i];
            self.gamma_wheel[i] *= over.gamma_wheel[i];
        }
        if !over.curves.is_identity() {
            self.curves = over.curves.clone();
        }
        if over.lut.is_some() {
            self.lut = over.lut.clone();
        }
        self.skin_protect |= over.skin_protect;
        self.faded_film += over.faded_film;
        self.skin_warmth += over.skin_warmth;
        self.skin_saturation += over.skin_saturation;
        self.skin_brightness += over.skin_brightness;
    }

    /// True when every parameter is at its neutral value.
    ///
    /// The engine uses this to bypass the color pass entirely.
    /// `skin_protect` is excluded: with neutral parameters it cannot change
    /// the image.
    pub fn is_identity(&self) -> bool {
        self.exposure == 0.0
            && self.brightness == 0.0
            && self.contrast == 0.0
            && self.saturation == 100.0
            && self.vibrance == 0.0
            && self.temperature == 6500.0
            && self.tint == 0.0
            && self.highlights == 0.0
            && self.shadows == 0.0
            && self.whites == 0.0
            && self.blacks == 0.0
            && self.gamma == 1.0
            && self.sharpness == 0.0
            && self.lift == [0.0, 0.0, 0.0, 0.0]
            && self.gamma_wheel == [1.0, 1.0, 1.0, 1.0]
            && self.gain == [1.0, 1.0, 1.0, 1.0]
            && self.offset == [0.0, 0.0, 0.0, 0.0]
            && self.curves.is_identity()
            && self.lut.is_none()
            && self.faded_film == 0.0
            && self.skin_warmth == 0.0
            && self.skin_saturation == 0.0
            && self.skin_brightness == 0.0
    }

    /// Normalize UI units into the GPU uniform.
    ///
    /// - `brightness/contrast/vibrance/highlights/shadows/whites/blacks`
    ///   are divided by 100 (-> -1..1); `saturation` by 100 (-> 0..2);
    ///   `sharpness` by 100 (-> 0..1).
    /// - `wb_gain` comes from [`kelvin_to_gains`](crate::temp::kelvin_to_gains).
    /// - `gamma_w` entries are clamped to a minimum of 0.01 (div-by-zero guard).
    /// - `lut_flags`: bit 0 = LUT present with intensity > 0, bit 1 = LUT
    ///   positioned before the grade.
    /// - `transfer` is always 0.0 (gamma 2.4) and `use_limited_range` always
    ///   0.0 (full range) in the MVP.
    pub fn to_uniform(&self) -> GradeUniform {
        let lut_enabled = self.lut.as_ref().map_or(false, |l| l.intensity > 0.0);
        let lut_before = self
            .lut
            .as_ref()
            .map_or(false, |l| l.position == LutPosition::Before);
        let lut_flags = (u32::from(lut_enabled) | (u32::from(lut_before) << 1)) as f32;
        GradeUniform {
            exposure_ev: self.exposure,
            transfer: 0.0,
            use_limited_range: 0.0,
            brightness: self.brightness / 100.0,
            wb_gain: kelvin_to_gains(self.temperature, self.tint),
            _pad0: 0.0,
            lift: self.lift,
            gain: self.gain,
            gamma_w: self.gamma_wheel.map(|v| v.max(0.01)),
            offset: self.offset,
            contrast: self.contrast / 100.0,
            saturation: self.saturation / 100.0,
            vibrance: self.vibrance / 100.0,
            highlights: self.highlights / 100.0,
            shadows: self.shadows / 100.0,
            whites: self.whites / 100.0,
            blacks: self.blacks / 100.0,
            basic_gamma: self.gamma,
            sharpness: (self.sharpness / 100.0).clamp(0.0, 1.0),
            lut_intensity: self
                .lut
                .as_ref()
                .map_or(0.0, |l| l.intensity.clamp(0.0, 1.0)),
            lut_flags,
            skin_protect: if self.skin_protect { 1.0 } else { 0.0 },
            curves_enabled: if self.curves.is_identity() { 0.0 } else { 1.0 },
            faded_film: (self.faded_film / 100.0).clamp(0.0, 1.0),
            skin_warmth: (self.skin_warmth / 100.0).clamp(-1.0, 1.0),
            skin_saturation: (self.skin_saturation / 100.0).clamp(-1.0, 1.0),
            skin_brightness: (self.skin_brightness / 100.0).clamp(-1.0, 1.0),
            _pad1: [0.0; 3],
        }
    }
}

/// GPU uniform buffer for the fused grade pass.
///
/// `#[repr(C)]` with explicit padding: 44 x `f32` = **176 bytes** total.
/// Byte offsets are documented on each field; `sf-render` uploads this with
/// `bytemuck::bytes_of` and the WGSL struct must declare the fields in the
/// same order.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GradeUniform {
    pub exposure_ev: f32, // 0
    /// 4 — 0.0 = gamma 2.4, 1.0 = sRGB (MVP: always 0.0).
    pub transfer: f32,
    /// 8 — 0.0/1.0 (MVP: always 0.0, full range).
    pub use_limited_range: f32,
    /// 12 — -1..1.
    pub brightness: f32,
    pub wb_gain: [f32; 3], // 16
    pub _pad0: f32,        // 28
    pub lift: [f32; 4],    // 32
    pub gain: [f32; 4],    // 48
    pub gamma_w: [f32; 4], // 64
    pub offset: [f32; 4],  // 80
    /// 96 — -1..1.
    pub contrast: f32,
    /// 100 — 0..2.
    pub saturation: f32,
    /// 104 — -1..1.
    pub vibrance: f32,
    /// 108 — -1..1.
    pub highlights: f32,
    /// 112 — -1..1.
    pub shadows: f32,
    /// 116 — -1..1.
    pub whites: f32,
    /// 120 — -1..1.
    pub blacks: f32,
    pub basic_gamma: f32, // 124
    /// 128 — 0..1.
    pub sharpness: f32,
    /// 132 — 0..1.
    pub lut_intensity: f32,
    /// 136 — bit 0 = LUT enabled, bit 1 = LUT before grade.
    pub lut_flags: f32,
    /// 140 — 0.0/1.0.
    pub skin_protect: f32,
    /// 144 — 0.0/1.0.
    pub curves_enabled: f32,
    /// 148 — 0..1.
    pub faded_film: f32,
    /// 152 — -1..1.
    pub skin_warmth: f32,
    /// 156 — -1..1.
    pub skin_saturation: f32,
    /// 160 — -1..1.
    pub skin_brightness: f32,
    pub _pad1: [f32; 3], // 164 — total struct size 176 bytes.
}

impl Default for GradeUniform {
    fn default() -> Self {
        ColorGrade::default().to_uniform()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_is_176_bytes() {
        assert_eq!(std::mem::size_of::<GradeUniform>(), 176);
        let u = ColorGrade::default().to_uniform();
        assert_eq!(bytemuck::bytes_of(&u).len(), 176);
        assert_eq!(std::mem::size_of::<GradeUniform>() % 16, 0);
        // Spot-check the documented byte offsets.
        let base = std::ptr::addr_of!(u) as usize;
        assert_eq!(std::ptr::addr_of!(u.exposure_ev) as usize - base, 0);
        assert_eq!(std::ptr::addr_of!(u.wb_gain) as usize - base, 16);
        assert_eq!(std::ptr::addr_of!(u.lift) as usize - base, 32);
        assert_eq!(std::ptr::addr_of!(u.contrast) as usize - base, 96);
        assert_eq!(std::ptr::addr_of!(u.sharpness) as usize - base, 128);
        assert_eq!(std::ptr::addr_of!(u.lut_flags) as usize - base, 136);
        assert_eq!(std::ptr::addr_of!(u.curves_enabled) as usize - base, 144);
        assert_eq!(std::ptr::addr_of!(u.faded_film) as usize - base, 148);
        assert_eq!(std::ptr::addr_of!(u.skin_brightness) as usize - base, 160);
    }

    #[test]
    fn default_uniform_values() {
        let u = ColorGrade::default().to_uniform();
        assert_eq!(u.exposure_ev, 0.0);
        assert_eq!(u.transfer, 0.0);
        assert_eq!(u.use_limited_range, 0.0);
        assert_eq!(u.brightness, 0.0);
        assert_eq!(u.wb_gain, [1.0, 1.0, 1.0]);
        assert_eq!(u.lift, [0.0; 4]);
        assert_eq!(u.gain, [1.0; 4]);
        assert_eq!(u.gamma_w, [1.0; 4]);
        assert_eq!(u.offset, [0.0; 4]);
        assert_eq!(u.contrast, 0.0);
        assert_eq!(u.saturation, 1.0);
        assert_eq!(u.vibrance, 0.0);
        assert_eq!(u.highlights, 0.0);
        assert_eq!(u.shadows, 0.0);
        assert_eq!(u.whites, 0.0);
        assert_eq!(u.blacks, 0.0);
        assert_eq!(u.basic_gamma, 1.0);
        assert_eq!(u.sharpness, 0.0);
        assert_eq!(u.lut_intensity, 0.0);
        assert_eq!(u.lut_flags, 0.0);
        assert_eq!(u.skin_protect, 0.0);
        assert_eq!(u.curves_enabled, 0.0);
    }

    #[test]
    fn to_uniform_normalizes_ui_units() {
        let mut g = ColorGrade::default();
        g.exposure = 1.5;
        g.contrast = 25.0;
        g.saturation = 140.0;
        g.vibrance = -30.0;
        g.sharpness = 50.0;
        g.skin_protect = true;
        let u = g.to_uniform();
        assert_eq!(u.exposure_ev, 1.5);
        assert_eq!(u.contrast, 0.25);
        assert_eq!(u.saturation, 1.4);
        assert_eq!(u.vibrance, -0.3);
        assert_eq!(u.sharpness, 0.5);
        assert_eq!(u.skin_protect, 1.0);
    }

    #[test]
    fn lut_flags_encode_enabled_and_position() {
        let mut g = ColorGrade::default();
        g.lut = Some(LutRef {
            file: "color/luts/warm.cube".to_string(),
            intensity: 0.6,
            position: LutPosition::Before,
        });
        let u = g.to_uniform();
        assert_eq!(u.lut_flags, 3.0); // bit0 enabled | bit1 before
        assert_eq!(u.lut_intensity, 0.6);

        let mut g = ColorGrade::default();
        g.lut = Some(LutRef {
            file: "color/luts/warm.cube".to_string(),
            intensity: 0.6,
            position: LutPosition::After,
        });
        assert_eq!(g.to_uniform().lut_flags, 1.0);

        // Zero intensity disables the LUT even when present.
        let mut g = ColorGrade::default();
        g.lut = Some(LutRef {
            file: "color/luts/warm.cube".to_string(),
            intensity: 0.0,
            position: LutPosition::Before,
        });
        assert_eq!(g.to_uniform().lut_flags, 2.0);
    }

    #[test]
    fn gamma_wheel_clamped_against_div_by_zero() {
        let mut g = ColorGrade::default();
        g.gamma_wheel = [0.0, -2.0, 0.5, 1.0];
        let u = g.to_uniform();
        assert_eq!(u.gamma_w[0], 0.01);
        assert_eq!(u.gamma_w[1], 0.01);
        assert_eq!(u.gamma_w[2], 0.5);
        assert_eq!(u.gamma_w[3], 1.0);
    }

    #[test]
    fn identity_detection() {
        assert!(ColorGrade::default().is_identity());
        let mut g = ColorGrade::default();
        g.exposure = 0.5;
        assert!(!g.is_identity());
        // skin_protect alone changes nothing.
        let mut g = ColorGrade::default();
        g.skin_protect = true;
        assert!(g.is_identity());
        // Any LUT breaks identity.
        let mut g = ColorGrade::default();
        g.lut = Some(LutRef {
            file: "x.cube".to_string(),
            intensity: 0.0,
            position: LutPosition::After,
        });
        assert!(!g.is_identity());
        // Non-identity curves break identity.
        let mut g = ColorGrade::default();
        g.curves
            .master
            .set_points(vec![[0.0, 0.0], [1.0, 0.9]])
            .unwrap();
        assert!(!g.is_identity());
        assert_eq!(g.to_uniform().curves_enabled, 1.0);
    }

    /// Helper: an "expanded look" shaped like `apply_look(preset, 1.0)` —
    /// full deltas from identity on every composition rule.
    fn look_delta() -> ColorGrade {
        ColorGrade {
            exposure: 0.5,
            brightness: 10.0,
            contrast: -5.0,
            saturation: 140.0,
            vibrance: 3.0,
            temperature: 6000.0,
            tint: -2.0,
            highlights: 7.0,
            shadows: -8.0,
            whites: 4.0,
            blacks: -6.0,
            gamma: 1.2,
            sharpness: 20.0,
            lift: [0.01, 0.02, 0.03, 0.0],
            gamma_wheel: [1.1, 0.9, 1.0, 1.0],
            gain: [1.05, 1.0, 0.95, 1.0],
            offset: [0.0, 0.01, -0.01, 0.0],
            curves: CurveSet::default(),
            lut: None,
            skin_protect: true,
            faded_film: 12.0,
            skin_warmth: 5.0,
            skin_saturation: -4.0,
            skin_brightness: 3.0,
        }
    }

    #[test]
    fn layer_look_onto_identity_reproduces_look() {
        let mut base = ColorGrade::default();
        let look = look_delta();
        base.layer_look(&look);
        assert_eq!(base, look, "identity + full look delta must equal the look");
    }

    #[test]
    fn layer_look_delta_composition_rules() {
        // Base carries its own deltas; the look's deltas fold on top.
        let mut base = ColorGrade {
            exposure: 0.25,
            saturation: 110.0,
            temperature: 7000.0,
            gamma: 0.9,
            gain: [1.1, 1.0, 1.0, 1.0],
            lift: [0.05, 0.0, 0.0, 0.0],
            ..Default::default()
        };
        base.layer_look(&look_delta());
        // Additive around 0.
        assert_eq!(base.exposure, 0.75);
        // saturation: 110 + (140 - 100).
        assert_eq!(base.saturation, 150.0);
        // temperature: 7000 + (6000 - 6500).
        assert_eq!(base.temperature, 6500.0);
        // gamma: 0.9 * 1.2.
        assert!((base.gamma - 1.08).abs() < 1e-6);
        // lift additive, gain multiplicative.
        assert!((base.lift[0] - 0.06).abs() < 1e-6);
        assert!((base.gain[0] - 1.155).abs() < 1e-6);
        // skin_protect OR.
        assert!(base.skin_protect);
    }

    #[test]
    fn layer_look_curves_and_lut_last_wins() {
        let mut curved = ColorGrade::default();
        curved
            .curves
            .master
            .set_points(vec![[0.0, 0.0], [1.0, 0.8]])
            .unwrap();
        let mut base = curved.clone();

        // An identity-curves look leaves the base curves untouched.
        base.layer_look(&ColorGrade::default());
        assert_eq!(base.curves, curved.curves);

        // A look with curves replaces them (last wins).
        let mut with_curves = ColorGrade::default();
        with_curves
            .curves
            .master
            .set_points(vec![[0.0, 0.0], [1.0, 0.6]])
            .unwrap();
        base.layer_look(&with_curves);
        assert_eq!(base.curves, with_curves.curves);

        // LUT: None keeps base, Some replaces (last wins).
        let mut base2 = ColorGrade::default();
        base2.lut = Some(LutRef {
            file: "a.cube".to_string(),
            intensity: 0.5,
            position: LutPosition::After,
        });
        base2.layer_look(&ColorGrade::default());
        assert_eq!(base2.lut.as_ref().unwrap().file, "a.cube");
        let mut with_lut = ColorGrade::default();
        with_lut.lut = Some(LutRef {
            file: "b.cube".to_string(),
            intensity: 1.0,
            position: LutPosition::Before,
        });
        base2.layer_look(&with_lut);
        let lut = base2.lut.as_ref().unwrap();
        assert_eq!(lut.file, "b.cube");
        assert_eq!(lut.position, LutPosition::Before);
    }

    #[test]
    fn layer_look_skin_protect_or() {
        let mut base = ColorGrade::default();
        base.skin_protect = true;
        // An OR: a look with skin_protect=false must not clear the base.
        let look = look_delta(); // skin_protect = true here
        base.layer_look(&look);
        assert!(base.skin_protect);
        let mut plain = ColorGrade::default();
        plain.skin_protect = true;
        let mut no_skin = ColorGrade::default();
        no_skin.skin_protect = false;
        plain.layer_look(&no_skin);
        assert!(plain.skin_protect, "OR must not clear an existing true");
        // And false | true = true.
        let mut off = ColorGrade::default();
        off.layer_look(&look);
        assert!(off.skin_protect);
    }
}
