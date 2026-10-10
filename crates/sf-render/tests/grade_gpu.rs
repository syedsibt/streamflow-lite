//! Real headless-GPU tests for the fused grade pass (lavapipe, no mocks).
//!
//! Every test runs the actual WGSL shaders on a real (software) GPU and
//! compares against the CPU reference in `sf_color::cpu`, which is the exact
//! math contract both sides implement.

use sf_color::cpu::apply_grade_cpu;
use sf_color::grade::{ColorGrade, LutPosition, LutRef};
use sf_color::lut::parse_cube;
use sf_render::{GradePass, RenderContext};

/// Headless context + grade pass. Forces the lavapipe Vulkan ICD so the
/// tests run on any machine without a physical GPU.
fn ctx() -> (RenderContext, GradePass) {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let rc = RenderContext::new_headless().expect("headless render context");
    let gp = GradePass::new(&rc).expect("grade pass");
    (rc, gp)
}

/// 64x64 RGBA gradient: r = x*4, g = y*4, b = (x+y)*2 (all < 256), a = 255.
fn gradient64() -> Vec<u8> {
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

fn to_f32(px: &[u8]) -> Vec<[f32; 3]> {
    px.chunks_exact(4)
        .map(|c| {
            [
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
            ]
        })
        .collect()
}

fn max_abs_diff(got: &[u8], expected: &[[f32; 3]]) -> u8 {
    let mut max = 0u8;
    for (i, pix) in expected.iter().enumerate() {
        for c in 0..3 {
            let e = (pix[c] * 255.0).round().clamp(0.0, 255.0) as u8;
            let d = (got[i * 4 + c] as i16 - e as i16).unsigned_abs() as u8;
            max = max.max(d);
        }
    }
    max
}

#[test]
fn identity_pass_skipped() {
    let (rc, mut gp) = ctx();
    let px = gradient64();
    let src = rc.upload_rgba8(64, 64, &px).unwrap();
    let dst = rc.create_linear_target(64, 64);
    let grade = ColorGrade::default();
    let baked = grade.curves.bake();
    // The identity grade must not touch the GPU at all.
    assert_eq!(
        gp.apply(&rc, &grade, &baked, None, &src, &dst).unwrap(),
        false
    );
}

#[test]
fn upload_download_roundtrip() {
    let (rc, _gp) = ctx();
    let px = gradient64();
    let tex = rc.upload_rgba8(64, 64, &px).unwrap();
    let back = rc.download_rgba8(&tex).unwrap();
    assert_eq!(back, px, "upload -> download must be byte-exact");
}

#[test]
fn exposure_matches_cpu() {
    let (rc, mut gp) = ctx();
    let px = gradient64();
    let mut grade = ColorGrade::default();
    grade.exposure = 1.0;
    let baked = grade.curves.bake();

    let src = rc.upload_rgba8(64, 64, &px).unwrap();
    let linear = rc.create_linear_target(64, 64);
    assert!(gp.apply(&rc, &grade, &baked, None, &src, &linear).unwrap());
    let out = rc.create_rgba8_target(64, 64);
    gp.encode_output(&rc, 0.0, &linear, &out).unwrap();
    let got = rc.download_rgba8(&out).unwrap();

    let cpu: Vec<[f32; 3]> = to_f32(&px)
        .iter()
        .map(|&p| apply_grade_cpu(&grade, &baked, None, p))
        .collect();
    let max = max_abs_diff(&got, &cpu);
    println!("exposure_matches_cpu: max abs diff = {max}/255");
    assert!(max <= 2, "GPU diverged from CPU: max diff {max}/255");
}

/// Builds a 17^3 identity `.cube` in code (output == input lattice position).
fn identity_cube_17() -> String {
    let mut s = String::from("TITLE \"identity 17\"\nLUT_3D_SIZE 17\n");
    for b in 0..17 {
        for g in 0..17 {
            for r in 0..17 {
                s.push_str(&format!(
                    "{} {} {}\n",
                    r as f32 / 16.0,
                    g as f32 / 16.0,
                    b as f32 / 16.0
                ));
            }
        }
    }
    s
}

#[test]
fn lut_identity_matches_cpu() {
    let (rc, mut gp) = ctx();
    let px = gradient64();
    let cube = parse_cube(&identity_cube_17()).expect("identity cube parses");
    assert_eq!(cube.size, 33);

    let mut grade = ColorGrade::default();
    grade.lut = Some(LutRef {
        file: String::new(),
        intensity: 1.0,
        position: LutPosition::After,
    });
    let baked = grade.curves.bake();

    let src = rc.upload_rgba8(64, 64, &px).unwrap();
    let linear = rc.create_linear_target(64, 64);
    assert!(gp
        .apply(&rc, &grade, &baked, Some(&cube), &src, &linear)
        .unwrap());
    let out = rc.create_rgba8_target(64, 64);
    gp.encode_output(&rc, 0.0, &linear, &out).unwrap();
    let got = rc.download_rgba8(&out).unwrap();

    // An identity LUT is a (near) passthrough: compare against the CPU
    // reference with the LUT applied, and against the raw input.
    let cpu: Vec<[f32; 3]> = to_f32(&px)
        .iter()
        .map(|&p| apply_grade_cpu(&grade, &baked, Some(&cube), p))
        .collect();
    let max_cpu = max_abs_diff(&got, &cpu);
    let max_input = max_abs_diff(&got, &to_f32(&px));
    println!(
        "lut_identity_matches_cpu: max diff vs CPU = {max_cpu}/255, vs input = {max_input}/255"
    );
    assert!(
        max_cpu <= 4,
        "GPU diverged from CPU LUT reference: {max_cpu}/255"
    );
    assert!(
        max_input <= 4,
        "identity LUT changed the image: {max_input}/255"
    );
}

#[test]
fn curves_match_cpu() {
    let (rc, mut gp) = ctx();
    let px = gradient64();
    let mut grade = ColorGrade::default();
    // Lift the mids on master, pull red down slightly: exercises the
    // per-channel + master textureLoad path and the CPU<->GPU curve upload.
    grade
        .curves
        .master
        .set_points(vec![[0.0, 0.0], [0.5, 0.65], [1.0, 1.0]])
        .unwrap();
    grade
        .curves
        .r
        .set_points(vec![[0.0, 0.0], [1.0, 0.9]])
        .unwrap();
    assert!(!grade.is_identity());
    let baked = grade.curves.bake();

    let src = rc.upload_rgba8(64, 64, &px).unwrap();
    let linear = rc.create_linear_target(64, 64);
    assert!(gp.apply(&rc, &grade, &baked, None, &src, &linear).unwrap());
    let out = rc.create_rgba8_target(64, 64);
    gp.encode_output(&rc, 0.0, &linear, &out).unwrap();
    let got = rc.download_rgba8(&out).unwrap();

    let cpu: Vec<[f32; 3]> = to_f32(&px)
        .iter()
        .map(|&p| apply_grade_cpu(&grade, &baked, None, p))
        .collect();
    let max = max_abs_diff(&got, &cpu);
    println!("curves_match_cpu: max abs diff = {max}/255");
    assert!(max <= 3, "GPU curves diverged from CPU: max diff {max}/255");
}

#[test]
fn grade_pass_caches_per_grade_resources() {
    let (rc, mut gp) = ctx();
    let px = gradient64();
    let mut grade = ColorGrade::default();
    grade.exposure = 0.5;
    let baked = grade.curves.bake();
    let src = rc.upload_rgba8(64, 64, &px).unwrap();
    let dst = rc.create_linear_target(64, 64);

    // First apply: all three resources are built.
    assert!(gp.apply(&rc, &grade, &baked, None, &src, &dst).unwrap());
    assert_eq!(gp.cache_stats(), (0, 3), "first apply builds everything");

    // Identical apply: full cache hit, no rebuilds.
    assert!(gp.apply(&rc, &grade, &baked, None, &src, &dst).unwrap());
    assert_eq!(
        gp.cache_stats(),
        (3, 3),
        "identical apply must hit the cache"
    );

    // One param changes: only the uniform buffer rebuilds.
    grade.exposure = 1.0;
    assert!(gp.apply(&rc, &grade, &baked, None, &src, &dst).unwrap());
    assert_eq!(
        gp.cache_stats(),
        (5, 4),
        "only the uniform is dirty on a param change"
    );

    // Identity grade: bypassed before the cache is even consulted.
    let identity = ColorGrade::default();
    let baked_id = identity.curves.bake();
    assert!(!gp
        .apply(&rc, &identity, &baked_id, None, &src, &dst)
        .unwrap());
    assert_eq!(gp.cache_stats(), (5, 4), "identity bypass touches nothing");
}
