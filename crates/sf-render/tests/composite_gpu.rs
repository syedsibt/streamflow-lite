//! Real headless-GPU tests for the alpha-compositing pass (lavapipe, no mocks).
//!
//! Every test runs the actual WGSL compute shader on a real (software) GPU
//! and checks the blended pixels against the src-over-dst math the shader
//! implements. This validates the 4-binding (dst/src sampled, out storage
//! write) API end to end.

use sf_render::{CompositePass, RenderContext};

/// Headless context + composite pass. Forces the lavapipe Vulkan ICD so the
/// tests run on any machine without a physical GPU.
fn ctx() -> (RenderContext, CompositePass) {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let rc = RenderContext::new_headless().expect("headless render context");
    let cp = CompositePass::new(&rc).expect("composite pass");
    (rc, cp)
}

fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
    let mut v = Vec::with_capacity(w as usize * h as usize * 4);
    for _ in 0..w * h {
        v.extend_from_slice(&px);
    }
    v
}

fn view(tex: &wgpu::Texture) -> wgpu::TextureView {
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

/// |got - expected| <= 1 per channel (unorm rounding).
fn assert_px_near(got: &[u8], x: u32, y: u32, w: u32, expected: [u8; 4]) {
    let i = ((y * w + x) * 4) as usize;
    let got_px = [got[i], got[i + 1], got[i + 2], got[i + 3]];
    for c in 0..4 {
        let d = (got_px[c] as i16 - expected[c] as i16).unsigned_abs();
        assert!(
            d <= 1,
            "pixel ({x},{y}) channel {c}: got {got_px:?}, expected {expected:?}"
        );
    }
}

#[test]
fn composite_blends_src_over_dst() {
    let (rc, cp) = ctx();
    // dst: opaque blue. src: half-transparent red.
    let dst = rc.upload_rgba8(64, 64, &solid(64, 64, [0, 0, 255, 255])).unwrap();
    let src = rc.upload_rgba8(64, 64, &solid(64, 64, [255, 0, 0, 128])).unwrap();
    let out = rc.create_rgba8_target(64, 64);

    // Blend the 32x32 top-left of src at dst offset (16,16), full opacity.
    cp.composite(
        &rc,
        &view(&dst),
        &view(&src),
        &view(&out),
        64,
        64,
        (0, 0, 32, 32),
        (16, 16),
        1.0,
    )
    .unwrap();

    let px = rc.download_rgba8(&out).unwrap();

    // Outside the src rect: untouched destination (blue).
    assert_px_near(&px, 0, 0, 64, [0, 0, 255, 255]);
    assert_px_near(&px, 63, 63, 64, [0, 0, 255, 255]);
    assert_px_near(&px, 15, 15, 64, [0, 0, 255, 255]);
    assert_px_near(&px, 48, 16, 64, [0, 0, 255, 255]);

    // Inside: src-over-dst. sa = 128/255, da = 1, oa = 1.
    // rgb = src.rgb*sa + dst.rgb*(1-sa) = (128, 0, 127), a = 255.
    assert_px_near(&px, 16, 16, 64, [128, 0, 127, 255]);
    assert_px_near(&px, 47, 47, 64, [128, 0, 127, 255]);
    assert_px_near(&px, 32, 32, 64, [128, 0, 127, 255]);
}

#[test]
fn composite_zero_opacity_keeps_dst() {
    let (rc, cp) = ctx();
    let dst = rc.upload_rgba8(32, 32, &solid(32, 32, [10, 20, 30, 255])).unwrap();
    let src = rc.upload_rgba8(32, 32, &solid(32, 32, [200, 210, 220, 255])).unwrap();
    let out = rc.create_rgba8_target(32, 32);

    cp.composite(
        &rc,
        &view(&dst),
        &view(&src),
        &view(&out),
        32,
        32,
        (0, 0, 32, 32),
        (0, 0),
        0.0,
    )
    .unwrap();

    let px = rc.download_rgba8(&out).unwrap();
    assert_px_near(&px, 0, 0, 32, [10, 20, 30, 255]);
    assert_px_near(&px, 31, 31, 32, [10, 20, 30, 255]);
    assert_px_near(&px, 17, 5, 32, [10, 20, 30, 255]);
}
