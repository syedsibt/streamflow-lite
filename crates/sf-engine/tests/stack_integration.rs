//! Engine-level integration tests for the C5 effect-filter stack (worker 3).
//!
//! Real headless GPU throughout (lavapipe, no mocks):
//!
//! * stack order is honored (f32-level, via `EffectPass::apply_slot`
//!   directly, checked against manual CPU composition in the same order);
//! * the identity bypass is bit-exact and runs zero effect dispatches;
//! * AI effect kinds follow the C7 policy: background blur and skin smooth
//!   are functional without a model (person/skin masks via the hue fallback);
//!   background replace without a model is skipped gracefully (counted,
//!   zero GPU work) and the frame still processes with the other slots;
//! * look folding: a look in the stack == the same look baked into the base
//!   grade, byte-identical;
//! * temporal noise reduction blends with the previous frame, and the first
//!   frame (prev == cur) behaves like no NR;
//! * the two group-C filters (chroma key, noise reduction) agree with their
//!   CPU references on the real shaders.
//!
//! Tolerances are honest f16: the GPU stores through `rgba16float`, so
//! comparisons allow 3e-3 absolute.

use std::path::PathBuf;

use sf_engine::Engine;
use sf_filterfx::cpu;
use sf_filterfx::test_utils::{download_f32_linear, quantize_f16, upload_f32_linear};
use sf_filterfx::EffectPass;
use sf_filters::{
    apply_look, EffectKind, EffectParams, Filter, FilterSlot, FilterStack, FlipAxis, LookRegistry,
};
use sf_render::{RenderContext, RenderError};
use uuid::Uuid;

/// Headless context. Forces the lavapipe Vulkan ICD so the tests run on any
/// machine without a physical GPU.
fn ctx() -> RenderContext {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    RenderContext::new_headless().expect("headless render context")
}

fn engine() -> Engine {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    Engine::new_headless().expect("headless engine")
}

/// 64x64 linear test frame: smooth r/g gradients + checkerboard.
fn test_frame(w: u32, h: u32) -> Vec<f32> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = x as f32 / (w - 1).max(1) as f32;
            let g = y as f32 / (h - 1).max(1) as f32;
            let b = (((x / 8) + (y / 8)) % 2) as f32 * 0.75 + 0.1;
            v.extend_from_slice(&[r, g, b, 1.0]);
        }
    }
    v
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

/// Solid-color RGBA8 frame.
fn solid_rgba8(v: u8) -> Vec<u8> {
    vec![v, v, v, 255]
        .into_iter()
        .cycle()
        .take(64 * 64 * 4)
        .collect()
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

fn max_byte_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i16 - *y as i16).abs() as u8)
        .max()
        .unwrap_or(0)
}

/// The premultiplied amount both sides use: `(amount * intensity).clamp(0,1)`.
fn amount_u(params: &EffectParams, intensity: f32) -> f32 {
    (params.amount * intensity).clamp(0.0, 1.0)
}

fn effect_slot(kind: EffectKind, intensity: f32) -> FilterSlot {
    FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: kind.clone(),
            params: kind.default_params(),
        },
        enabled: true,
        intensity,
        mask: None,
    }
}

fn look_slot(preset_id: &str, intensity: f32) -> FilterSlot {
    FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Look {
            preset_id: preset_id.to_string(),
            intensity,
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
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

fn looks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/looks")
}

#[test]
fn stack_order_honored() {
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let q = quantize_f16(&input);

    let mut v = EffectKind::Vignette.default_params();
    v.center = [0.25, 0.75]; // asymmetric: flip order must matter
    v.radius = 0.3;
    v.softness = 0.2;
    let mut m = EffectKind::MirrorFlip.default_params();
    m.axis = FlipAxis::Vertical;
    let (uv, um) = (amount_u(&v, 0.7), amount_u(&m, 1.0));

    // Order 1: vignette -> mirror flip.
    let src = upload_f32_linear(&rc, w, h, &input);
    let t1 = rc.create_linear_target(w, h);
    let t2 = rc.create_linear_target(w, h);
    assert!(fx
        .apply_slot(&rc, &EffectKind::Vignette, &v, 0.7, &src, &t1, None, 0)
        .unwrap());
    assert!(fx
        .apply_slot(&rc, &EffectKind::MirrorFlip, &m, 1.0, &t1, &t2, None, 0)
        .unwrap());
    let out1 = download_f32_linear(&rc, &t2);

    // Order 2: mirror flip -> vignette.
    let src2 = upload_f32_linear(&rc, w, h, &input);
    let u1 = rc.create_linear_target(w, h);
    let u2 = rc.create_linear_target(w, h);
    assert!(fx
        .apply_slot(&rc, &EffectKind::MirrorFlip, &m, 1.0, &src2, &u1, None, 0)
        .unwrap());
    assert!(fx
        .apply_slot(&rc, &EffectKind::Vignette, &v, 0.7, &u1, &u2, None, 0)
        .unwrap());
    let out2 = download_f32_linear(&rc, &u2);

    // Manual CPU composition in the same orders.
    let cpu1 = cpu::mirror_flip_cpu(w, h, &cpu::vignette_cpu(w, h, &q, &v, uv), &m, um);
    let cpu2 = cpu::vignette_cpu(w, h, &cpu::mirror_flip_cpu(w, h, &q, &m, um), &v, uv);

    let d1 = max_abs_diff(&out1, &cpu1);
    let d2 = max_abs_diff(&out2, &cpu2);
    println!("stack_order_honored: order1 vs cpu = {d1}, order2 vs cpu = {d2}");
    assert!(d1 <= 3e-3, "vignette->mirror diverged from CPU: {d1}");
    assert!(d2 <= 3e-3, "mirror->vignette diverged from CPU: {d2}");

    // The orders genuinely differ (this is what proves order, not just that
    // each matches its own CPU composition).
    let order_diff = max_abs_diff(&out1, &out2);
    println!("stack_order_honored: order1 vs order2 = {order_diff}");
    assert!(
        order_diff > 1e-2,
        "swapped stack order must change the output"
    );
    assert_eq!(fx.passes_executed(), 4, "two slots x two orders");
}

#[test]
fn bypass_is_bit_identical() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    // A disabled slot at full intensity plus an enabled slot at zero
    // intensity: neither may reach the GPU.
    let mut disabled = effect_slot(EffectKind::Vignette, 1.0);
    disabled.enabled = false;
    set_stack(
        &mut e,
        "cam1",
        vec![disabled, effect_slot(EffectKind::Vignette, 0.0)],
    );

    let px = gradient64_rgba8();
    let frame = e
        .grade_input_frame("cam1", 64, 64, &px)
        .expect("bypass frame");
    assert_eq!(frame.pixels, px, "bypass must return input bytes exactly");
    assert_eq!(
        e.effect_passes(),
        0,
        "bypass must run zero effect dispatches"
    );
}

#[test]
fn ai_effects_c7_policy() {
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    set_stack(
        &mut e,
        "cam1",
        vec![
            effect_slot(EffectKind::BackgroundBlur, 1.0), // AI: functional (hue fallback)
            effect_slot(EffectKind::BackgroundReplace, 1.0), // AI: skipped, no model
            effect_slot(EffectKind::Vignette, 0.5),       // non-AI: runs
        ],
    );

    let px = gradient64_rgba8();
    let frame = e
        .grade_input_frame("cam1", 64, 64, &px)
        .expect("frame with AI slots must still process");
    assert_eq!(
        e.ai_skipped(),
        1,
        "only the model-less background replace is skipped"
    );
    assert_eq!(
        e.effect_passes(),
        4,
        "vignette (1 dispatch) + background blur (2-pass blur + mix = 3); \
         the skipped replace does zero GPU work"
    );
    assert_eq!(e.mask_uploads(), 1, "the blur uploaded one person mask");
    assert_ne!(
        frame.pixels, px,
        "the working slots must still affect the frame"
    );
}

#[test]
fn look_folding_matches_baked_grade() {
    let registry = LookRegistry::load_dir(&looks_dir()).expect("built-in looks");
    let preset = registry.get("teal-orange").expect("teal-orange exists");
    let baked = apply_look(preset, 1.0);

    let px = gradient64_rgba8();

    // Input A: identity base grade + the look in the stack.
    let mut ea = engine();
    ea.state_mut().add_input("a", "A");
    *ea.look_registry_mut() = LookRegistry::load_dir(&looks_dir()).expect("looks");
    set_stack(&mut ea, "a", vec![look_slot("teal-orange", 1.0)]);
    let frame_a = ea.grade_input_frame("a", 64, 64, &px).expect("A");

    // Input B: the same look baked into the base grade, empty stack.
    let mut eb = engine();
    eb.state_mut().add_input("b", "B");
    eb.state_mut().input_color_mut("b").expect("b exists").grade = baked;
    let frame_b = eb.grade_input_frame("b", 64, 64, &px).expect("B");

    assert_ne!(frame_a.pixels, px, "the look must change the frame");
    assert_eq!(
        frame_a.pixels, frame_b.pixels,
        "stack look and baked grade must be byte-identical"
    );
}

#[test]
fn noise_reduction_is_temporal() {
    let px_dark = solid_rgba8(30);
    let px_bright = solid_rgba8(200);

    // Engine with NR @ u=1: frame 2 must come out as (a blend toward) frame 1.
    let mut e = engine();
    e.state_mut().add_input("cam1", "Camera 1");
    set_stack(
        &mut e,
        "cam1",
        vec![effect_slot(EffectKind::NoiseReduction, 1.0)],
    );
    let out1 = e
        .grade_input_frame("cam1", 64, 64, &px_dark)
        .expect("frame 1");
    let out2 = e
        .grade_input_frame("cam1", 64, 64, &px_bright)
        .expect("frame 2");

    // Same bright frame through a fresh engine with no NR: the plain result.
    let mut e_plain = engine();
    e_plain.state_mut().add_input("cam1", "Camera 1");
    let out2_plain = e_plain
        .grade_input_frame("cam1", 64, 64, &px_bright)
        .expect("plain frame 2");

    // At u=1 the output IS the previous frame: out2 ~= dark, far from bright.
    let d = max_byte_diff(&out2.pixels, &out2_plain.pixels);
    println!("noise_reduction_is_temporal: out2 vs plain = {d} (u8)");
    assert!(
        d > 100,
        "u=1 NR must replace the frame with the previous one, diff = {d}"
    );
    let d_prev = max_byte_diff(&out2.pixels, &out1.pixels);
    assert!(
        d_prev <= 2,
        "u=1 NR output must equal the previous frame's output, diff = {d_prev}"
    );

    // First frame (prev == cur): identical to the same frame without NR,
    // modulo the f16 linearize/encode roundtrip.
    let d_first = max_byte_diff(&out1.pixels, &px_dark);
    println!("noise_reduction_is_temporal: first-frame vs input = {d_first} (u8)");
    assert!(
        d_first <= 2,
        "first NR frame (prev==cur) must match the input within f16 roundtrip, diff = {d_first}"
    );
}

#[test]
fn chroma_key_agrees_with_cpu() {
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let (w, h) = (64, 64);
    // Left half green screen, right half red subject (linear values).
    let mut input = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let _ = y;
            if x < w / 2 {
                input.extend_from_slice(&[0.05, 0.55, 0.05, 1.0]);
            } else {
                input.extend_from_slice(&[0.6, 0.1, 0.1, 1.0]);
            }
        }
    }
    let mut params = EffectKind::ChromaKey.default_params();
    params.color = [0.0, 0.5, 0.0];
    params.tolerance = 0.15;
    params.key_softness = 0.2;
    params.spill = 0.5;
    let intensity = 1.0;

    let src = upload_f32_linear(&rc, w, h, &input);
    let dst = rc.create_linear_target(w, h);
    let applied = fx
        .apply_slot(
            &rc,
            &EffectKind::ChromaKey,
            &params,
            intensity,
            &src,
            &dst,
            None,
            0,
        )
        .expect("apply_slot");
    assert!(applied);
    assert_eq!(fx.passes_executed(), 1);
    let gpu = download_f32_linear(&rc, &dst);

    let cpu = cpu::chroma_key_cpu(
        w,
        h,
        &quantize_f16(&input),
        &params,
        amount_u(&params, intensity),
    );
    let max = max_abs_diff(&gpu, &cpu);
    println!("chroma_key_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 3e-3, "chroma key diverged from CPU reference: {max}");

    // Sanity: the green half keyed out (alpha -> 0), the red half kept.
    let keyed_px = ((h / 2 * w + w / 4) * 4) as usize;
    let kept_px = ((h / 2 * w + 3 * w / 4) * 4) as usize;
    assert!(
        gpu[keyed_px + 3] < 0.05,
        "green screen pixel must key out, alpha = {}",
        gpu[keyed_px + 3]
    );
    assert!(
        (gpu[kept_px + 3] - 1.0).abs() < 1e-3,
        "red subject pixel must stay opaque, alpha = {}",
        gpu[kept_px + 3]
    );
    assert!(
        (cpu[keyed_px + 3] - gpu[keyed_px + 3]).abs() <= 3e-3,
        "CPU and GPU must agree on the keyed alpha"
    );
}

#[test]
fn noise_reduction_agrees_with_cpu() {
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let (w, h) = (64, 64);
    let cur = test_frame(w, h);
    // Previous frame: the same gradients shifted (a genuinely different frame).
    let prev: Vec<f32> = cur
        .chunks_exact(4)
        .map(|px| [px[0] * 0.5 + 0.1, px[1] * 0.5, px[2], px[3]])
        .flat_map(|px| px.into_iter())
        .collect();
    let params = EffectKind::NoiseReduction.default_params();
    let intensity = 0.5; // u = 0.5: an honest blend, not a copy

    let src = upload_f32_linear(&rc, w, h, &cur);
    let prev_tex = upload_f32_linear(&rc, w, h, &prev);
    let dst = rc.create_linear_target(w, h);
    let applied = fx
        .apply_slot(
            &rc,
            &EffectKind::NoiseReduction,
            &params,
            intensity,
            &src,
            &dst,
            Some(&prev_tex),
            7,
        )
        .expect("apply_slot");
    assert!(applied);
    assert_eq!(fx.passes_executed(), 1);
    let gpu = download_f32_linear(&rc, &dst);

    let cpu = cpu::noise_reduction_cpu(
        w,
        h,
        &quantize_f16(&cur),
        Some(&quantize_f16(&prev)),
        &params,
        amount_u(&params, intensity),
    );
    let max = max_abs_diff(&gpu, &cpu);
    println!("noise_reduction_agrees_with_cpu (u=0.5): max abs diff = {max}");
    assert!(max <= 3e-3, "noise reduction diverged from CPU: {max}");

    // At u=1 the blend is exactly the previous frame (alpha stays cur's).
    let dst2 = rc.create_linear_target(w, h);
    fx.apply_slot(
        &rc,
        &EffectKind::NoiseReduction,
        &params,
        1.0,
        &src,
        &dst2,
        Some(&prev_tex),
        7,
    )
    .expect("apply_slot u=1");
    let gpu_full = download_f32_linear(&rc, &dst2);
    let q_prev = quantize_f16(&prev);
    let q_cur = quantize_f16(&cur);
    for (i, px) in gpu_full.chunks_exact(4).enumerate() {
        assert!(
            (px[0] - q_prev[i * 4]).abs() <= 3e-3
                && (px[1] - q_prev[i * 4 + 1]).abs() <= 3e-3
                && (px[2] - q_prev[i * 4 + 2]).abs() <= 3e-3,
            "u=1 must copy prev rgb"
        );
        assert!(
            (px[3] - q_cur[i * 4 + 3]).abs() <= 3e-3,
            "alpha must stay the current frame's"
        );
    }
}

#[test]
fn noise_reduction_requires_prev_texture() {
    // Locks the worker-1 contract the engine relies on: the temporal
    // bind-group layout needs a prev texture, so apply_slot errors when it
    // is missing instead of running a half-bound dispatch.
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let input = test_frame(32, 32);
    let src = upload_f32_linear(&rc, 32, 32, &input);
    let dst = rc.create_linear_target(32, 32);
    let params = EffectKind::NoiseReduction.default_params();
    let err = fx
        .apply_slot(
            &rc,
            &EffectKind::NoiseReduction,
            &params,
            1.0,
            &src,
            &dst,
            None,
            0,
        )
        .expect_err("prev=None must fail for the temporal kind");
    assert!(
        matches!(err, RenderError::SizeMismatch),
        "expected SizeMismatch, got {err:?}"
    );
    assert_eq!(fx.passes_executed(), 0, "the failed slot ran no dispatch");
}
