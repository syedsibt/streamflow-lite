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

#[test]
fn load_lut_sets_ref() {
    let mut s = shell();
    s.load_lut("cam1", "color/luts/podcast_warm.cube", "before", 0.6)
        .unwrap();
    let grade = s.grade_snapshot("cam1").unwrap();
    let lut = grade.lut.expect("lut must be set");
    assert_eq!(lut.file, "color/luts/podcast_warm.cube");
    assert!((lut.intensity - 0.6).abs() < 1e-6);
    assert!(matches!(lut.position, sf_ui_backend::LutPosition::Before));
    // Unknown position falls back to After (documented).
    s.load_lut("cam1", "x.cube", "sideways", 1.0).unwrap();
    let grade = s.grade_snapshot("cam1").unwrap();
    assert!(matches!(
        grade.lut.unwrap().position,
        sf_ui_backend::LutPosition::After
    ));
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
