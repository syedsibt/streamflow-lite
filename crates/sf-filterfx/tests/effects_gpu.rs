//! Real headless-GPU tests for the group-A effect filters (lavapipe, no mocks).
//!
//! Each test uploads a small f32 linear frame, runs the actual WGSL shader
//! via [`EffectPass::apply_slot`], downloads the result, and compares it
//! against the CPU reference in `sf_filterfx::cpu` — the exact math contract
//! both sides implement. Tolerances are honest: the GPU stores its result
//! through f16, so comparisons allow the f16 quantization step (3e-3);
//! integer-exact filters (mirror, letterbox) assert a tighter 1e-3 bound.

use sf_filterfx::cpu;
use sf_filterfx::test_utils::{download_f32_linear, quantize_f16, upload_f32_linear};
use sf_filterfx::EffectPass;
use sf_filters::{EffectKind, EffectParams, FlipAxis, MaskRef};
use sf_render::{RenderContext, RenderError};

/// Headless context. Forces the lavapipe Vulkan ICD so the tests run on any
/// machine without a physical GPU.
fn ctx() -> RenderContext {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    RenderContext::new_headless().expect("headless render context")
}

/// 64x64 linear test frame: smooth r/g gradients, a checkerboard patch, and
/// a dark corner — exercises vignette falloff, grain everywhere, letterbox
/// bars, and mirror symmetry.
fn test_frame(w: u32, h: u32) -> Vec<f32> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = x as f32 / (w - 1).max(1) as f32;
            let g = y as f32 / (h - 1).max(1) as f32;
            let b = (((x / 8) + (y / 8)) % 2) as f32 * 0.75 + 0.1;
            let dark = if x < w / 4 && y < h / 4 { 0.05 } else { 1.0 };
            v.extend_from_slice(&[r * dark, g * dark, b * dark, 1.0]);
        }
    }
    v
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

/// Runs one effect slot on `input` and returns (downloaded output, total
/// dispatches executed).
fn run_slot(
    kind: &EffectKind,
    params: &EffectParams,
    intensity: f32,
    seed: u32,
    input: &[f32],
    w: u32,
    h: u32,
) -> (Vec<f32>, u64) {
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let src = upload_f32_linear(&rc, w, h, input);
    let dst = rc.create_linear_target(w, h);
    let applied = fx
        .apply_slot(&rc, kind, params, intensity, &src, &dst, None, seed)
        .expect("apply_slot");
    assert!(applied, "apply_slot returned false for {kind:?}");
    let out = download_f32_linear(&rc, &dst);
    (out, fx.passes_executed())
}

/// The premultiplied amount both sides use: `(amount * intensity).clamp(0,1)`.
fn amount_u(params: &EffectParams, intensity: f32) -> f32 {
    (params.amount * intensity).clamp(0.0, 1.0)
}

#[test]
fn upload_download_roundtrip_is_exact() {
    let rc = ctx();
    let input = test_frame(64, 64);
    let tex = upload_f32_linear(&rc, 64, 64, &input);
    let back = download_f32_linear(&rc, &tex);
    let q = quantize_f16(&input);
    let max = max_abs_diff(&back, &q);
    println!("upload_download_roundtrip_is_exact: max abs diff = {max}");
    assert_eq!(max, 0.0, "f32->f16->f32 roundtrip must be bit-exact");
}

#[test]
fn vignette_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::Vignette.default_params();
    params.center = [0.25, 0.75]; // non-default center
    params.radius = 0.3;
    params.softness = 0.2;
    let intensity = 0.7; // non-default intensity
    let (gpu, passes) = run_slot(&EffectKind::Vignette, &params, intensity, 0, &input, w, h);
    assert_eq!(passes, 1, "vignette is a single dispatch");
    let cpu = cpu::vignette_cpu(
        w,
        h,
        &quantize_f16(&input),
        &params,
        amount_u(&params, intensity),
    );
    let max = max_abs_diff(&gpu, &cpu);
    println!("vignette_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 3e-3, "vignette diverged from CPU reference: {max}");
}

#[test]
fn film_grain_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let params = EffectKind::FilmGrain.default_params();
    let intensity = 0.5;
    let seed = 12345u32; // non-zero frame seed
    let (gpu, passes) = run_slot(
        &EffectKind::FilmGrain,
        &params,
        intensity,
        seed,
        &input,
        w,
        h,
    );
    assert_eq!(passes, 1, "film grain is a single dispatch");
    let cpu = cpu::film_grain_cpu(
        w,
        h,
        &quantize_f16(&input),
        &params,
        amount_u(&params, intensity),
        seed,
    );
    let max = max_abs_diff(&gpu, &cpu);
    println!("film_grain_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 3e-3, "film grain diverged from CPU reference: {max}");
    // Sanity: grain at 0.5 actually perturbs the frame (not a silent no-op).
    let q = quantize_f16(&input);
    assert!(
        max_abs_diff(&gpu, &q) > 1e-4,
        "film grain had no visible effect"
    );
}

#[test]
fn letterbox_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::Letterbox.default_params();
    params.aspect = 1.85; // non-default aspect (frame is 1.0 -> bars expected)
    let intensity = 1.0;
    let (gpu, passes) = run_slot(&EffectKind::Letterbox, &params, intensity, 0, &input, w, h);
    assert_eq!(passes, 1, "letterbox is a single dispatch");
    let cpu = cpu::letterbox_cpu(
        w,
        h,
        &quantize_f16(&input),
        &params,
        amount_u(&params, intensity),
    );
    let max = max_abs_diff(&gpu, &cpu);
    println!("letterbox_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 1e-3, "letterbox diverged from CPU reference: {max}");
    // Sanity: bars actually exist — top rows are black, middle rows untouched.
    let q = quantize_f16(&input);
    assert_eq!(
        (gpu[0], gpu[1], gpu[2]),
        (0.0, 0.0, 0.0),
        "top bar not black"
    );
    let mid = ((h / 2 * w + w / 2) * 4) as usize;
    assert!(
        (gpu[mid] - q[mid]).abs() < 1e-6,
        "frame middle should be untouched by 1.85 letterbox"
    );
}

#[test]
fn mirror_flip_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    // Vertical flip at full strength.
    let mut params = EffectKind::MirrorFlip.default_params();
    params.axis = FlipAxis::Vertical;
    let (gpu, passes) = run_slot(&EffectKind::MirrorFlip, &params, 1.0, 0, &input, w, h);
    assert_eq!(passes, 1, "mirror flip is a single dispatch");
    let q = quantize_f16(&input);
    let cpu = cpu::mirror_flip_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("mirror_flip_agrees_with_cpu (vertical): max abs diff = {max}");
    assert!(
        max <= 1e-3,
        "mirror flip diverged from CPU reference: {max}"
    );
    // Spot-check: pixel (5, 7) holds what used to be at (5, h-1-7).
    let a = ((7 * w + 5) * 4) as usize;
    let b = (((h - 1 - 7) * w + 5) * 4) as usize;
    assert_eq!(gpu[a], q[b], "vertical flip sample mismatch");
    assert_eq!(gpu[a + 1], q[b + 1], "vertical flip sample mismatch");

    // Both-axes at half strength exercises the mix path.
    let mut params2 = EffectKind::MirrorFlip.default_params();
    params2.axis = FlipAxis::Both;
    let (gpu2, _) = run_slot(&EffectKind::MirrorFlip, &params2, 0.5, 0, &input, w, h);
    let cpu2 = cpu::mirror_flip_cpu(w, h, &q, &params2, amount_u(&params2, 0.5));
    let max2 = max_abs_diff(&gpu2, &cpu2);
    println!("mirror_flip_agrees_with_cpu (both, 0.5): max abs diff = {max2}");
    assert!(max2 <= 1e-3, "mirror flip (both, 0.5) diverged: {max2}");
}

#[test]
fn zero_strength_does_no_gpu_work() {
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let input = test_frame(32, 32);
    let src = upload_f32_linear(&rc, 32, 32, &input);
    let dst = rc.create_linear_target(32, 32);
    let before = fx.passes_executed();

    // intensity 0 -> bypass.
    let params = EffectKind::Vignette.default_params();
    let applied = fx
        .apply_slot(
            &rc,
            &EffectKind::Vignette,
            &params,
            0.0,
            &src,
            &dst,
            None,
            0,
        )
        .expect("apply_slot");
    assert!(!applied, "zero intensity must bypass");
    assert_eq!(fx.passes_executed(), before, "bypass ran GPU work");

    // amount 0 (with intensity 1) -> bypass too.
    let mut p0 = params.clone();
    p0.amount = 0.0;
    let applied = fx
        .apply_slot(&rc, &EffectKind::Vignette, &p0, 1.0, &src, &dst, None, 0)
        .expect("apply_slot");
    assert!(!applied, "zero amount must bypass");
    assert_eq!(fx.passes_executed(), before, "bypass ran GPU work");

    // Non-zero strength really runs.
    let applied = fx
        .apply_slot(
            &rc,
            &EffectKind::Vignette,
            &params,
            1.0,
            &src,
            &dst,
            None,
            0,
        )
        .expect("apply_slot");
    assert!(applied);
    assert_eq!(fx.passes_executed(), before + 1);
}

#[test]
fn sharpen_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::Sharpen.default_params();
    params.amount = 0.8; // non-default strength
    let (gpu, passes) = run_slot(&EffectKind::Sharpen, &params, 1.0, 0, &input, w, h);
    assert_eq!(passes, 1, "sharpen is a single dispatch");
    let q = quantize_f16(&input);
    let cpu = cpu::sharpen_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("sharpen_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 3e-3, "sharpen diverged from CPU reference: {max}");
    // Sanity: sharpening at 0.8 actually changes the frame.
    assert!(
        max_abs_diff(&gpu, &q) > 1e-4,
        "sharpen had no visible effect"
    );
}

#[test]
fn chromatic_aberration_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::ChromaticAberration.default_params();
    params.radius = 5.0; // non-default (default 8.0)
    params.center = [0.3, 0.7]; // non-default center
    let (gpu, passes) = run_slot(
        &EffectKind::ChromaticAberration,
        &params,
        1.0,
        0,
        &input,
        w,
        h,
    );
    assert_eq!(passes, 1, "chromatic aberration is a single dispatch");
    let q = quantize_f16(&input);
    let cpu = cpu::chromatic_aberration_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("chromatic_aberration_agrees_with_cpu: max abs diff = {max}");
    assert!(
        max <= 1e-3,
        "chromatic aberration diverged from CPU reference: {max}"
    );
}

#[test]
fn pixelate_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::Pixelate.default_params();
    params.radius = 12.0; // non-default (default 16.0)
    let (gpu, passes) = run_slot(&EffectKind::Pixelate, &params, 1.0, 0, &input, w, h);
    assert_eq!(passes, 1, "pixelate is a single dispatch");
    let q = quantize_f16(&input);
    let cpu = cpu::pixelate_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("pixelate_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 1e-3, "pixelate diverged from CPU reference: {max}");
    // Sanity: with 12px blocks the top-left 12x12 block is one flat color.
    let block = gpu[0];
    for yy in 0..12u32 {
        for xx in 0..12u32 {
            let i = ((yy * w + xx) * 4) as usize;
            assert_eq!(gpu[i], block, "pixelate block not flat at ({xx},{yy})");
        }
    }
}

#[test]
fn gaussian_blur_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::GaussianBlur.default_params();
    params.radius = 6.0; // non-default (default 4.0)
    let (gpu, passes) = run_slot(&EffectKind::GaussianBlur, &params, 1.0, 0, &input, w, h);
    assert_eq!(passes, 2, "gaussian blur is two dispatches (H then V)");
    let q = quantize_f16(&input);
    let cpu = cpu::gaussian_blur_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("gaussian_blur_agrees_with_cpu: max abs diff = {max}");
    assert!(
        max <= 3e-3,
        "gaussian blur diverged from CPU reference: {max}"
    );
    // Sanity: the blur changes the frame and smooths it — the checkerboard
    // contrast (local neighbor difference) drops after blurring.
    let before = max_abs_diff(&q, &{
        let mut shifted = q.clone();
        shifted.rotate_right(4);
        shifted
    });
    let after = max_abs_diff(&gpu, &{
        let mut shifted = gpu.clone();
        shifted.rotate_right(4);
        shifted
    });
    assert!(
        max_abs_diff(&gpu, &q) > 1e-3,
        "gaussian blur had no visible effect"
    );
    assert!(
        after < before,
        "blurred frame should be smoother than the input ({after} !< {before})"
    );
}

#[test]
fn bloom_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::Bloom.default_params();
    params.radius = 8.0; // non-default (default 6.0)
    params.threshold = 0.6; // non-default (default 0.8)
    params.softness = 0.8; // non-default (default 0.5)
    let (gpu, passes) = run_slot(&EffectKind::Bloom, &params, 1.0, 0, &input, w, h);
    assert_eq!(passes, 2, "bloom is two dispatches (H then V+composite)");
    let q = quantize_f16(&input);
    let cpu = cpu::bloom_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("bloom_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 3e-3, "bloom diverged from CPU reference: {max}");
    // Sanity: bloom is purely additive on rgb and leaves alpha untouched.
    for (i, (&g, &qq)) in gpu.iter().zip(q.iter()).enumerate() {
        if i % 4 == 3 {
            assert_eq!(g, qq, "bloom must not touch alpha");
        } else {
            assert!(
                g + 1e-3 >= qq,
                "bloom must not darken rgb (pixel {i}: {g} < {qq})"
            );
        }
    }
}

#[test]
fn lens_correction_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::LensCorrection.default_params();
    params.k1 = 0.2; // non-default (default 0.0)
    params.k2 = -0.05; // non-default (default 0.0)
    let (gpu, passes) = run_slot(&EffectKind::LensCorrection, &params, 1.0, 0, &input, w, h);
    assert_eq!(passes, 1, "lens correction is a single dispatch");
    let q = quantize_f16(&input);
    let cpu = cpu::lens_correction_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("lens_correction_agrees_with_cpu: max abs diff = {max}");
    assert!(
        max <= 1e-3,
        "lens correction diverged from CPU reference: {max}"
    );
    // Sanity: with k1 != 0 the corners move (frame actually changes).
    assert!(
        max_abs_diff(&gpu, &q) > 1e-4,
        "lens correction had no visible effect"
    );
}

#[test]
fn crop_zoom_pan_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let mut params = EffectKind::CropZoomPan.default_params();
    params.zoom = 2.0; // non-default (default 1.5)
    params.center = [0.4, 0.6]; // non-default center
    let (gpu, passes) = run_slot(&EffectKind::CropZoomPan, &params, 1.0, 0, &input, w, h);
    assert_eq!(passes, 1, "crop/zoom/pan is a single dispatch");
    let q = quantize_f16(&input);
    let cpu = cpu::crop_zoom_pan_cpu(w, h, &q, &params, amount_u(&params, 1.0));
    let max = max_abs_diff(&gpu, &cpu);
    println!("crop_zoom_pan_agrees_with_cpu: max abs diff = {max}");
    assert!(
        max <= 1e-3,
        "crop/zoom/pan diverged from CPU reference: {max}"
    );
}

#[test]
fn ai_kinds_do_no_gpu_work() {
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let input = test_frame(32, 32);
    let src = upload_f32_linear(&rc, 32, 32, &input);
    let dst = rc.create_linear_target(32, 32);
    let before = fx.passes_executed();
    for kind in [
        EffectKind::BackgroundBlur,
        EffectKind::BackgroundReplace,
        EffectKind::SkinSmooth,
    ] {
        let params = kind.default_params();
        let applied = fx
            .apply_slot(&rc, &kind, &params, 1.0, &src, &dst, None, 0)
            .expect("apply_slot");
        assert!(!applied, "{kind:?} is AI and must bypass GPU");
    }
    assert_eq!(fx.passes_executed(), before, "AI bypass ran GPU work");
}

/// Variance of the rgb channels pooled over a pixel region (f64
/// accumulation for stability).
fn region_variance(frame: &[f32], w: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> f64 {
    let mut n = 0u64;
    let mut sum = 0.0f64;
    let mut sum2 = 0.0f64;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                let v = frame[i + c] as f64;
                n += 1;
                sum += v;
                sum2 += v * v;
            }
        }
    }
    let n = n as f64;
    (sum2 - sum * sum / n) / n
}

/// 4px linear-feather person mask: 1 inside the rect, 0 outside.
fn feathered_rect_mask(w: u32, h: u32, rx0: u32, ry0: u32, rx1: u32, ry1: u32) -> Vec<f32> {
    let mut mask = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let dx = if x < rx0 {
                (rx0 - x) as f32
            } else if x >= rx1 {
                (x - (rx1 - 1)) as f32
            } else {
                0.0
            };
            let dy = if y < ry0 {
                (ry0 - y) as f32
            } else if y >= ry1 {
                (y - (ry1 - 1)) as f32
            } else {
                0.0
            };
            let d = (dx * dx + dy * dy).sqrt();
            let m = (1.0 - d / 4.0).clamp(0.0, 1.0);
            mask.extend_from_slice(&[m, 0.0, 0.0, 1.0]);
        }
    }
    mask
}

#[test]
fn background_blur_agrees_with_cpu() {
    let (w, h) = (64, 64);
    // "Person" rect: high-variance checkerboard (4px cells); background:
    // 2px vertical stripes (high variance, different pattern).
    let (rx0, ry0, rx1, ry1) = (20u32, 16u32, 44u32, 48u32);
    let mut sharp = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let in_rect = x >= rx0 && x < rx1 && y >= ry0 && y < ry1;
            let c = if in_rect {
                0.15 + (((x / 4) + (y / 4)) % 2) as f32 * 0.6
            } else {
                0.55 + ((x / 2) % 2) as f32 * 0.6
            };
            sharp.extend_from_slice(&[c, c * 0.9, c * 0.8, 1.0]);
        }
    }
    let mask = feathered_rect_mask(w, h, rx0, ry0, rx1, ry1);

    let params = EffectKind::BackgroundBlur.default_params(); // amount 1, radius 4
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let sharp_tex = upload_f32_linear(&rc, w, h, &sharp);
    let mask_tex = upload_f32_linear(&rc, w, h, &mask);
    let dst = rc.create_linear_target(w, h);
    let before = fx.passes_executed();
    let applied = fx
        .apply_background_blur(&rc, &params, 1.0, &sharp_tex, &mask_tex, &dst)
        .expect("apply_background_blur");
    assert!(applied, "bg blur at full strength must run");
    assert_eq!(
        fx.passes_executed(),
        before + 3,
        "bg blur = 2 blur passes + 1 mix"
    );
    let gpu = download_f32_linear(&rc, &dst);

    // CPU reference: full-strength gaussian into a quantized intermediate,
    // then the masked mix (mirrors the GPU's two-stage pipeline).
    let q = quantize_f16(&sharp);
    let mq = quantize_f16(&mask);
    let mut bp = params.clone();
    bp.amount = 1.0;
    let blur = cpu::gaussian_blur_cpu(w, h, &q, &bp, 1.0);
    let blur_q = quantize_f16(&blur);
    let expected = cpu::masked_mix_cpu(w, h, &blur_q, &q, &mq, 1.0);
    let max = max_abs_diff(&gpu, &expected);
    println!("background_blur_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 3e-3, "bg blur diverged from CPU reference: {max}");

    // Background variance drops by >= 50% (corner region, away from edges).
    let var_in = region_variance(&q, w, 4, 4, 14, 14);
    let var_out = region_variance(&gpu, w, 4, 4, 14, 14);
    println!("bg blur: bg variance {var_in} -> {var_out}");
    assert!(
        var_out <= 0.5 * var_in,
        "bg not blurred enough: {var_out} !<= 0.5 * {var_in}"
    );

    // Person-region variance preserved within 5% (mask = 1 in the interior).
    let pvar_in = region_variance(&q, w, rx0 + 6, ry0 + 6, rx1 - 6, ry1 - 6);
    let pvar_out = region_variance(&gpu, w, rx0 + 6, ry0 + 6, rx1 - 6, ry1 - 6);
    println!("bg blur: person variance {pvar_in} -> {pvar_out}");
    assert!(
        (pvar_out - pvar_in).abs() <= 0.05 * pvar_in,
        "person region changed: {pvar_out} vs {pvar_in}"
    );

    // Edge band: no binary seam — the transition row carries strictly
    // interior values (between sharp and fully-blurred).
    let y = (ry0 + ry1) / 2;
    let mut interior = 0;
    for x in rx1..rx1 + 4 {
        let i = ((y * w + x) * 4) as usize;
        let (o, s, b) = (gpu[i], q[i], blur_q[i]);
        if (o - s).abs() > 5e-3 && (o - b).abs() > 5e-3 {
            interior += 1;
        }
    }
    assert!(
        interior >= 2,
        "edge band shows a binary seam (interior={interior})"
    );
}

#[test]
fn background_replace_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let input = test_frame(w, h);
    let (rx0, ry0, rx1, ry1) = (20u32, 16u32, 44u32, 48u32);
    // Hard person mask for the solid-plate properties.
    let mut mask = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let m = if x >= rx0 && x < rx1 && y >= ry0 && y < ry1 {
                1.0
            } else {
                0.0
            };
            mask.extend_from_slice(&[m, 0.0, 0.0, 1.0]);
        }
    }
    let mut params = EffectKind::BackgroundReplace.default_params();
    params.color = [0.2, 0.4, 0.8];
    params.spill = 0.5;

    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let src_tex = upload_f32_linear(&rc, w, h, &input);
    let mask_tex = upload_f32_linear(&rc, w, h, &mask);
    // Solid-color plate: 1x1 texture carrying params.color.
    let plate_tex = upload_f32_linear(&rc, 1, 1, &[0.2, 0.4, 0.8, 1.0]);
    let dst = rc.create_linear_target(w, h);
    let before = fx.passes_executed();
    let applied = fx
        .apply_background_replace(&rc, &params, 1.0, &src_tex, &mask_tex, &plate_tex, &dst)
        .expect("apply_background_replace");
    assert!(applied, "bg replace at full strength must run");
    assert_eq!(
        fx.passes_executed(),
        before + 1,
        "bg replace is a single dispatch"
    );
    let gpu = download_f32_linear(&rc, &dst);

    let q = quantize_f16(&input);
    let mq = quantize_f16(&mask);
    let plate_q = quantize_f16(&[0.2, 0.4, 0.8, 1.0]);
    let expected = cpu::bg_replace_cpu(w, h, &q, &plate_q, 1, 1, &mq, &params, 1.0);
    let max = max_abs_diff(&gpu, &expected);
    println!("background_replace_agrees_with_cpu (solid plate): max abs diff = {max}");
    assert!(max <= 3e-3, "bg replace diverged from CPU reference: {max}");

    // Background pixels == plate color (+-1/255).
    let plate_c = quantize_f16(&[0.2f32, 0.4, 0.8]);
    for y in 0..8u32 {
        for x in 0..8u32 {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                assert!(
                    (gpu[i + c] - plate_c[c]).abs() <= 1.0 / 255.0,
                    "bg pixel not plate color at ({x},{y})"
                );
            }
        }
    }
    // Person pixels bit-identical to the source (m = 1, edge factor 0).
    let mut person_max = 0.0f32;
    for y in (ry0 + 2)..(ry1 - 2) {
        for x in (rx0 + 2)..(rx1 - 2) {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..4 {
                person_max = person_max.max((gpu[i + c] - q[i + c]).abs());
            }
        }
    }
    assert_eq!(person_max, 0.0, "person pixels must be bit-identical");

    // Image plate: 4x4 test image tiled over the frame, feathered mask
    // (exercises the spill-suppression path).
    let mut plate_img = Vec::with_capacity(4 * 4 * 4);
    for y in 0..4u32 {
        for x in 0..4u32 {
            let (r, g, b) = if (x + y) % 2 == 0 {
                (0.9, 0.1, 0.1)
            } else {
                (0.1, 0.1, 0.9)
            };
            plate_img.extend_from_slice(&[r, g, b, 1.0]);
        }
    }
    let mask_soft = feathered_rect_mask(w, h, rx0, ry0, rx1, ry1);
    let plate_img_tex = upload_f32_linear(&rc, 4, 4, &plate_img);
    let mask_soft_tex = upload_f32_linear(&rc, w, h, &mask_soft);
    let dst2 = rc.create_linear_target(w, h);
    let applied = fx
        .apply_background_replace(
            &rc,
            &params,
            1.0,
            &src_tex,
            &mask_soft_tex,
            &plate_img_tex,
            &dst2,
        )
        .expect("apply_background_replace (image plate)");
    assert!(applied);
    let gpu2 = download_f32_linear(&rc, &dst2);
    let plate_img_q = quantize_f16(&plate_img);
    let msq = quantize_f16(&mask_soft);
    let expected2 = cpu::bg_replace_cpu(w, h, &q, &plate_img_q, 4, 4, &msq, &params, 1.0);
    let max2 = max_abs_diff(&gpu2, &expected2);
    println!("background_replace_agrees_with_cpu (image plate): max abs diff = {max2}");
    assert!(max2 <= 3e-3, "bg replace (image plate) diverged: {max2}");
    // Background really is replaced (differs from the source)...
    let mut bg_diff = 0.0f32;
    for y in 0..8u32 {
        for x in 0..8u32 {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                bg_diff = bg_diff.max((gpu2[i + c] - q[i + c]).abs());
            }
        }
    }
    assert!(bg_diff > 1e-2, "image plate did not replace the background");
    // ...and the person interior is untouched.
    let mut person_max2 = 0.0f32;
    for y in (ry0 + 6)..(ry1 - 6) {
        for x in (rx0 + 6)..(rx1 - 6) {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..4 {
                person_max2 = person_max2.max((gpu2[i + c] - q[i + c]).abs());
            }
        }
    }
    assert_eq!(person_max2, 0.0, "person pixels must be bit-identical");
}

#[test]
fn skin_smooth_agrees_with_cpu() {
    let (w, h) = (64, 64);
    // Left half: skin tone (200,150,120)/255 + noise; right half: blue + noise.
    let mut src = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = if x < w / 2 {
                (200.0 / 255.0, 150.0 / 255.0, 120.0 / 255.0)
            } else {
                (0.1, 0.2, 0.9)
            };
            // Deterministic noise, +-0.03.
            let n = (((x * 73 + y * 149) % 17) as f32 / 16.0 - 0.5) * 0.06;
            src.extend_from_slice(&[r + n, g + n, b + n, 1.0]);
        }
    }
    // Skin mask: 1 on the left, 0 on the right, 4px linear ramp at x=30..34.
    let mut mask = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for x in 0..w {
            let m = if x < 30 {
                1.0
            } else if x >= 34 {
                0.0
            } else {
                (34 - x) as f32 / 4.0
            };
            mask.extend_from_slice(&[m, 0.0, 0.0, 1.0]);
        }
    }
    let mut params = EffectKind::SkinSmooth.default_params();
    params.amount = 0.9; // radius stays 0 -> 2.0 fallback

    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let src_tex = upload_f32_linear(&rc, w, h, &src);
    let mask_tex = upload_f32_linear(&rc, w, h, &mask);
    let dst = rc.create_linear_target(w, h);
    let before = fx.passes_executed();
    let applied = fx
        .apply_skin_smooth(&rc, &params, 1.0, &src_tex, &mask_tex, &dst)
        .expect("apply_skin_smooth");
    assert!(applied, "skin smooth at 0.9 must run");
    assert_eq!(
        fx.passes_executed(),
        before + 3,
        "skin smooth = 2 blur + 1 mix"
    );
    let gpu = download_f32_linear(&rc, &dst);

    let q = quantize_f16(&src);
    let mq = quantize_f16(&mask);
    let mut bp = params.clone();
    bp.amount = 1.0;
    bp.radius = 2.0; // the fallback the GPU applies
    let blur = cpu::gaussian_blur_cpu(w, h, &q, &bp, 1.0);
    let blur_q = quantize_f16(&blur);
    // a = src, b = blurred: the skin mask selects the blurred pixels.
    let expected = cpu::masked_mix_cpu(w, h, &q, &blur_q, &mq, 0.9);
    let max = max_abs_diff(&gpu, &expected);
    println!("skin_smooth_agrees_with_cpu: max abs diff = {max}");
    assert!(
        max <= 3e-3,
        "skin smooth diverged from CPU reference: {max}"
    );

    // Skin patch local variance reduced; blue patch untouched (0 diff).
    let svar_in = region_variance(&q, w, 4, 8, 24, 56);
    let svar_out = region_variance(&gpu, w, 4, 8, 24, 56);
    println!("skin smooth: skin variance {svar_in} -> {svar_out}");
    assert!(svar_out < svar_in, "skin patch was not smoothed");
    let mut blue_max = 0.0f32;
    for y in 8..56u32 {
        for x in 38..60u32 {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..4 {
                blue_max = blue_max.max((gpu[i + c] - q[i + c]).abs());
            }
        }
    }
    assert_eq!(blue_max, 0.0, "blue patch must be untouched");

    // Mask edge: strictly interior blend values (no binary seam).
    let mut seam_free = false;
    for y in 8..56u32 {
        for x in 30..34u32 {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                let (o, s, b) = (gpu[i + c], q[i + c], blur_q[i + c]);
                if (o - s) * (o - b) < -1e-10 {
                    seam_free = true;
                }
            }
        }
    }
    assert!(seam_free, "mask edge shows a binary seam");
}

#[test]
fn gen_region_mask_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let rc = ctx();
    let fx = EffectPass::new(&rc).expect("effect pass");

    // Circle through the MaskRef path.
    let dst = rc.create_linear_target(w, h);
    let before = fx.passes_executed();
    fx.gen_region_mask(
        &rc,
        &MaskRef::Circle {
            x: 0.5,
            y: 0.5,
            radius: 0.25,
            feather: 0.05,
        },
        &dst,
    )
    .expect("gen_region_mask circle");
    assert_eq!(
        fx.passes_executed(),
        before + 1,
        "mask gen is a single dispatch"
    );
    let gpu = download_f32_linear(&rc, &dst);
    let cpu = cpu::gen_region_mask_cpu(w, h, 1, [0.5, 0.5, 0.25, 0.0], 0.05);
    let max = max_abs_diff(&gpu, &cpu);
    println!("gen_region_mask_agrees_with_cpu (circle): max abs diff = {max}");
    assert!(
        max <= 1.0 / 255.0,
        "circle mask diverged from CPU reference: {max}"
    );
    assert!(
        gpu.iter().step_by(4).any(|&v| v > 0.0 && v < 1.0),
        "feather must produce interior values"
    );
    for px in gpu.chunks_exact(4) {
        assert_eq!(px[1], 0.0, "mask g must be 0");
        assert_eq!(px[2], 0.0, "mask b must be 0");
        assert_eq!(px[3], 1.0, "mask a must be 1");
    }

    // Rectangle through the MaskRef path.
    let dst2 = rc.create_linear_target(w, h);
    fx.gen_region_mask(
        &rc,
        &MaskRef::Rectangle {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
            feather: 0.05,
        },
        &dst2,
    )
    .expect("gen_region_mask rectangle");
    let gpu2 = download_f32_linear(&rc, &dst2);
    let cpu2 = cpu::gen_region_mask_cpu(w, h, 2, [0.25, 0.25, 0.5, 0.5], 0.05);
    let max2 = max_abs_diff(&gpu2, &cpu2);
    println!("gen_region_mask_agrees_with_cpu (rectangle): max abs diff = {max2}");
    assert!(
        max2 <= 1.0 / 255.0,
        "rectangle mask diverged from CPU reference: {max2}"
    );
    assert!(
        gpu2.iter().step_by(4).any(|&v| v > 0.0 && v < 1.0),
        "feather must produce interior values"
    );

    // Person masks are never generated here (the engine supplies them).
    let err = fx
        .gen_region_mask(&rc, &MaskRef::Person, &dst)
        .expect_err("Person must not generate");
    assert!(
        matches!(err, RenderError::SizeMismatch),
        "unexpected error for Person: {err:?}"
    );
}

#[test]
fn masked_mix_agrees_with_cpu() {
    let (w, h) = (64, 64);
    let a = test_frame(w, h);
    // b: inverted frame; mask: horizontal gradient.
    let mut b = Vec::with_capacity((w * h * 4) as usize);
    let mut mask = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            b.extend_from_slice(&[1.0 - a[i], 1.0 - a[i + 1], 1.0 - a[i + 2], a[i + 3]]);
            let m = x as f32 / (w - 1) as f32;
            mask.extend_from_slice(&[m, 0.0, 0.0, 1.0]);
        }
    }
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let a_tex = upload_f32_linear(&rc, w, h, &a);
    let b_tex = upload_f32_linear(&rc, w, h, &b);
    let mask_tex = upload_f32_linear(&rc, w, h, &mask);
    let dst = rc.create_linear_target(w, h);
    let before = fx.passes_executed();
    fx.masked_mix(&rc, 0.65, &a_tex, &b_tex, &mask_tex, &dst)
        .expect("masked_mix");
    assert_eq!(
        fx.passes_executed(),
        before + 1,
        "masked mix is a single dispatch"
    );
    let gpu = download_f32_linear(&rc, &dst);
    let cpu = cpu::masked_mix_cpu(
        w,
        h,
        &quantize_f16(&a),
        &quantize_f16(&b),
        &quantize_f16(&mask),
        0.65,
    );
    let max = max_abs_diff(&gpu, &cpu);
    println!("masked_mix_agrees_with_cpu: max abs diff = {max}");
    assert!(
        max <= 1.0 / 255.0,
        "masked mix diverged from CPU reference: {max}"
    );
}

#[test]
fn c7_zero_strength_does_no_gpu_work() {
    let rc = ctx();
    let mut fx = EffectPass::new(&rc).expect("effect pass");
    let input = test_frame(32, 32);
    let src = upload_f32_linear(&rc, 32, 32, &input);
    let mask = upload_f32_linear(&rc, 32, 32, &vec![1.0f32; 32 * 32 * 4]);
    let plate = upload_f32_linear(&rc, 1, 1, &[0.2, 0.4, 0.8, 1.0]);
    let dst = rc.create_linear_target(32, 32);
    let before = fx.passes_executed();

    // amount 0 -> bypass for all three C7 methods.
    let mut p0 = EffectKind::BackgroundBlur.default_params();
    p0.amount = 0.0;
    assert!(
        !fx.apply_background_blur(&rc, &p0, 1.0, &src, &mask, &dst)
            .expect("bg blur bypass"),
        "zero amount must bypass bg blur"
    );
    let mut p1 = EffectKind::BackgroundReplace.default_params();
    p1.amount = 0.0;
    assert!(
        !fx.apply_background_replace(&rc, &p1, 1.0, &src, &mask, &plate, &dst)
            .expect("bg replace bypass"),
        "zero amount must bypass bg replace"
    );
    let mut p2 = EffectKind::SkinSmooth.default_params();
    p2.amount = 0.0;
    assert!(
        !fx.apply_skin_smooth(&rc, &p2, 1.0, &src, &mask, &dst)
            .expect("skin smooth bypass"),
        "zero amount must bypass skin smooth"
    );
    // intensity 0 -> bypass too.
    let p3 = EffectKind::BackgroundBlur.default_params();
    assert!(
        !fx.apply_background_blur(&rc, &p3, 0.0, &src, &mask, &dst)
            .expect("bg blur bypass"),
        "zero intensity must bypass bg blur"
    );
    assert_eq!(fx.passes_executed(), before, "bypass ran GPU work");
}
