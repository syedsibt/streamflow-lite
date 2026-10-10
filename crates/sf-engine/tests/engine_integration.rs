//! Engine-level integration test: real headless GPU end to end.

use sf_engine::Engine;

/// 64x64 RGBA gradient: r = x*4, g = y*4, b = (x+y)*2, a = 255.
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

fn mean_rgb_luma(px: &[u8]) -> f64 {
    let n = (px.len() / 4) as f64;
    px.chunks_exact(4)
        .map(|c| (c[0] as u32 + c[1] as u32 + c[2] as u32) as f64 / 3.0)
        .sum::<f64>()
        / n
}

#[test]
fn grade_input_frame_identity_bypass_and_exposure() {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let mut engine = Engine::new_headless().expect("headless engine");
    engine.state_mut().add_input("cam1", "Camera 1");
    let px = gradient64();

    // Identity grade: output must equal the input exactly (GPU untouched).
    let frame = engine
        .grade_input_frame("cam1", 64, 64, &px)
        .expect("identity grade");
    assert_eq!((frame.width, frame.height), (64, 64));
    assert_eq!(
        frame.pixels, px,
        "identity grade must return input unchanged"
    );

    // Unknown input errors.
    assert!(
        engine.grade_input_frame("nope", 64, 64, &px).is_err(),
        "unknown input must fail"
    );

    // +1 EV: output must differ from the input and be brighter on average.
    engine
        .state_mut()
        .input_color_mut("cam1")
        .expect("cam1 exists")
        .grade
        .exposure = 1.0;
    let graded = engine
        .grade_input_frame("cam1", 64, 64, &px)
        .expect("exposure grade");
    assert_eq!((graded.width, graded.height), (64, 64));
    assert_eq!(graded.pixels.len(), 64 * 64 * 4);
    assert_ne!(graded.pixels, px, "exposure must change the frame");
    let before = mean_rgb_luma(&px);
    let after = mean_rgb_luma(&graded.pixels);
    println!("mean luma before = {before:.2}, after +1 EV = {after:.2}");
    assert!(
        after > before,
        "exposure +1 EV must brighten: {before} -> {after}"
    );
}
