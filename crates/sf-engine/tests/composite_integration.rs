//! Engine-level GPU compositing test: real headless GPU end to end.
//!
//! Exercises `Engine::gpu_composite_over` (the production program-bus
//! composite path) on lavapipe: blend math, pooled-texture reuse across
//! frames, resize recreation, and the size-mismatch error.

use sf_engine::Engine;

fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
    let mut v = Vec::with_capacity(w as usize * h as usize * 4);
    for _ in 0..w * h {
        v.extend_from_slice(&px);
    }
    v
}

fn px_at(px: &[u8], x: u32, y: u32, w: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [px[i], px[i + 1], px[i + 2], px[i + 3]]
}

fn assert_px_near(got: [u8; 4], expected: [u8; 4], what: &str) {
    for c in 0..4 {
        let d = (got[c] as i16 - expected[c] as i16).unsigned_abs();
        assert!(d <= 1, "{what} channel {c}: got {got:?}, expected {expected:?}");
    }
}

#[test]
fn gpu_composite_over_blends_and_pools() {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let mut engine = Engine::new_headless().expect("headless engine");

    // dst: opaque blue canvas; src: half-transparent red frame.
    let canvas = solid(64, 64, [0, 0, 255, 255]);
    let src = solid(64, 64, [255, 0, 0, 128]);

    // First call: allocates the pooled out texture.
    let out1 = engine
        .gpu_composite_over(&canvas, &src, 64, 64)
        .expect("gpu composite");
    assert_eq!(out1.len(), 64 * 64 * 4);
    // sa = 128/255, oa = 1: rgb = (128, 0, 127), a = 255.
    assert_px_near(px_at(&out1, 10, 10, 64), [128, 0, 127, 255], "blended pixel");
    assert_px_near(px_at(&out1, 63, 63, 64), [128, 0, 127, 255], "corner pixel");

    // Second call at the same size: reuses the pooled texture, same result.
    let out2 = engine
        .gpu_composite_over(&canvas, &src, 64, 64)
        .expect("gpu composite reuse");
    assert_eq!(out1, out2, "pooled composite must be deterministic");

    // Different size: pool recreates, still correct.
    let canvas32 = solid(32, 32, [0, 0, 255, 255]);
    let src32 = solid(32, 32, [255, 0, 0, 128]);
    let out3 = engine
        .gpu_composite_over(&canvas32, &src32, 32, 32)
        .expect("gpu composite resized");
    assert_eq!(out3.len(), 32 * 32 * 4);
    assert_px_near(px_at(&out3, 5, 5, 32), [128, 0, 127, 255], "resized pixel");

    // Opaque src fully covers dst.
    let opaque = solid(64, 64, [10, 20, 30, 255]);
    let out4 = engine
        .gpu_composite_over(&canvas, &opaque, 64, 64)
        .expect("opaque composite");
    assert_px_near(px_at(&out4, 0, 0, 64), [10, 20, 30, 255], "opaque pixel");

    // Size mismatch is a clean error (caller falls back to CPU).
    assert!(
        engine.gpu_composite_over(&canvas, &src32, 64, 64).is_err(),
        "mismatched buffers must fail"
    );
}
