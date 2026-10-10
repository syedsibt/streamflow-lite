//! Preset-pack validation (C8).
//!
//! Every `.sfstack` under `assets/packs/` must parse as a [`FilterStack`]
//! (respecting the stack rules: <= 8 slots, <= 2 looks, intensities in
//! 0..=1) and apply end-to-end in the engine. Every `.sflook` must parse
//! as a [`LookPreset`] and apply as a look slot.
//!
//! Real headless GPU throughout (lavapipe). Single test function on
//! purpose: the engine section stages a project-style temp layout
//! (`color/luts/podcast_warm.cube` — the engine resolves LUT paths
//! relative to the current working directory) and swaps cwd, which is
//! process-global.

use std::path::PathBuf;

use sf_engine::Engine;
use sf_filters::{apply_look, Filter, FilterSlot, FilterStack, LookRegistry};
use uuid::Uuid;

fn repo_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
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

/// Restores the working directory when dropped.
struct CwdGuard(PathBuf);
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
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
fn preset_packs_parse_and_apply() {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let repo = repo_dir();
    let packs = repo.join("assets/packs");

    // 1. Every .sfstack parses and respects the stack rules.
    let mut stack_files: Vec<PathBuf> = std::fs::read_dir(&packs)
        .expect("assets/packs must exist")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sfstack"))
        .collect();
    stack_files.sort();
    assert_eq!(
        stack_files.len(),
        3,
        "expected 3 stack presets, found {:?}",
        stack_files
    );
    let mut stacks: Vec<(String, FilterStack)> = Vec::new();
    for path in &stack_files {
        let text = std::fs::read_to_string(path).unwrap();
        let stack: FilterStack = serde_json::from_str(&text).expect("valid .sfstack JSON");
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_string();
        let n = stack.slots().len();
        assert!(
            (1..=FilterStack::MAX_SLOTS).contains(&n),
            "{name}: {n} slots, must be 1..={}",
            FilterStack::MAX_SLOTS
        );
        assert!(
            stack.look_count() <= FilterStack::MAX_LOOKS,
            "{name}: too many look filters"
        );
        for slot in stack.slots() {
            assert!(
                (0.0..=1.0).contains(&slot.intensity),
                "{name}: slot intensity out of range"
            );
            match &slot.filter {
                Filter::Look {
                    preset_id,
                    intensity,
                } => {
                    assert!(!preset_id.is_empty(), "{name}: empty look preset id");
                    assert!(
                        (0.0..=1.0).contains(intensity),
                        "{name}: look intensity out of range"
                    );
                }
                Filter::Effect { kind, params } => {
                    // Re-derive the kind's defaults: stored params must be
                    // sane (finite, and the shared baseline fields intact).
                    let defaults = kind.default_params();
                    assert_eq!(params.center, defaults.center, "{name}: {kind:?}");
                    assert_eq!(params.axis, defaults.axis, "{name}: {kind:?}");
                    assert!(
                        params.amount.is_finite(),
                        "{name}: {kind:?} amount not finite"
                    );
                }
            }
        }
        stacks.push((name, stack));
    }

    // 2. Every .sflook parses; each expands to a non-identity grade.
    let registry = LookRegistry::load_dir(&packs).expect("pack looks load");
    assert_eq!(registry.len(), 2, "expected 2 community looks");
    for id in registry.ids() {
        let preset = registry.get(&id).unwrap();
        let grade = apply_look(preset, 1.0);
        assert!(
            !grade.is_identity(),
            "community look {id} must be non-identity"
        );
        println!("pack look {id}: {}", preset.name);
    }

    // 3. Apply everything end-to-end in the engine. Stage a project-style
    //    layout: the engine resolves LUT paths relative to cwd, and the
    //    podcast stack's look references color/luts/podcast_warm.cube.
    let tmp = std::env::temp_dir().join("sf-packs-c8-test");
    let _ = std::fs::remove_dir_all(&tmp);
    let luts_dir = tmp.join("color/luts");
    std::fs::create_dir_all(&luts_dir).unwrap();
    std::fs::copy(
        repo.join("assets/luts/podcast_warm.cube"),
        luts_dir.join("podcast_warm.cube"),
    )
    .expect("shipped LUT must exist (run `cargo run -p gen-lut` first)");
    // One registry with the built-in looks plus the community pack looks.
    let looks_dir = tmp.join("looks");
    std::fs::create_dir_all(&looks_dir).unwrap();
    for src in [&repo.join("assets/looks"), &packs] {
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("sflook") {
                std::fs::copy(&path, looks_dir.join(path.file_name().unwrap())).unwrap();
            }
        }
    }

    let _cwd = CwdGuard(std::env::current_dir().unwrap());
    std::env::set_current_dir(&tmp).unwrap();

    let mut e = Engine::new_headless().expect("headless engine");
    *e.look_registry_mut() = LookRegistry::load_dir(&looks_dir).expect("combined look registry");
    e.state_mut().add_input("cam1", "Camera 1");
    let px = gradient64();
    let plain = e.grade_input_frame("cam1", 64, 64, &px).unwrap();

    let set_stack = |e: &mut Engine, stack: &FilterStack| {
        e.state_mut()
            .inputs
            .iter_mut()
            .find(|i| i.id == "cam1")
            .expect("cam1 exists")
            .filters = stack.clone();
    };

    // Each stack preset applies cleanly and visibly changes the frame.
    for (name, stack) in &stacks {
        set_stack(&mut e, stack);
        let out = e
            .grade_input_frame("cam1", 64, 64, &px)
            .expect("pack stack must apply");
        assert_ne!(
            plain.pixels, out.pixels,
            "stack preset {name} must change the frame"
        );
        println!("pack stack {name}: {} slots applied", stack.slots().len());
    }

    // Each community look applies as a look slot.
    for id in registry.ids() {
        let mut stack = FilterStack::new();
        stack.push(look_slot(&id)).unwrap();
        set_stack(&mut e, &stack);
        let out = e
            .grade_input_frame("cam1", 64, 64, &px)
            .expect("community look must apply");
        assert_ne!(
            plain.pixels, out.pixels,
            "community look {id} must change the frame"
        );
    }
}
