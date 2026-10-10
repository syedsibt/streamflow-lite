//! `sf-filters` — look presets, effect filters, and filter stacks.
//!
//! Two kinds of filters:
//!
//! * **Look filters** ([`Filter::Look`]) are color-only: a named [`LookPreset`]
//!   (parameter set + optional LUT) loaded from `.sflook` JSON files, with an
//!   intensity slider that scales the look toward the identity grade via
//!   [`apply_look`]. Built-in looks live in `assets/looks/` and are indexed by
//!   [`LookRegistry`].
//! * **Effect filters** ([`Filter::Effect`]) are spatial/temporal operations
//!   ([`EffectKind`] + [`EffectParams`]); the C5 WGSL implementations live in
//!   `sf-filterfx`, with an estimated GPU cost badge ([`EffectKind::cost`])
//!   for the UI.
//!
//! Filters are ordered in a [`FilterStack`] (input, scene, or program level)
//! with at most [`FilterStack::MAX_SLOTS`] slots and at most
//! [`FilterStack::MAX_LOOKS`] look filters per stack.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sf_color::{ColorGrade, CurveSet, LutPosition, LutRef};
use uuid::Uuid;

/// Category of a look preset, shown in the looks strip of the Color Studio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LookCategory {
    /// Clean, neutral corrections.
    #[serde(rename = "Natural")]
    Natural,
    /// Filmic color with lifted blacks / stylized palettes.
    #[serde(rename = "Cinematic")]
    Cinematic,
    /// Punchy, camera-ready styles for talking-head / streaming content.
    #[serde(rename = "Creator")]
    Creator,
    /// Aged, faded, analog-inspired styles.
    #[serde(rename = "Vintage")]
    Vintage,
    /// Monochrome styles (skin protection is off: there is no color to protect).
    #[serde(rename = "B&W")]
    BlackAndWhite,
    /// Styles tuned for weddings, services, conferences, concerts.
    #[serde(rename = "Event")]
    Event,
    /// High-saturation, high-sharpness styles for screen/game capture.
    #[serde(rename = "Gaming/Screen")]
    Gaming,
    /// Corrective grades (exposure/contrast rescue).
    #[serde(rename = "Fix")]
    Fix,
}

/// LUT reference inside a look preset (`.sflook`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LookLut {
    /// Relative path to the `.cube` file.
    pub file: String,
    /// Full-strength blend amount `0.0..=1.0` (scaled by look intensity).
    pub intensity: f32,
}

fn default_true() -> bool {
    true
}

/// A look preset: a named, categorized parameter set plus an optional LUT.
///
/// This is the deserialized form of a `.sflook` JSON file; the registry key
/// (preset id) is the file stem, not stored in the struct.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LookPreset {
    /// Display name, e.g. `"Podcast Warm"`.
    pub name: String,
    /// Preset format version (currently `1`).
    pub version: u32,
    /// Category shown in the looks strip.
    pub category: LookCategory,
    /// Grade parameters in UI units; only non-default fields are stored.
    #[serde(default)]
    pub params: ColorGrade,
    /// Optional LUT applied after the grade.
    #[serde(default)]
    pub lut: Option<LookLut>,
    /// Skin-tone protection; false for B&W looks (no color to protect).
    #[serde(default = "default_true")]
    pub skin_protect: bool,
}

/// Estimated GPU cost badge for an effect filter, shown in the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuCost {
    /// Cheap per-pixel work (single pass, no temporal state).
    Low,
    /// Multi-tap / multi-pass work.
    Medium,
    /// AI inference or temporal accumulation.
    High,
}

/// Effect (spatial/temporal) filter kinds.
///
/// The C5 set is enumerated here; GPU implementations live in `sf-filterfx`
/// (group A: vignette, film grain, letterbox, mirror flip; the rest arrive
/// in later C5 work). Use [`EffectKind::cost`] for the UI cost badge and
/// [`EffectKind::default_params`] for a slot's starting settings.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffectKind {
    /// Unsharp-mask sharpening.
    Sharpen,
    /// Gaussian blur.
    GaussianBlur,
    /// AI background blur (segmentation + blur).
    BackgroundBlur,
    /// AI background replacement (segmentation + plate).
    BackgroundReplace,
    /// Chroma key with spill suppression.
    ChromaKey,
    /// Vignette.
    Vignette,
    /// Animated film grain.
    FilmGrain,
    /// Bloom / glow.
    Bloom,
    /// Temporal noise reduction.
    NoiseReduction,
    /// AI skin smoothing.
    SkinSmooth,
    /// Horizontal/vertical mirror flip.
    MirrorFlip,
    /// Digital pan-tilt-zoom (crop + scale).
    CropZoomPan,
    /// Lens distortion correction.
    LensCorrection,
    /// Cinematic letterbox bars.
    Letterbox,
    /// Chromatic aberration.
    ChromaticAberration,
    /// Pixelate / mosaic.
    Pixelate,
}

impl EffectKind {
    /// Estimated GPU cost badge for this effect (spec §5.2).
    ///
    /// * **Low** — single-pass, few taps, no state: sharpen, vignette, film
    ///   grain, mirror/flip, crop/zoom/pan, letterbox, chromatic aberration,
    ///   pixelate.
    /// * **Medium** — multi-tap or multi-pass: gaussian blur, chroma key,
    ///   bloom, lens correction.
    /// * **High** — AI inference or temporal state: background blur, background
    ///   replace, noise reduction, skin smooth.
    pub fn cost(&self) -> GpuCost {
        match self {
            EffectKind::Sharpen
            | EffectKind::Vignette
            | EffectKind::FilmGrain
            | EffectKind::MirrorFlip
            | EffectKind::CropZoomPan
            | EffectKind::Letterbox
            | EffectKind::ChromaticAberration
            | EffectKind::Pixelate => GpuCost::Low,
            EffectKind::GaussianBlur
            | EffectKind::ChromaKey
            | EffectKind::Bloom
            | EffectKind::LensCorrection => GpuCost::Medium,
            EffectKind::BackgroundBlur
            | EffectKind::BackgroundReplace
            | EffectKind::NoiseReduction
            | EffectKind::SkinSmooth => GpuCost::High,
        }
    }
}

/// Mirror/flip axis for [`EffectKind::MirrorFlip`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlipAxis {
    /// Flip left-right.
    Horizontal,
    /// Flip top-bottom.
    Vertical,
    /// Flip both (equivalent to a 180° rotation).
    Both,
}

/// Per-slot effect parameters (C5).
///
/// One struct carries the settings for every [`EffectKind`]; each kind only
/// reads the fields it needs (see [`EffectKind::default_params`] for the
/// per-kind defaults). All channels are in linear working space; `center` is
/// normalized `0..1` (origin top-left).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectParams {
    /// Generic strength `0..1` (multiplied by the slot intensity).
    pub amount: f32,
    /// Radius in px-ish units (blur, bloom, chromatic aberration, pixelate).
    pub radius: f32,
    /// Edge softness `0..1` (vignette falloff, key edges).
    pub softness: f32,
    /// Effect center, normalized `0..1`.
    pub center: [f32; 2],
    /// Linear-space key color (chroma key).
    pub color: [f32; 3],
    /// Luma threshold `0..1` (bloom).
    pub threshold: f32,
    /// Crop zoom `>= 1` (crop/zoom/pan).
    pub zoom: f32,
    /// Letterbox target aspect ratio.
    pub aspect: f32,
    /// Lens distortion coefficients.
    pub k1: f32,
    /// Lens distortion coefficients.
    pub k2: f32,
    /// Mirror axis.
    pub axis: FlipAxis,
    /// Chroma-key despill `0..1`.
    pub spill: f32,
    /// Chroma-key tolerance `0..1`.
    pub tolerance: f32,
    /// Chroma-key edge softness `0..1`.
    pub key_softness: f32,
}

impl EffectKind {
    /// Sensible starting parameters for this effect kind.
    ///
    /// Shared baseline: `amount = 1.0`, `softness = 0.5`, `center =
    /// [0.5, 0.5]`, `color = [0, 0.5, 0]` (green), `threshold = 0.8`, `zoom =
    /// 1.5`, `aspect = 2.39`, `k1 = k2 = 0`, `axis = Horizontal`, `spill =
    /// 0.5`, `tolerance = 0.15`, `key_softness = 0.2`. Only `radius` varies
    /// per kind: blur 4.0, bloom 6.0, chromatic aberration 8.0, pixelate 16.0.
    pub fn default_params(&self) -> EffectParams {
        let mut p = EffectParams {
            amount: 1.0,
            radius: 0.0,
            softness: 0.5,
            center: [0.5, 0.5],
            color: [0.0, 0.5, 0.0],
            threshold: 0.8,
            zoom: 1.5,
            aspect: 2.39,
            k1: 0.0,
            k2: 0.0,
            axis: FlipAxis::Horizontal,
            spill: 0.5,
            tolerance: 0.15,
            key_softness: 0.2,
        };
        p.radius = match self {
            EffectKind::GaussianBlur | EffectKind::BackgroundBlur => 4.0,
            EffectKind::Bloom => 6.0,
            EffectKind::ChromaticAberration => 8.0,
            EffectKind::Pixelate => 16.0,
            _ => 0.0,
        };
        p
    }

    /// True for the AI-backed effects (implemented in C6/C7 via
    /// `sf-ai-runtime`, not as WGSL compute): background blur, background
    /// replacement, and skin smoothing.
    pub fn is_ai(&self) -> bool {
        matches!(
            self,
            EffectKind::BackgroundBlur | EffectKind::BackgroundReplace | EffectKind::SkinSmooth
        )
    }
}

/// A filter: either a look preset reference or an effect kind + parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Filter {
    /// Look filter: `preset_id` is the `.sflook` file stem, `intensity`
    /// `0.0..=1.0` scales the look toward identity.
    Look {
        /// Registry id of the look preset (`.sflook` file stem).
        preset_id: String,
        /// Look strength `0.0..=1.0`.
        intensity: f32,
    },
    /// Effect filter: the kind plus its [`EffectParams`]. Applied by
    /// `sf-filterfx` in stack order; AI kinds ([`EffectKind::is_ai`]) are
    /// skipped until C6/C7.
    Effect {
        /// Which effect to apply.
        kind: EffectKind,
        /// Per-kind settings; start from [`EffectKind::default_params`].
        params: EffectParams,
    },
}

/// Region mask for a filter slot.
///
/// C7 evaluates these for effect slots in the engine: `Circle`/`Rectangle`
/// masks are generated analytically per frame, `Person` reuses the cached
/// AI person mask texture. Look slots with masks are out of C7 scope
/// (grade-level masking is a documented TODO in the engine).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MaskRef {
    /// Circular mask: center `(x, y)` in `0..=1`, `radius` in `0..=1`,
    /// `feather` in `0..=1`.
    Circle {
        /// Center x in `0..=1`.
        x: f32,
        /// Center y in `0..=1`.
        y: f32,
        /// Radius in `0..=1` (fraction of frame height).
        radius: f32,
        /// Edge feather in `0..=1` (fraction of the radius used as the
        /// smooth falloff band; `0.0` is a hard edge).
        feather: f32,
    },
    /// Rectangular mask: top-left `(x, y)`, size `(w, h)`, all normalized
    /// `0..=1`; `feather` in `0..=1`.
    Rectangle {
        /// Left edge in `0..=1`.
        x: f32,
        /// Top edge in `0..=1`.
        y: f32,
        /// Width in `0..=1`.
        w: f32,
        /// Height in `0..=1`.
        h: f32,
        /// Edge feather in `0..=1` (fraction of the smaller side used as
        /// the smooth falloff band; `0.0` is a hard edge).
        feather: f32,
    },
    /// AI person mask (requires `sf-ai-runtime`).
    Person,
}

/// One filter instance in a [`FilterStack`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilterSlot {
    /// Stable slot id (used by IPC reorder/intensity/toggle commands).
    pub id: Uuid,
    /// The filter itself.
    pub filter: Filter,
    /// Whether the slot is currently applied.
    pub enabled: bool,
    /// Slot strength `0.0..=1.0` (multiplies the filter's own intensity).
    pub intensity: f32,
    /// Optional region mask (v2; evaluated for effect slots in C7 —
    /// grade-level masking for look slots is still a TODO).
    pub mask: Option<MaskRef>,
}

/// Errors from [`FilterStack`] mutations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StackError {
    /// Pushing would exceed [`FilterStack::MAX_SLOTS`].
    #[error("stack full: max {max} filters")]
    TooManySlots {
        /// The configured slot limit.
        max: usize,
    },
    /// Pushing would exceed [`FilterStack::MAX_LOOKS`] look filters.
    #[error("max {max} look filters per stack")]
    TooManyLooks {
        /// The configured look limit.
        max: usize,
    },
    /// No slot with that id exists in the stack.
    #[error("slot not found")]
    SlotNotFound,
    /// A slot with that id is already in the stack.
    #[error("duplicate slot id")]
    DuplicateSlot,
}

/// An ordered, bounded list of filter slots (input, scene, or program level).
///
/// Rules: at most 8 slots; at most 2 look filters; each slot has
/// enable/disable + intensity; order is drag-reorderable.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FilterStack {
    slots: Vec<FilterSlot>,
}

impl FilterStack {
    /// Maximum number of filter slots per stack (lite limit).
    pub const MAX_SLOTS: usize = 8;
    /// Maximum number of look filters per stack.
    pub const MAX_LOOKS: usize = 2;

    /// Creates an empty filter stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a slot, enforcing the slot, look, and uniqueness limits.
    pub fn push(&mut self, slot: FilterSlot) -> Result<(), StackError> {
        if self.slots.iter().any(|s| s.id == slot.id) {
            return Err(StackError::DuplicateSlot);
        }
        if self.slots.len() >= Self::MAX_SLOTS {
            return Err(StackError::TooManySlots {
                max: Self::MAX_SLOTS,
            });
        }
        if matches!(slot.filter, Filter::Look { .. }) && self.look_count() >= Self::MAX_LOOKS {
            return Err(StackError::TooManyLooks {
                max: Self::MAX_LOOKS,
            });
        }
        self.slots.push(slot);
        Ok(())
    }

    /// Removes the slot with `id`; returns true if a slot was removed.
    pub fn remove(&mut self, id: &Uuid) -> bool {
        let before = self.slots.len();
        self.slots.retain(|s| &s.id != id);
        self.slots.len() != before
    }

    /// Reorders slots to match `order`, which must contain exactly the current
    /// slot ids (same set, no duplicates).
    pub fn reorder(&mut self, order: &[Uuid]) -> Result<(), StackError> {
        if order.len() != self.slots.len() {
            return Err(StackError::SlotNotFound);
        }
        let mut seen = std::collections::HashSet::new();
        for id in order {
            if !seen.insert(id) || !self.slots.iter().any(|s| &s.id == id) {
                return Err(StackError::SlotNotFound);
            }
        }
        let mut reordered = Vec::with_capacity(order.len());
        for id in order {
            let pos = self.slots.iter().position(|s| &s.id == id).unwrap();
            reordered.push(self.slots[pos].clone());
        }
        self.slots = reordered;
        Ok(())
    }

    /// Sets a slot's intensity, clamped to `0.0..=1.0`.
    pub fn set_intensity(&mut self, id: &Uuid, v: f32) -> Result<(), StackError> {
        match self.slots.iter_mut().find(|s| &s.id == id) {
            Some(slot) => {
                slot.intensity = v.clamp(0.0, 1.0);
                Ok(())
            }
            None => Err(StackError::SlotNotFound),
        }
    }

    /// Enables or disables a slot.
    pub fn toggle(&mut self, id: &Uuid, enabled: bool) -> Result<(), StackError> {
        match self.slots.iter_mut().find(|s| &s.id == id) {
            Some(slot) => {
                slot.enabled = enabled;
                Ok(())
            }
            None => Err(StackError::SlotNotFound),
        }
    }

    /// Returns the slots in application order.
    pub fn slots(&self) -> &[FilterSlot] {
        &self.slots
    }

    /// Number of look filters currently in the stack.
    pub fn look_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s.filter, Filter::Look { .. }))
            .count()
    }
}

/// Errors from [`LookRegistry::load_dir`].
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The looks directory could not be read.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A `.sflook` file failed to parse.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Index of built-in (and user) look presets, keyed by preset id.
///
/// The preset id is the `.sflook` file stem (e.g. `studio-clean` for
/// `studio-clean.sflook`).
#[derive(Debug, Default)]
pub struct LookRegistry {
    looks: HashMap<String, LookPreset>,
}

impl LookRegistry {
    /// Loads every `*.sflook` file in `dir`; the id is the file stem.
    ///
    /// Files are processed in sorted order so repeated loads are deterministic.
    pub fn load_dir(dir: &Path) -> Result<Self, RegistryError> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        let mut looks = HashMap::new();
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sflook") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let text = std::fs::read_to_string(&path)?;
            let preset: LookPreset = serde_json::from_str(&text)?;
            looks.insert(stem.to_string(), preset);
        }
        Ok(Self { looks })
    }

    /// Returns the preset for `id`, if present.
    pub fn get(&self, id: &str) -> Option<&LookPreset> {
        self.looks.get(id)
    }

    /// All preset ids, sorted.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.looks.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Number of registered presets.
    pub fn len(&self) -> usize {
        self.looks.len()
    }

    /// True when no presets are registered.
    pub fn is_empty(&self) -> bool {
        self.looks.is_empty()
    }
}

/// Expands a look preset into an editable [`ColorGrade`], scaled by `intensity`.
///
/// `intensity` is clamped to `0..=1`: `0.0` yields [`ColorGrade::default`]
/// (except `skin_protect`, which always comes from the preset), `1.0` yields
/// the full preset. Blending rules:
///
/// * Scalars (`exposure`, `brightness`, `contrast`, `saturation`, `vibrance`,
///   `tint`, `highlights`, `shadows`, `whites`, `blacks`, `gamma`,
///   `sharpness`): `default + (v − default) × i`.
/// * `temperature`: `6500 + (t − 6500) × i`.
/// * Wheels: `lift`/`offset` scale as `v × i`; `gain`/`gamma_wheel` as
///   `1 + (v − 1) × i` (element-wise).
/// * Curves: the preset curves when `i > 0`, otherwise the default curves.
/// * LUT: file kept, `intensity = preset_intensity × i`, position
///   [`LutPosition::After`]; `None` when the preset has no LUT or `i == 0`
///   (a zero-intensity LUT is a bypass).
/// * `skin_protect`: always `preset.skin_protect`.
pub fn apply_look(preset: &LookPreset, intensity: f32) -> ColorGrade {
    let i = intensity.clamp(0.0, 1.0);
    let p = &preset.params;
    let d = ColorGrade::default();

    let blend = |v: f32, default: f32| default + (v - default) * i;
    let blend_wheel_add = |v: [f32; 4]| [v[0] * i, v[1] * i, v[2] * i, v[3] * i];
    let blend_wheel_mul = |v: [f32; 4]| {
        [
            1.0 + (v[0] - 1.0) * i,
            1.0 + (v[1] - 1.0) * i,
            1.0 + (v[2] - 1.0) * i,
            1.0 + (v[3] - 1.0) * i,
        ]
    };

    ColorGrade {
        exposure: blend(p.exposure, d.exposure),
        brightness: blend(p.brightness, d.brightness),
        contrast: blend(p.contrast, d.contrast),
        saturation: blend(p.saturation, d.saturation),
        vibrance: blend(p.vibrance, d.vibrance),
        temperature: 6500.0 + (p.temperature - 6500.0) * i,
        tint: blend(p.tint, d.tint),
        highlights: blend(p.highlights, d.highlights),
        shadows: blend(p.shadows, d.shadows),
        whites: blend(p.whites, d.whites),
        blacks: blend(p.blacks, d.blacks),
        gamma: blend(p.gamma, d.gamma),
        sharpness: blend(p.sharpness, d.sharpness),
        lift: blend_wheel_add(p.lift),
        gamma_wheel: blend_wheel_mul(p.gamma_wheel),
        gain: blend_wheel_mul(p.gain),
        offset: blend_wheel_add(p.offset),
        curves: if i > 0.0 {
            p.curves.clone()
        } else {
            CurveSet::default()
        },
        lut: match &preset.lut {
            Some(l) if i > 0.0 => Some(LutRef {
                file: l.file.clone(),
                intensity: l.intensity * i,
                position: LutPosition::After,
            }),
            _ => None,
        },
        skin_protect: preset.skin_protect,
    }
}

/// Folds an input's [`FilterStack`] look filters into a base grade.
///
/// Clones `base`, then walks the stack in order: every slot that is
/// `enabled` and holds a [`Filter::Look`] expands via
/// [`apply_look`](preset, filter_intensity × slot_intensity) and is layered
/// onto the accumulator with
/// [`ColorGrade::layer_look`](sf_color::ColorGrade::layer_look). Unknown
/// preset ids are skipped silently (a look file may be missing without
/// breaking the grade); disabled looks and non-look filters are ignored.
///
/// The engine calls this per frame; the returned grade is what the fused
/// grade pass actually applies, so a look in the stack and the same look
/// baked into the base grade produce identical pixels.
pub fn resolve_grade(
    base: &ColorGrade,
    stack: &FilterStack,
    registry: &LookRegistry,
) -> ColorGrade {
    let mut acc = base.clone();
    for slot in stack.slots() {
        if !slot.enabled {
            continue;
        }
        let Filter::Look {
            preset_id,
            intensity,
        } = &slot.filter
        else {
            continue;
        };
        let Some(preset) = registry.get(preset_id) else {
            continue; // missing preset: skip silently
        };
        let expanded = apply_look(preset, intensity * slot.intensity);
        acc.layer_look(&expanded);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Directory of the built-in looks, relative to this crate.
    fn looks_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/looks")
    }

    fn effect_slot(kind: EffectKind) -> FilterSlot {
        let params = kind.default_params();
        FilterSlot {
            id: Uuid::new_v4(),
            filter: Filter::Effect { kind, params },
            enabled: true,
            intensity: 1.0,
            mask: None,
        }
    }

    fn look_slot(preset_id: &str) -> FilterSlot {
        FilterSlot {
            id: Uuid::new_v4(),
            filter: Filter::Look {
                preset_id: preset_id.to_string(),
                intensity: 1.0,
            },
            enabled: true,
            intensity: 1.0,
            mask: None,
        }
    }

    #[test]
    fn push_ninth_slot_fails() {
        let mut stack = FilterStack::new();
        for _ in 0..FilterStack::MAX_SLOTS {
            stack.push(effect_slot(EffectKind::Sharpen)).unwrap();
        }
        let err = stack.push(effect_slot(EffectKind::Vignette)).unwrap_err();
        assert_eq!(err, StackError::TooManySlots { max: 8 });
    }

    #[test]
    fn third_look_fails() {
        let mut stack = FilterStack::new();
        stack.push(look_slot("teal-orange")).unwrap();
        stack.push(look_slot("film-warm")).unwrap();
        let err = stack.push(look_slot("noir")).unwrap_err();
        assert_eq!(err, StackError::TooManyLooks { max: 2 });
        assert_eq!(stack.look_count(), 2);
    }

    #[test]
    fn one_look_plus_five_effects_ok() {
        let mut stack = FilterStack::new();
        stack.push(look_slot("podcast-warm")).unwrap();
        for kind in [
            EffectKind::Sharpen,
            EffectKind::Vignette,
            EffectKind::FilmGrain,
            EffectKind::Letterbox,
            EffectKind::Pixelate,
        ] {
            stack.push(effect_slot(kind)).unwrap();
        }
        assert_eq!(stack.slots().len(), 6);
        assert_eq!(stack.look_count(), 1);
    }

    #[test]
    fn duplicate_slot_id_fails() {
        let mut stack = FilterStack::new();
        let slot = effect_slot(EffectKind::Sharpen);
        let id = slot.id;
        stack.push(slot).unwrap();
        let dup = FilterSlot {
            id,
            filter: Filter::Effect {
                kind: EffectKind::Vignette,
                params: EffectKind::Vignette.default_params(),
            },
            enabled: true,
            intensity: 1.0,
            mask: None,
        };
        assert_eq!(stack.push(dup).unwrap_err(), StackError::DuplicateSlot);
    }

    #[test]
    fn reorder_remove_intensity_toggle() {
        let mut stack = FilterStack::new();
        let a = effect_slot(EffectKind::Sharpen);
        let b = effect_slot(EffectKind::Vignette);
        let (ida, idb) = (a.id, b.id);
        stack.push(a).unwrap();
        stack.push(b).unwrap();

        stack.reorder(&[idb, ida]).unwrap();
        assert_eq!(stack.slots()[0].id, idb);
        assert_eq!(stack.slots()[1].id, ida);

        // Wrong id set must fail.
        assert_eq!(stack.reorder(&[idb]).unwrap_err(), StackError::SlotNotFound);
        // Duplicate in order must fail.
        assert_eq!(
            stack.reorder(&[idb, idb]).unwrap_err(),
            StackError::SlotNotFound
        );

        stack.set_intensity(&ida, 2.5).unwrap();
        assert_eq!(stack.slots()[1].intensity, 1.0); // clamped
        stack.set_intensity(&ida, -0.5).unwrap();
        assert_eq!(stack.slots()[1].intensity, 0.0); // clamped
        assert_eq!(
            stack.set_intensity(&Uuid::new_v4(), 0.5).unwrap_err(),
            StackError::SlotNotFound
        );

        stack.toggle(&ida, false).unwrap();
        assert!(!stack.slots()[1].enabled);
        assert_eq!(
            stack.toggle(&Uuid::new_v4(), true).unwrap_err(),
            StackError::SlotNotFound
        );

        assert!(stack.remove(&ida));
        assert!(!stack.remove(&ida));
        assert_eq!(stack.slots().len(), 1);
    }

    #[test]
    fn registry_loads_30_builtin_looks() {
        let registry = LookRegistry::load_dir(&looks_dir()).unwrap();
        assert_eq!(registry.len(), 30);
        assert!(!registry.is_empty());
        // Spot-check a few ids and categories.
        let teal = registry.get("teal-orange").unwrap();
        assert_eq!(teal.name, "Teal Orange");
        assert_eq!(teal.category, LookCategory::Cinematic);
        let bw = registry.get("noir").unwrap();
        assert_eq!(bw.category, LookCategory::BlackAndWhite);
        assert!(!bw.skin_protect);
        let podcast = registry.get("podcast-warm").unwrap();
        assert!(podcast.skin_protect);
        assert!(podcast.lut.is_some());
    }

    #[test]
    fn all_looks_roundtrip_json() {
        let registry = LookRegistry::load_dir(&looks_dir()).unwrap();
        assert_eq!(registry.len(), 30);
        for id in registry.ids() {
            let preset = registry.get(&id).unwrap();
            let json = serde_json::to_string(preset).unwrap();
            let back: LookPreset = serde_json::from_str(&json).unwrap();
            assert_eq!(back.name, preset.name, "name mismatch for {id}");
            assert_eq!(back.category, preset.category, "category mismatch for {id}");
            assert_eq!(
                back.skin_protect, preset.skin_protect,
                "skin_protect mismatch for {id}"
            );
        }
    }

    #[test]
    fn look_category_serde_shapes() {
        assert_eq!(
            serde_json::to_string(&LookCategory::BlackAndWhite).unwrap(),
            "\"B&W\""
        );
        assert_eq!(
            serde_json::to_string(&LookCategory::Gaming).unwrap(),
            "\"Gaming/Screen\""
        );
        let cat: LookCategory = serde_json::from_str("\"Cinematic\"").unwrap();
        assert_eq!(cat, LookCategory::Cinematic);
    }

    #[test]
    fn apply_look_zero_is_default_except_skin_protect() {
        let registry = LookRegistry::load_dir(&looks_dir()).unwrap();
        // podcast-warm exercises the LUT path (zero intensity => bypassed).
        let preset = registry.get("podcast-warm").unwrap();
        let g0 = apply_look(preset, 0.0);
        let mut expected = ColorGrade::default();
        expected.skin_protect = g0.skin_protect;
        assert_eq!(g0, expected);
        assert!(g0.lut.is_none());
        assert!(g0.skin_protect);
    }

    #[test]
    fn apply_look_full_matches_preset() {
        let registry = LookRegistry::load_dir(&looks_dir()).unwrap();
        let preset = registry.get("teal-orange").unwrap();
        let g1 = apply_look(preset, 1.0);
        assert_eq!(g1.contrast, preset.params.contrast);
        assert_eq!(g1.temperature, preset.params.temperature);
        assert_eq!(g1.saturation, preset.params.saturation);
        assert_eq!(g1.lift, preset.params.lift);
        assert_eq!(g1.gain, preset.params.gain);
        assert!(g1.lut.is_none()); // teal-orange has no LUT
        assert!(g1.skin_protect);

        // LUT-carrying preset keeps file, scales intensity, forces After.
        let podcast = registry.get("podcast-warm").unwrap();
        let gp = apply_look(podcast, 1.0);
        let lut = gp.lut.as_ref().unwrap();
        assert_eq!(lut.file, "color/luts/podcast_warm.cube");
        assert!((lut.intensity - 0.6).abs() < 1e-6);
        assert_eq!(lut.position, LutPosition::After);
        let gp_half = apply_look(podcast, 0.5);
        assert!((gp_half.lut.as_ref().unwrap().intensity - 0.3).abs() < 1e-6);
    }

    #[test]
    fn apply_look_temperature_scales_from_6500() {
        let registry = LookRegistry::load_dir(&looks_dir()).unwrap();
        // podcast-warm: temperature 5400 -> 6500 + (5400-6500)*0.5 = 5950.
        let preset = registry.get("podcast-warm").unwrap();
        assert_eq!(preset.params.temperature, 5400.0);
        let g = apply_look(preset, 0.5);
        assert!((g.temperature - 5950.0).abs() < 1e-3);
    }

    #[test]
    fn apply_look_wheels_blend() {
        let registry = LookRegistry::load_dir(&looks_dir()).unwrap();
        // teal-orange: lift [0, 0.01, 0.02, 0], gain [1.05, 1.0, 0.95, 1.0].
        let preset = registry.get("teal-orange").unwrap();
        let g = apply_look(preset, 0.5);
        assert!((g.lift[1] - 0.005).abs() < 1e-6);
        assert!((g.lift[2] - 0.01).abs() < 1e-6);
        assert!((g.gain[0] - 1.025).abs() < 1e-6);
        assert!((g.gain[2] - 0.975).abs() < 1e-6);
    }

    /// Registry with two known looks (+0.5 EV and +0.25 EV exposure deltas)
    /// written to a temp dir, so the composition tests control the exact
    /// math. All tests write identical files, so sharing one dir across the
    /// parallel tests in this process is safe — but the remove/create/write
    /// sequence itself races under parallel execution (one test's
    /// `remove_dir_all` can wipe another's files mid-write), so the whole
    /// helper is serialized on a lock.
    static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn half_ev_registry() -> LookRegistry {
        let _guard = REGISTRY_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("sf-resolve-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (id, ev) in [("half-ev", 0.5), ("quarter-ev", 0.25)] {
            // temperature pinned: ColorGrade's serde default is 0.0, not the
            // 6500 K neutral, so a partial params object would otherwise
            // shift white balance.
            let json = format!(
                "{{\"name\": \"{id}\", \"version\": 1, \"category\": \"Natural\", \
                  \"params\": {{\"exposure\": {ev}, \"temperature\": 6500.0}}, \
                  \"skin_protect\": false}}"
            );
            std::fs::write(dir.join(format!("{id}.sflook")), json).unwrap();
        }
        let registry = LookRegistry::load_dir(&dir).unwrap();
        assert_eq!(registry.len(), 2);
        registry
    }

    #[test]
    fn resolve_grade_base_plus_one_look() {
        let registry = half_ev_registry();
        let mut stack = FilterStack::new();
        stack.push(look_slot("half-ev")).unwrap(); // intensity 1.0, enabled
        let out = resolve_grade(&ColorGrade::default(), &stack, &registry);
        let expected = apply_look(registry.get("half-ev").unwrap(), 1.0);
        assert_eq!(
            out, expected,
            "identity + look must equal the expanded look"
        );
        assert!((out.exposure - 0.5).abs() < 1e-6);
    }

    #[test]
    fn resolve_grade_two_looks_delta_add() {
        let registry = half_ev_registry();
        let mut stack = FilterStack::new();
        stack.push(look_slot("half-ev")).unwrap();
        stack.push(look_slot("quarter-ev")).unwrap();
        let out = resolve_grade(&ColorGrade::default(), &stack, &registry);
        assert!(
            (out.exposure - 0.75).abs() < 1e-6,
            "0.5 + 0.25 EV must compose to 0.75, got {}",
            out.exposure
        );
        // Base deltas fold in too.
        let mut base = ColorGrade::default();
        base.exposure = 0.25;
        let out2 = resolve_grade(&base, &stack, &registry);
        assert!(
            (out2.exposure - 1.0).abs() < 1e-6,
            "0.25 base + 0.75 looks must be 1.0, got {}",
            out2.exposure
        );
    }

    #[test]
    fn resolve_grade_missing_preset_skipped() {
        let registry = half_ev_registry();
        let mut stack = FilterStack::new();
        stack.push(look_slot("no-such-look")).unwrap();
        let out = resolve_grade(&ColorGrade::default(), &stack, &registry);
        assert!(
            out.is_identity(),
            "an unknown preset id must leave the grade alone"
        );
    }

    #[test]
    fn resolve_grade_disabled_look_ignored() {
        let registry = half_ev_registry();
        let mut stack = FilterStack::new();
        let mut slot = look_slot("half-ev");
        slot.enabled = false;
        stack.push(slot).unwrap();
        let out = resolve_grade(&ColorGrade::default(), &stack, &registry);
        assert!(out.is_identity(), "a disabled look must be ignored");
    }

    #[test]
    fn resolve_grade_intensities_multiply() {
        let registry = half_ev_registry();
        let mut stack = FilterStack::new();
        // filter intensity 1.0 x slot intensity 0.5 -> half the +0.5 EV look.
        let mut slot = look_slot("half-ev");
        slot.intensity = 0.5;
        stack.push(slot).unwrap();
        let out = resolve_grade(&ColorGrade::default(), &stack, &registry);
        assert!(
            (out.exposure - 0.25).abs() < 1e-6,
            "filter 1.0 x slot 0.5 of a +0.5 EV look = +0.25 EV, got {}",
            out.exposure
        );
    }

    #[test]
    fn resolve_grade_ignores_effect_slots() {
        let registry = half_ev_registry();
        let mut stack = FilterStack::new();
        stack.push(effect_slot(EffectKind::Vignette)).unwrap();
        stack.push(look_slot("half-ev")).unwrap();
        let out = resolve_grade(&ColorGrade::default(), &stack, &registry);
        assert!(
            (out.exposure - 0.5).abs() < 1e-6,
            "effect slots must not touch the grade, got {}",
            out.exposure
        );
    }

    #[test]
    fn effect_cost_badges() {
        // Low: single-pass, few taps, no state.
        for kind in [
            EffectKind::Sharpen,
            EffectKind::Vignette,
            EffectKind::FilmGrain,
            EffectKind::MirrorFlip,
            EffectKind::CropZoomPan,
            EffectKind::Letterbox,
            EffectKind::ChromaticAberration,
            EffectKind::Pixelate,
        ] {
            assert_eq!(kind.cost(), GpuCost::Low, "{kind:?}");
        }
        // Medium: multi-tap or multi-pass.
        for kind in [
            EffectKind::GaussianBlur,
            EffectKind::ChromaKey,
            EffectKind::Bloom,
            EffectKind::LensCorrection,
        ] {
            assert_eq!(kind.cost(), GpuCost::Medium, "{kind:?}");
        }
        // High: AI inference or temporal state.
        for kind in [
            EffectKind::BackgroundBlur,
            EffectKind::BackgroundReplace,
            EffectKind::NoiseReduction,
            EffectKind::SkinSmooth,
        ] {
            assert_eq!(kind.cost(), GpuCost::High, "{kind:?}");
        }
    }

    /// All 16 effect kinds, for the exhaustive per-kind tests below.
    fn all_kinds() -> [EffectKind; 16] {
        [
            EffectKind::Sharpen,
            EffectKind::GaussianBlur,
            EffectKind::BackgroundBlur,
            EffectKind::BackgroundReplace,
            EffectKind::ChromaKey,
            EffectKind::Vignette,
            EffectKind::FilmGrain,
            EffectKind::Bloom,
            EffectKind::NoiseReduction,
            EffectKind::SkinSmooth,
            EffectKind::MirrorFlip,
            EffectKind::CropZoomPan,
            EffectKind::LensCorrection,
            EffectKind::Letterbox,
            EffectKind::ChromaticAberration,
            EffectKind::Pixelate,
        ]
    }

    #[test]
    fn effect_default_params_sanity() {
        // Shared baseline holds for every kind.
        for kind in all_kinds() {
            let p = kind.default_params();
            assert_eq!(p.amount, 1.0, "{kind:?}");
            assert_eq!(p.softness, 0.5, "{kind:?}");
            assert_eq!(p.center, [0.5, 0.5], "{kind:?}");
            assert_eq!(p.color, [0.0, 0.5, 0.0], "{kind:?}");
            assert_eq!(p.threshold, 0.8, "{kind:?}");
            assert_eq!(p.zoom, 1.5, "{kind:?}");
            assert_eq!(p.aspect, 2.39, "{kind:?}");
            assert_eq!((p.k1, p.k2), (0.0, 0.0), "{kind:?}");
            assert_eq!(p.axis, FlipAxis::Horizontal, "{kind:?}");
            assert_eq!(p.spill, 0.5, "{kind:?}");
            assert_eq!(p.tolerance, 0.15, "{kind:?}");
            assert_eq!(p.key_softness, 0.2, "{kind:?}");
        }
        // Per-kind radius overrides.
        assert_eq!(EffectKind::GaussianBlur.default_params().radius, 4.0);
        assert_eq!(EffectKind::BackgroundBlur.default_params().radius, 4.0);
        assert_eq!(EffectKind::Bloom.default_params().radius, 6.0);
        assert_eq!(EffectKind::ChromaticAberration.default_params().radius, 8.0);
        assert_eq!(EffectKind::Pixelate.default_params().radius, 16.0);
        assert_eq!(EffectKind::Vignette.default_params().radius, 0.0);
        assert_eq!(EffectKind::Sharpen.default_params().radius, 0.0);
        assert_eq!(EffectKind::Letterbox.default_params().radius, 0.0);
    }

    #[test]
    fn is_ai_true_only_for_three_ai_kinds() {
        for kind in [
            EffectKind::BackgroundBlur,
            EffectKind::BackgroundReplace,
            EffectKind::SkinSmooth,
        ] {
            assert!(kind.is_ai(), "{kind:?}");
        }
        for kind in [
            EffectKind::Sharpen,
            EffectKind::GaussianBlur,
            EffectKind::ChromaKey,
            EffectKind::Vignette,
            EffectKind::FilmGrain,
            EffectKind::Bloom,
            EffectKind::NoiseReduction,
            EffectKind::MirrorFlip,
            EffectKind::CropZoomPan,
            EffectKind::LensCorrection,
            EffectKind::Letterbox,
            EffectKind::ChromaticAberration,
            EffectKind::Pixelate,
        ] {
            assert!(!kind.is_ai(), "{kind:?}");
        }
    }

    #[test]
    fn filter_effect_serde_roundtrip() {
        let slot = effect_slot(EffectKind::Letterbox);
        let json = serde_json::to_string(&slot).unwrap();
        let back: FilterSlot = serde_json::from_str(&json).unwrap();
        assert_eq!(slot, back);
        // Shape check: the struct variant carries kind + params.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["filter"]["Effect"]["kind"], "Letterbox");
        assert_eq!(v["filter"]["Effect"]["params"]["aspect"], 2.39);
        assert_eq!(v["filter"]["Effect"]["params"]["axis"], "Horizontal");
    }

    #[test]
    fn filter_slot_serde_roundtrip() {
        let slot = FilterSlot {
            id: Uuid::new_v4(),
            filter: Filter::Look {
                preset_id: "noir".into(),
                intensity: 0.8,
            },
            enabled: true,
            intensity: 0.9,
            mask: Some(MaskRef::Circle {
                x: 0.5,
                y: 0.5,
                radius: 0.3,
                feather: 0.1,
            }),
        };
        let json = serde_json::to_string(&slot).unwrap();
        let back: FilterSlot = serde_json::from_str(&json).unwrap();
        assert_eq!(slot, back);
    }

    #[test]
    fn mask_ref_rectangle_serde_roundtrip() {
        let m = MaskRef::Rectangle {
            x: 0.1,
            y: 0.2,
            w: 0.5,
            h: 0.4,
            feather: 0.15,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MaskRef = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        // Shape check: struct-variant fields by name.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["Rectangle"]["x"], 0.1);
        assert_eq!(v["Rectangle"]["w"], 0.5);
        assert_eq!(v["Rectangle"]["feather"], 0.15);
        // The other two variants still round-trip unchanged.
        let c = MaskRef::Circle {
            x: 0.5,
            y: 0.5,
            radius: 0.3,
            feather: 0.1,
        };
        let back_c: MaskRef = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(c, back_c);
        let back_p: MaskRef =
            serde_json::from_str(&serde_json::to_string(&MaskRef::Person).unwrap()).unwrap();
        assert_eq!(MaskRef::Person, back_p);
    }

    #[test]
    fn partial_grade_json_takes_neutral_defaults() {
        // Regression test: a missing field must mean "no change", not 0.0.
        // (0.0 would be black for gain, grayscale for saturation, 0 K for
        // temperature, and 1/0 = inf for gamma.)
        let g: ColorGrade = serde_json::from_str(r#"{"exposure": 0.5}"#).unwrap();
        assert_eq!(g.exposure, 0.5);
        assert_eq!(g.saturation, 100.0);
        assert_eq!(g.temperature, 6500.0);
        assert_eq!(g.gamma, 1.0);
        assert_eq!(g.gain, [1.0; 4]);
        assert_eq!(g.gamma_wheel, [1.0; 4]);
        assert_eq!(g.brightness, 0.0);
        assert_eq!(g.lift, [0.0; 4]);
    }

    #[test]
    fn builtin_looks_omitting_fields_expand_to_neutral() {
        let registry = LookRegistry::load_dir(&looks_dir()).unwrap();
        // bright-room.sflook omits saturation/gamma/gain/gamma_wheel.
        let bright = registry.get("bright-room").unwrap();
        let g = apply_look(bright, 1.0);
        assert_eq!(g.saturation, 100.0);
        assert_eq!(g.gamma, 1.0);
        assert_eq!(g.gain, [1.0; 4]);
        // fix-low-light.sflook omits temperature.
        let fix = registry.get("fix-low-light").unwrap();
        let gf = apply_look(fix, 1.0);
        assert_eq!(gf.temperature, 6500.0);
        assert!(gf.temperature.is_finite());
    }
}
