//! Engine-level integration tests for the C7 AI effects + region masks.
//!
//! Real headless GPU throughout (lavapipe, no mocks):
//!
//! * region-masked effect slots (Circle / Rectangle / Person): masked-out
//!   pixels are bit-identical to the unmasked frame, deep-inside pixels are
//!   bit-identical to the full-effect run, the feather band shows a gradual
//!   transition;
//! * the AI mask cache refresh policy (recompute on size change or every 15
//!   frames — deterministic, driven by the frame counter, never wall-clock);
//! * [`EffectKind::BackgroundReplace`] without a model is skipped gracefully
//!   (`ai_skipped`, zero GPU work, other slots keep working);
//! * [`EffectKind::BackgroundBlur`] is functional without a model (hue
//!   fallback): the skin-tone "person" region stays sharp, the background
//!   is blurred;
//! * [`EffectKind::BackgroundReplace`] with a (fake) model replaces the
//!   background with the plate color;
//! * [`EffectKind::SkinSmooth`] routes through the skin mask.

use sf_ai_runtime::{AiError, FaceMask, InferenceSession};
use sf_engine::Engine;
use sf_filters::{EffectKind, EffectParams, Filter, FilterSlot, FilterStack, MaskRef};
use uuid::Uuid;

/// Headless engine (forces the lavapipe Vulkan ICD).
fn engine() -> Engine {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    Engine::new_headless().expect("headless engine")
}

/// 64x64 RGBA8 gradient: r = x*4, g = y*4, b = (x+y)*2, a = 255.
fn gradient64_rgba8() -> Vec<u8> {
    let mut px = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            px.push((x * 4) as u8);
            px.push((y * 4) as u8);
            px.push(((x + y) * 2) as u8);
            px.push(255);
        }
    }
    px
}

fn effect_slot(kind: EffectKind, intensity: f32, mask: Option<MaskRef>) -> FilterSlot {
    FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: kind.clone(),
            params: kind.default_params(),
        },
        enabled: true,
        intensity,
        mask,
    }
}

/// Pushes `slots` into the named input's filter stack.
fn set_stack(engine: &mut Engine, input_id: &str, slots: Vec<FilterSlot>) {
    let input = engine
        .state_mut()
        .inputs
        .iter_mut()
        .find(|i| i.id == input_id)
        .expect("input exists");
    let mut stack = FilterStack::new();
    for slot in slots {
        stack.push(slot).unwrap();
    }
    input.filters = stack;
}

fn px(frame: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * 64 + x) * 4) as usize;
    [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
}

fn max_byte_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i16 - *y as i16).abs() as u8)
        .max()
        .unwrap_or(0)
}

/// UV-space distance from `(cx, cy)` for pixel `(x, y)` on a 64x64 frame.
fn uv_dist(x: u32, y: u32, cx: f32, cy: f32) -> f32 {
    let u = (x as f32 + 0.5) / 64.0;
    let v = (y as f32 + 0.5) / 64.0;
    ((u - cx).powi(2) + (v - cy).powi(2)).sqrt()
}

fn circle_mask(cx: f32, cy: f32, radius: f32, feather: f32) -> MaskRef {
    MaskRef::Circle {
        x: cx,
        y: cy,
        radius,
        feather,
    }
}

#[test]
fn region_circle_vignette_masked_blend() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    let frame = gradient64_rgba8();

    // Circle near the corner (vignette is strong there), feather band
    // strictly inside the radius: wgt = 1 - sstep(r-feather, r, d).
    let (cx, cy, r, f) = (0.8f32, 0.78f32, 0.22f32, 0.1f32);
    let masked = effect_slot(EffectKind::Vignette, 1.0, Some(circle_mask(cx, cy, r, f)));
    // All-zero mask: same pipeline, effect fully masked out.
    let zero = effect_slot(
        EffectKind::Vignette,
        1.0,
        Some(circle_mask(cx, cy, 0.0, 0.0)),
    );
    let full = effect_slot(EffectKind::Vignette, 1.0, None);

    set_stack(&mut e, "cam1", vec![masked]);
    let m = e.grade_input_frame("cam1", 64, 64, &frame).unwrap().pixels;
    set_stack(&mut e, "cam1", vec![zero]);
    let z = e.grade_input_frame("cam1", 64, 64, &frame).unwrap().pixels;
    set_stack(&mut e, "cam1", vec![full]);
    let f_full = e.grade_input_frame("cam1", 64, 64, &frame).unwrap().pixels;

    // Deep inside the circle (d << r - feather): effect fully applied.
    let (ix, iy) = (51u32, 50u32);
    assert!(
        uv_dist(ix, iy, cx, cy) < r - f,
        "test pixel must be deep inside the circle"
    );
    assert_eq!(
        px(&m, ix, iy),
        px(&f_full, ix, iy),
        "deep inside: masked == full-effect run (bit-identical)"
    );
    assert_ne!(
        px(&m, ix, iy),
        px(&z, ix, iy),
        "deep inside: effect actually changed the pixels"
    );

    // Outside the circle (d > r): bit-identical to the unmasked frame.
    let (ox, oy) = (32u32, 32u32);
    assert!(uv_dist(ox, oy, cx, cy) > r, "test pixel must be outside");
    assert_eq!(
        px(&m, ox, oy),
        px(&z, ox, oy),
        "outside: masked-out pixels bit-identical to the unmasked frame"
    );

    // Feather band: mid-band pixels land strictly between the unmasked and
    // full-effect values — a gradual transition, not a hard jump.
    let mut gradual = 0;
    for y in 0..64u32 {
        for x in 0..64u32 {
            let d = uv_dist(x, y, cx, cy);
            if (d - (r - f / 2.0)).abs() > 0.02 {
                continue;
            }
            let zm = px(&z, x, y);
            let mm = px(&m, x, y);
            let fm = px(&f_full, x, y);
            // Vignette only darkens: z >= m >= f_full per channel.
            let ok = (0..3).all(|c| zm[c] >= mm[c] && mm[c] >= fm[c])
                && (0..3).any(|c| zm[c] > mm[c] && mm[c] > fm[c]);
            if ok {
                gradual += 1;
            }
        }
    }
    assert!(
        gradual >= 4,
        "feather band must show gradual transition, found {gradual} gradual pixels"
    );
}

#[test]
fn region_rectangle_vignette_spot_check() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    let frame = gradient64_rgba8();

    let rect = MaskRef::Rectangle {
        x: 0.6,
        y: 0.6,
        w: 0.3,
        h: 0.3,
        feather: 0.05,
    };
    let masked = effect_slot(EffectKind::Vignette, 1.0, Some(rect));
    let zero = effect_slot(
        EffectKind::Vignette,
        1.0,
        Some(MaskRef::Rectangle {
            x: 0.6,
            y: 0.6,
            w: 0.0,
            h: 0.0,
            feather: 0.0,
        }),
    );
    let full = effect_slot(EffectKind::Vignette, 1.0, None);

    set_stack(&mut e, "cam1", vec![masked]);
    let m = e.grade_input_frame("cam1", 64, 64, &frame).unwrap().pixels;
    set_stack(&mut e, "cam1", vec![zero]);
    let z = e.grade_input_frame("cam1", 64, 64, &frame).unwrap().pixels;
    set_stack(&mut e, "cam1", vec![full]);
    let f_full = e.grade_input_frame("cam1", 64, 64, &frame).unwrap().pixels;

    // Rect center (0.75, 0.75) -> pixel (48, 48): deep inside.
    assert_eq!(
        px(&m, 48, 48),
        px(&f_full, 48, 48),
        "rect inside: masked == full-effect run"
    );
    assert_ne!(
        px(&m, 48, 48),
        px(&z, 48, 48),
        "rect inside: effect applied"
    );
    // Far outside the rect: bit-identical to the unmasked frame.
    assert_eq!(
        px(&m, 16, 16),
        px(&z, 16, 16),
        "rect outside: masked-out pixels bit-identical"
    );
}

/// Frame: blue background, one off-center skin-tone rect (the hue fallback
/// sees it as the "person").
fn person_test_frame() -> Vec<u8> {
    let mut px = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            let skin = (36..56).contains(&x) && (36..56).contains(&y);
            if skin {
                px.extend_from_slice(&[200, 150, 120, 255]);
            } else {
                px.extend_from_slice(&[0, 0, 255, 255]);
            }
        }
    }
    px
}

#[test]
fn region_person_mask_uses_mask_path() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    let frame = person_test_frame();

    // Vignette masked by the person mask (stub session -> hue fallback).
    // Vignette center is (0.5, 0.5); the skin rect sits at uv ~0.73 where
    // the vignette darkens strongly.
    let masked = effect_slot(EffectKind::Vignette, 1.0, Some(MaskRef::Person));
    set_stack(&mut e, "cam1", vec![masked]);
    let m = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    // Same pipeline with an all-zero mask: the no-effect reference.
    let zero = effect_slot(
        EffectKind::Vignette,
        1.0,
        Some(circle_mask(0.8, 0.78, 0.0, 0.0)),
    );
    set_stack(&mut e, "cam1", vec![zero]);
    let z = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    // A pixel deep in the skin rect: mask ~1, vignette applied -> changed.
    assert_ne!(
        px(&m.pixels, 46, 46),
        px(&z.pixels, 46, 46),
        "person-masked effect must apply inside the skin region"
    );
    // A far-away blue pixel: mask ~0 -> bit-identical to the unmasked frame.
    assert_eq!(
        px(&m.pixels, 4, 4),
        px(&z.pixels, 4, 4),
        "person mask ~0 outside: pixel bit-identical to unmasked frame"
    );
    // The person mask was computed and cached exactly once.
    assert_eq!(e.mask_uploads(), 1);
}

#[test]
fn mask_cache_refresh_policy() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    // Uniform skin-tone frame: hue person mask is high everywhere.
    let frame: Vec<u8> = (0..32 * 32).flat_map(|_| [200u8, 150, 120, 255]).collect();

    set_stack(
        &mut e,
        "cam1",
        vec![effect_slot(EffectKind::BackgroundBlur, 1.0, None)],
    );
    for i in 1..=16u32 {
        e.grade_input_frame("cam1", 32, 32, &frame)
            .expect("frame processes");
        let uploads = e.mask_uploads();
        if i == 1 {
            assert_eq!(uploads, 1, "frame 1 uploads the mask");
        } else if i < 16 {
            assert_eq!(uploads, 1, "frames 2..15 reuse the cached mask (frame {i})");
        } else {
            assert_eq!(uploads, 2, "frame 16 refreshes the mask");
        }
    }
}

#[test]
fn background_replace_without_model_skips_gracefully() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    let frame = gradient64_rgba8();

    // No model (default stub session): BackgroundReplace must do zero GPU
    // work and count one skip; the Vignette slot still applies.
    let mut replace_params = EffectKind::BackgroundReplace.default_params();
    replace_params.color = [1.0, 0.0, 0.0];
    let replace = FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: EffectKind::BackgroundReplace,
            params: replace_params,
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
    };
    let vignette = effect_slot(EffectKind::Vignette, 1.0, None);
    set_stack(&mut e, "cam1", vec![replace, vignette]);
    let out = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    assert_eq!(e.ai_skipped(), 1, "one graceful skip counted");
    assert_eq!(e.mask_uploads(), 0, "skipped slot uploads no mask");

    // Identical to a vignette-only run: the skipped slot did zero GPU work.
    let mut e2 = engine();
    e2.state_mut().add_input("cam1", "Camera 1");
    set_stack(
        &mut e2,
        "cam1",
        vec![effect_slot(EffectKind::Vignette, 1.0, None)],
    );
    let only_vignette = e2.grade_input_frame("cam1", 64, 64, &frame).unwrap();
    assert_eq!(
        max_byte_diff(&out.pixels, &only_vignette.pixels),
        0,
        "skipped replace == vignette-only run, byte-identical"
    );
}

/// Checkerboard background (mask ~0) + skin-tone rect (mask ~1 via the hue
/// fallback): background blurs, the "person" stays sharp.
fn blur_test_frame() -> Vec<u8> {
    let mut px = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            if (24..40).contains(&x) && (24..40).contains(&y) {
                px.extend_from_slice(&[200, 150, 120, 255]);
            } else if ((x / 8) + (y / 8)) % 2 == 0 {
                px.extend_from_slice(&[0, 0, 0, 255]);
            } else {
                px.extend_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    px
}

#[test]
fn background_blur_functional_without_model() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    let frame = blur_test_frame();

    // B1: the AI background blur (stub session -> hue fallback).
    let mut params: EffectParams = EffectKind::BackgroundBlur.default_params();
    params.radius = 6.0;
    let blur = FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: EffectKind::BackgroundBlur,
            params: params.clone(),
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
    };
    set_stack(&mut e, "cam1", vec![blur]);
    let b1 = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    // B2: the same blur as a plain full-frame gaussian (same radius).
    let mut gparams: EffectParams = EffectKind::GaussianBlur.default_params();
    gparams.radius = 6.0;
    let gauss = FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: EffectKind::GaussianBlur,
            params: gparams,
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
    };
    set_stack(&mut e, "cam1", vec![gauss]);
    let b2 = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    // Background checker far from the rect: person mask ~0 -> the full blur
    // applies, byte-identical to the plain gaussian run.
    assert_eq!(
        px(&b1.pixels, 4, 4),
        px(&b2.pixels, 4, 4),
        "background: bg-blur == full gaussian blur (bit-identical)"
    );
    // Skin rect edge: the person mask keeps it sharp, so the masked blur
    // differs clearly from the full blur there.
    let e1 = px(&b1.pixels, 24, 32);
    let e2 = px(&b2.pixels, 24, 32);
    let edge_diff = e1
        .iter()
        .zip(e2.iter())
        .map(|(a, b)| (*a as i16 - *b as i16).abs())
        .max()
        .unwrap();
    assert!(
        edge_diff > 10,
        "person edge must stay sharper than full blur, diff = {edge_diff}"
    );
    assert_eq!(e.ai_skipped(), 0, "blur never counts as skipped");
    assert_eq!(e.mask_uploads(), 1, "person mask uploaded once");
}

/// Fake model-backed session: exercises the model-available paths without
/// a real ONNX backend.
struct FakeSeg {
    w: u32,
    h: u32,
    weights: Vec<f32>,
}

impl InferenceSession for FakeSeg {
    fn name(&self) -> &str {
        "fake-seg"
    }
    fn model_available(&self) -> bool {
        true
    }
    fn face_mask(&mut self, rgb: &[u8], width: u32, height: u32) -> Result<FaceMask, AiError> {
        assert_eq!(rgb.len(), width as usize * height as usize * 3);
        Ok(FaceMask {
            width: self.w,
            height: self.h,
            weights: self.weights.clone(),
        })
    }
}

/// 8x8 mask: 1.0 in the center 4x4, 0.0 at the border.
fn fake_seg_center() -> FakeSeg {
    let mut weights = vec![0.0f32; 64];
    for y in 2..6u32 {
        for x in 2..6u32 {
            weights[(y * 8 + x) as usize] = 1.0;
        }
    }
    FakeSeg {
        w: 8,
        h: 8,
        weights,
    }
}

#[test]
fn background_replace_with_model_replaces_background() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    e.set_ai_session(Box::new(fake_seg_center()));
    assert!(e.ai_model_available());
    let frame = gradient64_rgba8();

    let mut params = EffectKind::BackgroundReplace.default_params();
    params.color = [1.0, 0.0, 0.0]; // linear red plate
    let replace = FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: EffectKind::BackgroundReplace,
            params,
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
    };
    set_stack(&mut e, "cam1", vec![replace]);
    let out = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    assert_eq!(e.ai_skipped(), 0, "model available: nothing skipped");
    assert_eq!(e.mask_uploads(), 1, "person mask uploaded once");

    // Mask convention (see bg_replace.wgsl): m = 1 keeps the source
    // (person), m = 0 takes the plate (background). The fake mask is 1 in
    // the center 4x4 of 8x8, so:
    // - corner (4,4): mask = 0 -> plate red (linear 1,0,0 -> sRGB red).
    let c = px(&out.pixels, 4, 4);
    assert!(
        c[0] > 240 && c[1] < 15 && c[2] < 15,
        "corner must be plate red, got {c:?}"
    );

    // All-ones model mask: same pipeline, everything kept — the center
    // pixel of the real run must match it bit-identically (mask = 1 keeps
    // the source), and its corner must NOT be red.
    let mut e2 = engine();
    e2.state_mut().add_input("cam1", "Camera 1");
    e2.set_ai_session(Box::new(FakeSeg {
        w: 8,
        h: 8,
        weights: vec![1.0; 64],
    }));
    let mut params2 = EffectKind::BackgroundReplace.default_params();
    params2.color = [1.0, 0.0, 0.0];
    let replace2 = FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: EffectKind::BackgroundReplace,
            params: params2,
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
    };
    set_stack(&mut e2, "cam1", vec![replace2]);
    let kept = e2.grade_input_frame("cam1", 64, 64, &frame).unwrap();
    assert_eq!(
        px(&out.pixels, 32, 32),
        px(&kept.pixels, 32, 32),
        "mask = 1 keeps the source, bit-identical"
    );
    assert!(
        px(&kept.pixels, 4, 4)[0] < 240,
        "all-ones mask run must not paint the plate"
    );
}

#[test]
fn skin_smooth_routing_applies_on_skin() {
    // All-ones model mask: skin everywhere -> skin smooth must equal a
    // plain full-frame gaussian at the fallback radius (2.0), bit-identical.
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    e.set_ai_session(Box::new(FakeSeg {
        w: 8,
        h: 8,
        weights: vec![1.0; 64],
    }));
    let frame = blur_test_frame(); // skin rect + checkerboard

    let smooth = effect_slot(EffectKind::SkinSmooth, 1.0, None);
    set_stack(&mut e, "cam1", vec![smooth]);
    let s = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    let mut gparams = EffectKind::GaussianBlur.default_params();
    gparams.radius = 2.0; // the smooth fallback radius
    let gauss = FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: EffectKind::GaussianBlur,
            params: gparams,
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
    };
    set_stack(&mut e, "cam1", vec![gauss]);
    let g = e.grade_input_frame("cam1", 64, 64, &frame).unwrap();

    assert_eq!(
        max_byte_diff(&s.pixels, &g.pixels),
        0,
        "all-ones skin mask: skin smooth == full gaussian blur, byte-identical"
    );
    assert_eq!(e.ai_skipped(), 0, "skin smooth never counts as skipped");

    // And functional without a model (stub session -> hue path): completes,
    // uploads exactly one skin mask, skips nothing.
    let mut e2 = engine();
    e2.state_mut().add_input("cam1", "Camera 1");
    set_stack(
        &mut e2,
        "cam1",
        vec![effect_slot(EffectKind::SkinSmooth, 1.0, None)],
    );
    e2.grade_input_frame("cam1", 64, 64, &frame)
        .expect("skin smooth works without a model");
    assert_eq!(e2.ai_skipped(), 0);
    assert_eq!(e2.mask_uploads(), 1);
}
