//! Headless integration tests for `sf-ui-backend`: every command handler
//! drives the real engine (real wgpu passes on lavapipe).

use sf_ipc::FilterTarget;
use sf_ui_backend::{ScopeDataView, Shell, UiEvent};
use uuid::Uuid;

fn shell() -> Shell {
    Shell::new().expect("headless engine must open (lavapipe)")
}

#[test]
fn set_param_roundtrip_clamps_and_echoes() {
    let mut s = shell();
    let grade = s.set_param("cam1", "exposure", 0.5).unwrap();
    assert!((grade.exposure - 0.5).abs() < 1e-6);
    // Clamping to the spec range.
    let grade = s.set_param("cam1", "exposure", 10.0).unwrap();
    assert!((grade.exposure - 3.0).abs() < 1e-6);
    let grade = s.set_param("cam1", "saturation", -5.0).unwrap();
    assert!((grade.saturation - 0.0).abs() < 1e-6);
    // Wheel channel suffix.
    let grade = s.set_param("cam1", "gain_r", 1.2).unwrap();
    assert!((grade.gain[0] - 1.2).abs() < 1e-6);
    // Unknown param + unknown input.
    assert!(s.set_param("cam1", "nope", 1.0).is_err());
    assert!(s.set_param("ghost", "exposure", 1.0).is_err());
    // Echo event queued.
    let evts = s.drain_events();
    assert!(evts.iter().any(|e| matches!(
        e,
        UiEvent::ColorChanged { input_id } if input_id == "cam1"
    )));
}

#[test]
fn reset_param_restores_neutral() {
    let mut s = shell();
    s.set_param("cam1", "exposure", 1.5).unwrap();
    s.set_param("cam1", "saturation", 140.0).unwrap();
    let grade = s.reset_param("cam1", "exposure").unwrap();
    assert!((grade.exposure).abs() < 1e-6);
    assert!((grade.saturation - 140.0).abs() < 1e-6); // untouched
    let grade = s.reset_param("cam1", "temperature").unwrap();
    assert!((grade.temperature - 6500.0).abs() < 1e-6);
}

#[test]
fn set_curve_and_reset_sections() {
    let mut s = shell();
    s.set_curve("cam1", "master", vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]])
        .unwrap();
    assert!(s.set_curve("cam1", "bogus", vec![[0.0, 0.0]]).is_err());
    s.set_param("cam1", "exposure", 1.0).unwrap();
    // Reset only basic: curve stays non-identity, exposure back to 0.
    let grade = s.reset("cam1", Some("basic")).unwrap();
    assert!((grade.exposure).abs() < 1e-6);
    assert!(!grade.curves.is_identity());
    let grade = s.reset("cam1", Some("curves")).unwrap();
    assert!(grade.curves.is_identity());
}

/// Writes a small identity `.cube` and returns its path.
fn temp_cube(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("sf-shell-luts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut text = format!("TITLE \"{name}\"\nLUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                text.push_str(&format!("{r} {g} {b}\n"));
            }
        }
    }
    let p = dir.join(format!("{name}.cube"));
    std::fs::write(&p, text).unwrap();
    p.to_string_lossy().to_string()
}

#[test]
fn load_lut_sets_ref() {
    let mut s = shell();
    let warm = temp_cube("podcast_warm");
    s.load_lut("cam1", &warm, "before", 0.6).unwrap();
    let grade = s.grade_snapshot("cam1").unwrap();
    let lut = grade.lut.expect("lut must be set");
    assert_eq!(lut.file, warm);
    assert!((lut.intensity - 0.6).abs() < 1e-6);
    assert!(matches!(lut.position, sf_ui_backend::LutPosition::Before));
    // The UI sends capitalized positions.
    s.load_lut("cam1", &warm, "Before", 0.6).unwrap();
    assert!(matches!(
        s.grade_snapshot("cam1").unwrap().lut.unwrap().position,
        sf_ui_backend::LutPosition::Before
    ));
    // Unknown position falls back to After (documented).
    let x = temp_cube("x");
    s.load_lut("cam1", &x, "sideways", 1.0).unwrap();
    let grade = s.grade_snapshot("cam1").unwrap();
    assert!(matches!(
        grade.lut.unwrap().position,
        sf_ui_backend::LutPosition::After
    ));
}

#[test]
fn bad_lut_is_rejected_and_old_lut_kept() {
    let mut s = shell();
    let good = temp_cube("good");
    s.load_lut("cam1", &good, "after", 1.0).unwrap();
    // Missing file, wrong format, truncated data: all rejected clearly.
    assert!(s.load_lut("cam1", "does/not/exist.cube", "after", 1.0).is_err());
    let e = s.load_lut("cam1", "C:/luts/film.3dl", "after", 1.0).unwrap_err().to_string();
    assert!(e.contains(".cube"), "{e}");
    let dir = std::env::temp_dir().join(format!("sf-shell-luts-{}", std::process::id()));
    let broken = dir.join("broken.cube");
    std::fs::write(&broken, "LUT_3D_SIZE 17\n0 0 0\n").unwrap();
    assert!(s.load_lut("cam1", broken.to_str().unwrap(), "after", 1.0).is_err());
    assert_eq!(s.grade_snapshot("cam1").unwrap().lut.unwrap().file, good);
    // Live intensity without reload.
    let g = s.set_param("cam1", "lut_intensity", 0.35).unwrap();
    assert!((g.lut.unwrap().intensity - 0.35).abs() < 1e-6);
}

#[test]
fn lut_library_import_list_remove() {
    let mut s = shell();
    let data = std::env::temp_dir().join(format!("sf-shell-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    s.set_data_dir(&data);
    let src = temp_cube("My Cinematic LUT");
    let info = s.lut_import(&src).unwrap();
    assert_eq!(info.name, "My Cinematic LUT");
    assert!(info.path.starts_with(data.to_str().unwrap()));
    // Re-import of the identical file does not duplicate.
    let again = s.lut_import(&src).unwrap();
    assert_eq!(again.path, info.path);
    assert_eq!(s.lut_library().len(), 1);
    s.load_lut("cam1", &info.path, "after", 0.7).unwrap();
    s.lut_remove(&info.id).unwrap();
    assert!(s.lut_library().is_empty());
    assert!(s.grade_snapshot("cam1").unwrap().lut.is_none(), "removed LUT detaches");
}

#[test]
fn one_active_look_and_presets_roundtrip() {
    let mut s = shell();
    let target = sf_ipc::FilterTarget::Input("cam1".to_string());
    let looks = s.list_looks();
    assert!(looks.len() >= 3);
    let a = s.look_apply(target.clone(), &looks[0].id, 0.8).unwrap();
    let b = s.look_apply(target.clone(), &looks[1].id, 0.5).unwrap();
    let c = s.look_apply(target.clone(), &looks[2].id, 0.5).unwrap();
    // Swapping looks keeps one slot (same id) — no stacking, no 2-look limit.
    assert_eq!(a, b);
    assert_eq!(b, c);
    let stack = s.stack_snapshot(&target).unwrap();
    assert_eq!(stack.iter().filter(|v| v.kind == "look").count(), 1);
    assert_eq!(stack[0].look_id.as_deref(), Some(looks[2].id.as_str()));

    // Presets: save, change, apply restores grade + stack.
    let data = std::env::temp_dir().join(format!("sf-shell-presets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    s.set_data_dir(&data);
    s.set_param("cam1", "faded_film", 30.0).unwrap();
    s.set_param("cam1", "skin_warmth", 12.0).unwrap();
    let p = s.color_preset_save("cam1", "My Warm Look").unwrap();
    s.reset("cam1", None).unwrap();
    s.look_clear(target.clone()).unwrap();
    assert!(s.stack_snapshot(&target).unwrap().is_empty());
    let g = s.color_preset_apply("cam1", &p.id).unwrap();
    assert!((g.faded_film - 30.0).abs() < 1e-6);
    assert!((g.skin_warmth - 12.0).abs() < 1e-6);
    assert_eq!(s.stack_snapshot(&target).unwrap().len(), 1);
    assert_eq!(s.color_preset_list().len(), 1);
    s.color_preset_delete(&p.id).unwrap();
    assert!(s.color_preset_list().is_empty());
}

#[test]
fn preview_modes_render() {
    let mut s = shell();
    s.set_param("cam1", "exposure", 1.0).unwrap();
    for m in [
        sf_ui_backend::PreviewMode::After,
        sf_ui_backend::PreviewMode::Before,
        sf_ui_backend::PreviewMode::Split,
    ] {
        let jpg = s.preview_frame_jpeg("cam1", 160, 90, m).unwrap();
        assert_eq!(&jpg[..2], &[0xFF, 0xD8], "JPEG magic");
    }
}

#[test]
fn filter_stack_add_reorder_intensity_toggle_remove() {
    use sf_filters::{EffectKind, Filter};
    let mut s = shell();
    let target = FilterTarget::Input("cam1".to_string());
    let a = s
        .filter_add(
            target.clone(),
            Filter::Effect {
                kind: EffectKind::Vignette,
                params: EffectKind::Vignette.default_params(),
            },
        )
        .unwrap();
    let b = s
        .filter_add(
            target.clone(),
            Filter::Effect {
                kind: EffectKind::FilmGrain,
                params: EffectKind::FilmGrain.default_params(),
            },
        )
        .unwrap();
    let snap = s.stack_snapshot(&target).unwrap();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].id, a);
    assert!(snap[0].label.contains("Vignette"));
    assert_eq!(snap[0].cost, "low");

    s.filter_set_intensity(target.clone(), b, 0.5).unwrap();
    let snap = s.stack_snapshot(&target).unwrap();
    assert!((snap[1].intensity - 0.5).abs() < 1e-6);

    s.filter_toggle(target.clone(), a, false).unwrap();
    let snap = s.stack_snapshot(&target).unwrap();
    assert!(!snap[0].enabled);

    // Reorder: b first.
    s.filter_reorder(target.clone(), vec![b, a]).unwrap();
    let snap = s.stack_snapshot(&target).unwrap();
    assert_eq!(snap[0].id, b);

    // Bad reorder (wrong ids) errors.
    assert!(s
        .filter_reorder(target.clone(), vec![Uuid::new_v4()])
        .is_err());

    s.filter_remove(target.clone(), a).unwrap();
    let snap = s.stack_snapshot(&target).unwrap();
    assert_eq!(snap.len(), 1);

    // Stack-changed events were queued.
    let evts = s.drain_events();
    assert!(evts
        .iter()
        .any(|e| matches!(e, UiEvent::FilterStackChanged { .. })));
}

#[test]
fn stack_limit_enforced_and_warns() {
    use sf_filters::{EffectKind, Filter};
    let mut s = shell();
    let target = FilterTarget::Input("cam1".to_string());
    for _ in 0..8 {
        s.filter_add(
            target.clone(),
            Filter::Effect {
                kind: EffectKind::Vignette,
                params: EffectKind::Vignette.default_params(),
            },
        )
        .unwrap();
    }
    // 9th is rejected by the stack rules.
    assert!(s
        .filter_add(
            target.clone(),
            Filter::Effect {
                kind: EffectKind::Vignette,
                params: EffectKind::Vignette.default_params(),
            },
        )
        .is_err());
    // Crossing the warning threshold emitted a cost warning.
    let evts = s.drain_events();
    assert!(evts
        .iter()
        .any(|e| matches!(e, UiEvent::GpuCostWarning { .. })));
}

#[test]
fn look_apply_and_list_looks() {
    let mut s = shell();
    let looks = s.list_looks();
    assert!(!looks.is_empty(), "built-in looks must load");
    let target = FilterTarget::Input("cam1".to_string());
    let id = s.look_apply(target.clone(), &looks[0].id, 0.8).unwrap();
    let snap = s.stack_snapshot(&target).unwrap();
    let slot = snap.iter().find(|v| v.id == id).unwrap();
    assert!(slot.label.starts_with("Look:"));
    assert!((slot.intensity - 0.8).abs() < 1e-6);
    assert!(s.look_apply(target, "no-such-look", 1.0).is_err());
}

#[test]
fn preview_png_is_valid_and_throttled() {
    let mut s = shell();
    let png = s.preview_png("cam1", 160, 90).unwrap();
    // PNG magic.
    assert_eq!(&png[0..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    // Immediate second call hits the 10 Hz throttle cache: identical bytes.
    let png2 = s.preview_png("cam1", 160, 90).unwrap();
    assert_eq!(png, png2);
    assert!(s.preview_png("ghost", 160, 90).is_err());
}

#[test]
fn scopes_subscribe_emit_frames_on_drain() {
    let mut s = shell();
    s.scopes_subscribe("cam1", "waveform").unwrap();
    s.scopes_subscribe("cam1", "histogram").unwrap();
    assert!(s.scopes_subscribe("cam1", "bogus").is_err());
    // Grading a frame taps the scope hub.
    s.preview_png("cam1", 160, 90).unwrap();
    let evts = s.drain_events();
    let frames: Vec<_> = evts
        .iter()
        .filter_map(|e| match e {
            UiEvent::ScopesFrame { kind, data, .. } => Some((kind.clone(), data)),
            _ => None,
        })
        .collect();
    assert!(frames.iter().any(|(k, _)| k == "waveform"));
    assert!(frames.iter().any(|(k, _)| k == "histogram"));
    for (kind, data) in &frames {
        match (kind.as_str(), data) {
            ("waveform", ScopeDataView::Waveform { luma_min, .. }) => {
                assert_eq!(luma_min.len(), 256)
            }
            ("histogram", ScopeDataView::Histogram { r, .. }) => assert_eq!(r.len(), 256),
            _ => panic!("unexpected scope payload for {kind}"),
        }
    }
    // Unsubscribe stops future subscriptions (idempotent, no error).
    s.scopes_unsubscribe("cam1", "waveform").unwrap();
    s.scopes_unsubscribe("cam1", "waveform").unwrap();
}

#[test]
fn auto_color_applies_editable_params() {
    let mut s = shell();
    s.preview_png("cam1", 160, 90).unwrap(); // ingests the synthetic frame
    let before = s.grade_snapshot("cam1").unwrap();
    let result = s.auto_color("cam1").unwrap();
    assert!(!result.explanation.is_empty());
    let after = s.grade_snapshot("cam1").unwrap();
    assert_ne!(before, after, "auto color must change the grade");
    // No frame ingested -> clean error.
    s.add_input("cam2", "Camera 2");
    assert!(s.auto_color("cam2").is_err());
}

#[test]
fn ui_event_serde_tags_match_spec() {
    let e = UiEvent::ColorChanged {
        input_id: "cam1".to_string(),
    };
    let json = serde_json::to_string(&e).unwrap();
    assert!(json.contains("\"event\":\"color.changed\""), "{json}");
    let back: UiEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, UiEvent::ColorChanged { .. }));
}
