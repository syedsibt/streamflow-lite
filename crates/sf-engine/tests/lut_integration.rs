//! End-to-end test for the committed `podcast_warm.cube` LUT (C8).
//!
//! Flow: run the committed `tools/gen-lut` generator into a temp dir ->
//! parse the output with the real `.cube` parser -> spot-check the creative
//! transform -> load `podcast-warm.sflook` through the look registry ->
//! apply it in the engine (the look's `color/luts/podcast_warm.cube`
//! reference resolves against a project-style temp layout — the engine
//! resolves LUT paths relative to the current working directory).
//!
//! Real headless GPU throughout (lavapipe). Single test function on
//! purpose: the engine section swaps the process working directory, which
//! is process-global.

use std::path::{Path, PathBuf};
use std::process::Command;

use sf_engine::Engine;
use sf_filters::{apply_look, Filter, FilterSlot, FilterStack, LookRegistry};
use sf_render::lut::parse_cube;
use uuid::Uuid;

fn repo_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `gen-lut` binary built by `cargo test --workspace`: it sits next to
/// this test binary's `deps/` directory.
fn gen_lut_bin() -> PathBuf {
    std::env::current_exe()
        .expect("current test exe path")
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps layout")
        .join("gen-lut")
}

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

/// Restores the working directory when dropped (the engine resolves LUT
/// paths relative to cwd, so the test below swaps it temporarily).
struct CwdGuard(PathBuf);
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[test]
fn podcast_warm_lut_generate_parse_and_apply() {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let repo = repo_dir();

    // 1. Generate with the committed generator (into a temp dir, so the
    //    committed asset itself is never clobbered by the test).
    let tmp = std::env::temp_dir().join("sf-lut-c8-test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let generated_path = tmp.join("podcast_warm.cube");
    let status = Command::new(gen_lut_bin())
        .arg(&generated_path)
        .status()
        .expect(
            "gen-lut binary missing: run via `cargo test --workspace` so the bin target is built",
        );
    assert!(status.success(), "gen-lut exited with {status}");
    let text = std::fs::read_to_string(&generated_path).unwrap();

    // The committed asset must be byte-identical to fresh generator output
    // (the generator is deterministic — no timestamps or randomness).
    let shipped = std::fs::read_to_string(repo.join("assets/luts/podcast_warm.cube"))
        .expect("shipped assets/luts/podcast_warm.cube must exist");
    assert_eq!(
        shipped, text,
        "committed LUT drifted from generator output — re-run `cargo run -p gen-lut`"
    );

    // 2. Parse with the real .cube parser.
    let cube = parse_cube(&text).expect("generated LUT must parse");
    assert_eq!(cube.size, 33, "must be a 33^3 LUT");
    assert_eq!(cube.data.len(), 33 * 33 * 33);
    assert!(
        cube.warnings.is_empty(),
        "unexpected parser warnings: {:?}",
        cube.warnings
    );
    assert!(
        cube.title.as_deref().unwrap_or("").contains("Podcast Warm"),
        "title should identify the look: {:?}",
        cube.title
    );
    // Red-fastest node order (standard .cube layout).
    let node = |r: usize, g: usize, b: usize| cube.data[r + 33 * g + 33 * 33 * b];

    // 3. Spot-check the creative transform.
    // Black node: warm (amber) shadows lift — red lifted most, then green.
    let black = node(0, 0, 0);
    assert_eq!(black[3], 1.0, "alpha must be 1");
    assert!(
        (black[0] - 0.0306).abs() < 0.002,
        "black.r: expected ~0.0306, got {}",
        black[0]
    );
    assert!(
        (black[1] - 0.0179).abs() < 0.002,
        "black.g: expected ~0.0179, got {}",
        black[1]
    );
    assert!(
        (black[2] - 0.0115).abs() < 0.002,
        "black.b: expected ~0.0115, got {}",
        black[2]
    );
    assert!(
        black[0] > black[1] && black[1] > black[2],
        "shadows lift must be amber (r > g > b): {black:?}"
    );
    // White node: identity (lift and warmth vanish at the top of the domain).
    let white = node(32, 32, 32);
    for (c, v) in white.iter().take(3).enumerate() {
        assert!(
            (v - 1.0).abs() < 1e-4,
            "white channel {c}: expected 1.0, got {v}"
        );
    }
    // Mid gray: warmed — red pushed up, blue pulled down.
    let mid = node(16, 16, 16);
    assert!(
        mid[0] - mid[2] > 0.03,
        "mid gray must be warm (r-b > 0.03): {mid:?}"
    );
    assert!(mid[0] > 0.5 && mid[2] < 0.5, "warm split: {mid:?}");

    // 4. Load podcast-warm.sflook through the look registry and expand it.
    let registry = LookRegistry::load_dir(&repo.join("assets/looks")).expect("built-in looks");
    let preset = registry
        .get("podcast-warm")
        .expect("podcast-warm registered");
    assert!(preset.skin_protect);
    let grade = apply_look(preset, 1.0);
    assert!(!grade.is_identity(), "expanded look must be non-identity");
    let lut = grade.lut.as_ref().expect("podcast-warm carries a LUT");
    assert_eq!(lut.file, "color/luts/podcast_warm.cube");
    assert!(
        (lut.intensity - 0.6).abs() < 1e-6,
        "preset LUT intensity 0.6, got {}",
        lut.intensity
    );

    // 5. Apply end-to-end in the engine. The engine resolves the LUT path
    //    relative to cwd, so stage a project-style layout and swap cwd.
    let luts_dir = tmp.join("color/luts");
    std::fs::create_dir_all(&luts_dir).unwrap();
    std::fs::write(luts_dir.join("podcast_warm.cube"), &text).unwrap();
    let _cwd = CwdGuard(std::env::current_dir().unwrap());
    std::env::set_current_dir(&tmp).unwrap();

    let mut e = Engine::new_headless().expect("headless engine");
    *e.look_registry_mut() = registry;
    e.state_mut().add_input("cam1", "Camera 1");
    let px = gradient64();
    let plain = e.grade_input_frame("cam1", 64, 64, &px).unwrap();

    // 5a. The look in a filter stack (the real UI path: look folding).
    let mut stack = FilterStack::new();
    stack
        .push(FilterSlot {
            id: Uuid::new_v4(),
            filter: Filter::Look {
                preset_id: "podcast-warm".to_string(),
                intensity: 1.0,
            },
            enabled: true,
            intensity: 1.0,
            mask: None,
        })
        .unwrap();
    e.state_mut()
        .inputs
        .iter_mut()
        .find(|i| i.id == "cam1")
        .expect("cam1 exists")
        .filters = stack;
    // A missing/unparseable LUT would fail here with LutLoad/io — success
    // proves the file was found, parsed, and uploaded to the GPU.
    let via_stack = e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    assert_ne!(
        plain.pixels, via_stack.pixels,
        "the look must change the frame"
    );

    // 5b. Isolate the LUT's own contribution: the same expanded grade with
    // and without the LUT must differ on the GPU.
    {
        let color = e
            .state_mut()
            .input_color_mut("cam1")
            .expect("cam1 color exists");
        color.grade = grade.clone();
    }
    e.state_mut()
        .inputs
        .iter_mut()
        .find(|i| i.id == "cam1")
        .expect("cam1 exists")
        .filters = FilterStack::new();
    let with_lut = e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    {
        let mut no_lut = grade.clone();
        no_lut.lut = None;
        e.state_mut()
            .input_color_mut("cam1")
            .expect("cam1 color exists")
            .grade = no_lut;
    }
    let without_lut = e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    assert_ne!(
        with_lut.pixels, without_lut.pixels,
        "the 3D LUT stage must change the graded frame"
    );
}
