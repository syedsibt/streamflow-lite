//! Headless integration tests for the `sf-ui-backend` production state
//! machine: scenes, sources, preview/program, transitions, mixer,
//! streaming, recording, and macros — all against the real engine.

use sf_ipc::FilterTarget;
use sf_ui_backend::{
    transition_progress, MacroAction, RecordLifecycle, Shell, SourceKind, StreamLifecycle, UiEvent,
};

fn shell() -> Shell {
    Shell::new().expect("headless engine must open (lavapipe)")
}

/// A fresh scene with no sources.
fn empty_scene(s: &mut Shell) -> String {
    s.scene_create("Empty").id
}

fn add_pattern(s: &mut Shell, scene_id: &str, name: &str) -> String {
    s.source_add(scene_id, "test_pattern", name, [0, 0, 0], None, None, None, None, 0, 0, 0)
        .expect("add test pattern source")
        .id
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

#[test]
fn scene_crud_with_guards() {
    let mut s = shell();
    let initial = s.scene_list();
    assert_eq!(initial.len(), 1); // "Scene 1" from init

    let a = s.scene_create("A").id;
    assert_eq!(s.scene_list().len(), 2);

    let renamed = s.scene_rename(&a, "Alpha").unwrap();
    assert_eq!(renamed.name, "Alpha");
    assert!(s.scene_rename("nope", "x").is_err());

    let dup = s.scene_duplicate(&a).unwrap();
    assert_ne!(dup.id, a);
    assert_eq!(dup.name, "Alpha copy");
    assert_eq!(s.scene_list().len(), 3);

    // Duplicate deep-copies sources with fresh ids.
    let src = add_pattern(&mut s, &a, "P1");
    let dup2 = s.scene_duplicate(&a).unwrap();
    assert_eq!(dup2.sources.len(), 1);
    assert_ne!(dup2.sources[0].id, src);
    assert!(dup2.sources[0].input_id.is_some());
    assert_ne!(
        dup2.sources[0].input_id.as_ref().unwrap(),
        s.scene_list().iter().find(|sc| sc.id == a).unwrap().sources[0]
            .input_id
            .as_ref()
            .unwrap()
    );

    s.scene_delete(&a).unwrap();
    assert_eq!(s.scene_list().len(), 3);
    assert!(s.scene_delete("nope").is_err());

    // Delete down to one scene; the last delete must fail.
    let ids: Vec<String> = s.scene_list().iter().map(|sc| sc.id.clone()).collect();
    for id in &ids[..ids.len() - 1] {
        s.scene_delete(id).unwrap();
    }
    assert_eq!(s.scene_list().len(), 1);
    assert!(s.scene_delete(&s.scene_list()[0].id.clone()).is_err());
}

#[test]
fn deleting_program_scene_falls_back_sanely() {
    let mut s = shell();
    let b = s.scene_create("B").id;
    s.production_set_program(&b).unwrap();
    s.production_set_preview(&b).unwrap();
    s.scene_delete(&b).unwrap();
    let st = s.production_state();
    assert_eq!(st.scenes.len(), 1);
    assert_eq!(st.program_scene_id, st.scenes[0].id);
    assert_eq!(st.preview_scene_id, st.scenes[0].id);
    // The fallback emitted a scene-changed event.
    assert!(s
        .drain_events()
        .iter()
        .any(|e| matches!(e, UiEvent::SceneChanged { .. })));
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(windows, ignore = "adds a camera source; on Windows this opens a real DirectShow device")]
fn source_crud_reorder_visibility() {
    let mut s = shell();
    let scene = empty_scene(&mut s);

    let p1 = add_pattern(&mut s, &scene, "P1");
    let red = s
        .source_add(&scene, "color", "Red", [255, 0, 0], None, None, None, None, 0, 0, 0)
        .unwrap();
    assert!(matches!(red.kind, SourceKind::Color { r: 255, g: 0, b: 0 }));
    assert!(red.input_id.is_some());
    assert_eq!(red.order, 1);

    // Pending kinds are honest: status pending + reason, no engine input.
    let cam = s
        .source_add(&scene, "camera", "Cam", [0, 0, 0], None, None, None, None, 0, 0, 0)
        .unwrap();
    assert!(cam.input_id.is_none());
    assert!(matches!(cam.kind, SourceKind::Camera { .. }));
    assert_eq!(
        cam.kind.pending_reason(),
        Some("capture backend not implemented for this source kind")
    );
    // Media with a bad path fails cleanly (no half-added source).
    let media_err = s
        .source_add(&scene, "media", "Clip", [0, 0, 0], None, Some("/tmp/x.mp4"), None, None, 0, 0, 0)
        .expect_err("nonexistent media must fail");
    assert!(
        media_err.to_string().contains("cannot open media"),
        "unexpected: {media_err}"
    );
    assert!(s
        .source_add(&scene, "bogus", "X", [0, 0, 0], None, None, None, None, 0, 0, 0)
        .is_err());

    // Reorder: red to the bottom.
    s.source_reorder(&scene, vec![red.id.clone(), p1.clone(), cam.id.clone()])
        .unwrap();
    let sc = s.scene_list().into_iter().find(|x| x.id == scene).unwrap();
    assert_eq!(sc.sources[0].id, red.id);
    assert_eq!(sc.sources[0].order, 0);
    // Mismatched reorder list errors.
    assert!(s.source_reorder(&scene, vec![p1.clone()]).is_err());

    // Visibility toggle.
    let hidden = s.source_set_visible(&scene, &p1, false).unwrap();
    assert!(!hidden.visible);
    let shown = s.source_set_visible(&scene, &p1, true).unwrap();
    assert!(shown.visible);

    // Remove drops the source (and errors on unknown).
    s.source_remove(&scene, &cam.id).unwrap();
    let sc = s.scene_list().into_iter().find(|x| x.id == scene).unwrap();
    assert!(!sc.sources.iter().any(|x| x.id == cam.id));
    assert!(s.source_remove(&scene, "nope").is_err());

    // Source changes emit events.
    assert!(s
        .drain_events()
        .iter()
        .any(|e| matches!(e, UiEvent::SourceChanged { .. })));
}

// ---------------------------------------------------------------------------
// Scene render: real pipeline, composite, valid PNG
// ---------------------------------------------------------------------------

fn decode_rgba(png: &[u8]) -> Vec<u8> {
    image::load_from_memory(png)
        .expect("valid PNG")
        .to_rgba8()
        .into_raw()
}

#[test]
#[cfg_attr(windows, ignore = "adds a camera source; on Windows this opens a real DirectShow device")]
fn scene_render_returns_valid_nonblank_png_via_real_pipeline() {
    let mut s = shell();
    let scene = empty_scene(&mut s);
    add_pattern(&mut s, &scene, "P1");

    let png = s.scene_render_png(&scene, 160, 90).unwrap();
    // PNG magic.
    assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    let px = decode_rgba(&png);
    assert_eq!(px.len(), 160 * 90 * 4);
    // The test chart is colorful: red channel must vary (not blank).
    let (mn, mx) = px
        .chunks_exact(4)
        .map(|c| c[0])
        .fold((u8::MAX, u8::MIN), |(a, b), v| (a.min(v), b.max(v)));
    assert!(mx - mn > 40, "frame looks blank: {mn}..{mx}");

    // Pending sources are skipped, not faked: adding a camera still renders.
    s.source_add(&scene, "camera", "Cam", [0, 0, 0], None, None, None, None, 0, 0, 0)
        .unwrap();
    let png2 = s.scene_render_png(&scene, 160, 90).unwrap();
    assert_eq!(png, png2); // camera contributed nothing

    assert!(s.scene_render_png("nope", 160, 90).is_err());
}

#[test]
fn composite_order_and_visibility() {
    let mut s = shell();
    let scene = empty_scene(&mut s);
    // Bottom: test pattern. Top: opaque red — covers everything.
    add_pattern(&mut s, &scene, "P1");
    let red = s
        .source_add(&scene, "color", "Red", [255, 0, 0], None, None, None, None, 0, 0, 0)
        .unwrap();

    let px = decode_rgba(&s.scene_render_png(&scene, 64, 36).unwrap());
    let mean_r: f32 = px.chunks_exact(4).map(|c| c[0] as f32).sum::<f32>() / (64.0 * 36.0);
    let mean_g: f32 = px.chunks_exact(4).map(|c| c[1] as f32).sum::<f32>() / (64.0 * 36.0);
    assert!(
        mean_r > 250.0 && mean_g < 5.0,
        "top opaque source must cover"
    );

    // Hide the red layer: the pattern shows through again.
    s.source_set_visible(&scene, &red.id, false).unwrap();
    let px = decode_rgba(&s.scene_render_png(&scene, 64, 36).unwrap());
    let (mn, mx) = px
        .chunks_exact(4)
        .map(|c| c[0])
        .fold((u8::MAX, u8::MIN), |(a, b), v| (a.min(v), b.max(v)));
    assert!(mx - mn > 40, "hidden source must not composite");
}

// ---------------------------------------------------------------------------
// Preview/program + transitions
// ---------------------------------------------------------------------------

#[test]
fn cut_swaps_program_instantly() {
    let mut s = shell();
    let b = empty_scene(&mut s);
    s.production_set_preview(&b).unwrap();
    let before = s.production_state().program_scene_id;
    assert_ne!(before, b);
    s.transition_cut().unwrap();
    assert_eq!(s.production_state().program_scene_id, b);
    // Cut emits transition_done + scene_changed.
    let evts = s.drain_events();
    assert!(evts
        .iter()
        .any(|e| matches!(e, UiEvent::TransitionDone { .. })));
    assert!(evts
        .iter()
        .any(|e| matches!(e, UiEvent::SceneChanged { .. })));
}

#[test]
fn transition_progress_math_is_pure_and_eased() {
    assert_eq!(transition_progress(0, 1000), 0.0);
    assert!((transition_progress(500, 1000) - 0.5).abs() < 1e-6);
    assert_eq!(transition_progress(1000, 1000), 1.0);
    assert_eq!(transition_progress(1500, 1000), 1.0); // clamped
    assert_eq!(transition_progress(0, 0), 1.0); // degenerate duration
                                                // Smoothstep shape: slow start, fast middle, slow end.
    let q1 = transition_progress(250, 1000);
    let q3 = transition_progress(750, 1000);
    assert!(q1 < 0.25 && q3 > 0.75 && q1 < q3);
    // Monotonic.
    let mut prev = 0.0f32;
    for ms in (0..=1000).step_by(50) {
        let p = transition_progress(ms, 1000);
        assert!(p >= prev);
        prev = p;
    }
}

#[test]
fn auto_transition_ticks_and_commits() {
    let mut s = shell();
    let b = empty_scene(&mut s);
    add_pattern(&mut s, &b, "PB");
    s.production_set_preview(&b).unwrap();

    // Unknown kind errors; double-start errors.
    assert!(s.transition_auto(200, "wipe").is_err());
    let view = s.transition_auto(200, "mix").unwrap();
    assert!(view.active);
    assert_eq!(view.kind, "mix");
    assert!(s.transition_auto(200, "mix").is_err());

    // Tick while active: progress events, program monitor is a live crossfade.
    std::thread::sleep(std::time::Duration::from_millis(30));
    let view = s.transition_tick();
    assert!(view.active);
    assert!(view.progress > 0.0 && view.progress < 1.0);
    let png = s.program_frame_png(64, 36).unwrap();
    assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    let evts = s.drain_events();
    assert!(evts
        .iter()
        .any(|e| matches!(e, UiEvent::TransitionProgress { .. })));

    // Run to completion: program commits to preview.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let view = s.transition_tick();
        if !view.active {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "transition stalled");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(s.production_state().program_scene_id, b);
    assert!(s
        .drain_events()
        .iter()
        .any(|e| matches!(e, UiEvent::TransitionDone { .. })));
    // Idle tick is a no-op.
    let idle = s.transition_tick();
    assert!(!idle.active);
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

#[test]
fn mixer_gain_mute_and_master() {
    let mut s = shell();
    let scene = empty_scene(&mut s);
    let p1 = add_pattern(&mut s, &scene, "P1");

    let strip = s.mixer_set_gain(&p1, 1.5).unwrap();
    assert!((strip.gain - 1.5).abs() < 1e-6);
    let strip = s.mixer_set_gain(&p1, 5.0).unwrap();
    assert!((strip.gain - 2.0).abs() < 1e-6); // clamped
    let strip = s.mixer_set_gain(&p1, -1.0).unwrap();
    assert!((strip.gain - 0.0).abs() < 1e-6); // clamped
    let strip = s.mixer_set_mute(&p1, true).unwrap();
    assert!(strip.mute);
    assert!(s.mixer_set_gain("nope", 1.0).is_err());

    let master = s.mixer_set_master_gain(0.5);
    assert!((master.gain - 0.5).abs() < 1e-6);
    let master = s.mixer_set_master_mute(true);
    assert!(master.mute);

    assert!(s
        .drain_events()
        .iter()
        .any(|e| matches!(e, UiEvent::MixerChanged)));

    // Meters are 0.0 with meters_live false until real audio flows
    // (no media sources here).
    let st = s.production_state();
    assert!(!st.mixer.meters_live);
    assert!(st.mixer.meters.values().all(|&v| v == 0.0));
    assert_eq!(st.mixer.meters.len(), st.mixer.channels.len());
}

// ---------------------------------------------------------------------------
// Streaming (real attempt) + key redaction
// ---------------------------------------------------------------------------

#[test]
fn stream_lifecycle_fails_cleanly_and_redacts_key() {
    use sf_output::ReconnectPolicy;
    let mut s = shell();
    let secret = "<redacted>";
    // Nothing listens on 1935 here: deterministic connection-refused.
    s.stream_set_settings("rtmp://127.0.0.1:1935/live", secret, "custom_rtmp")
        .unwrap();
    assert!(s
        .stream_set_settings("rtmp://x", secret, "myspace")
        .is_err());

    // Connect now happens on the async send worker: start() returns Ok and
    // the failure surfaces through the lifecycle (fail-fast policy so the
    // test doesn't wait out the real backoff).
    s.stream_start_with_policy(ReconnectPolicy {
        max_attempts: 0,
        ..ReconnectPolicy::default()
    })
    .expect("async start must not fail synchronously");
    let mut lifecycle = s.production_state().stream_lifecycle;
    for _ in 0..200 {
        s.stream_drain_worker();
        lifecycle = s.production_state().stream_lifecycle;
        if matches!(lifecycle, StreamLifecycle::Error { .. }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // ...and the lifecycle reflects the redacted failure.
    match &lifecycle {
        StreamLifecycle::Error { message } => {
            assert!(
                message.contains("RTMP connect failed") || message.contains("reconnect"),
                "unexpected: {message}"
            );
            assert!(!message.contains(secret), "stream key leaked: {message}");
        }
        other => panic!("expected Error lifecycle, got {other:?}"),
    }
    let evts = s.drain_events();
    assert!(evts
        .iter()
        .any(|e| matches!(e, UiEvent::StreamState { .. })));
}

// ---------------------------------------------------------------------------
// Recording (real backend: sf-encode + sf-output)
// ---------------------------------------------------------------------------

#[test]
fn record_lifecycle_records_for_real() {
    let mut s = shell();
    let out = std::env::temp_dir().join("sf-prod-record.mp4").to_string_lossy().into_owned();
    let out = out.as_str();
    let _ = std::fs::remove_file(out);
    s.record_set_settings(out, "mp4", "High").unwrap();
    assert!(s.record_set_settings("/tmp/x", "avi", "High").is_err());

    // Real backend now: start works, lifecycle shows Recording.
    s.record_start().expect("record_start");
    assert!(matches!(
        s.production_state().record_lifecycle,
        RecordLifecycle::Recording { .. }
    ));
    assert!(s
        .drain_events()
        .iter()
        .any(|e| matches!(e, UiEvent::RecordState { .. })));

    // Pause/resume around a real recording.
    s.record_pause().expect("pause");
    assert!(matches!(
        s.production_state().record_lifecycle,
        RecordLifecycle::Paused { .. }
    ));
    s.record_resume().expect("resume");

    // Double-start is rejected.
    assert!(s.record_start().is_err());

    // Drive a few frames through the (real) tick path.
    for _ in 0..3 {
        s.av_tick();
    }

    s.record_stop().unwrap();
    assert!(matches!(
        s.production_state().record_lifecycle,
        RecordLifecycle::Idle
    ));
    assert!(
        std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) > 1_000,
        "recording file should exist and be non-trivial"
    );
}

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

#[test]
fn macro_add_dispatch_remove() {
    let mut s = shell();
    let b = empty_scene(&mut s);

    let cut = s.macro_add("Quick cut", "F1", MacroAction::Cut).unwrap();
    // Duplicate hotkeys are rejected.
    assert!(s.macro_add("Dup", "F1", MacroAction::Cut).is_err());
    assert!(s.macro_dispatch("F9").is_err());

    // Dispatch cut: program jumps to preview.
    s.production_set_preview(&b).unwrap();
    let r = s.macro_dispatch("F1").unwrap();
    assert_eq!(r.macro_id, cut.id);
    assert_eq!(s.production_state().program_scene_id, b);
    assert!(r.happened.contains("cut"));

    // Dispatch auto transition.
    s.macro_add(
        "Slow mix",
        "F2",
        MacroAction::AutoTransition { duration_ms: 50 },
    )
    .unwrap();
    let r = s.macro_dispatch("F2").unwrap();
    assert!(r.happened.contains("auto transition"));
    assert!(s.production_state().transition.active);
    // Let it finish so later dispatches are clean.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while s.production_state().transition.active {
        s.transition_tick();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Toggle macros drive the real backends: stream fails cleanly with
    // no URL set, record fails cleanly with no path set.
    s.macro_add("Stream", "F3", MacroAction::ToggleStream)
        .unwrap();
    let r = s.macro_dispatch("F3").unwrap();
    assert!(
        r.happened.contains("stream start failed"),
        "got: {}",
        r.happened
    );
    s.macro_add("Record", "F4", MacroAction::ToggleRecord)
        .unwrap();
    let r = s.macro_dispatch("F4").unwrap();
    assert!(
        r.happened.contains("record start failed"),
        "got: {}",
        r.happened
    );

    // Remove + list.
    s.macro_remove(&cut.id).unwrap();
    assert!(s.macro_remove(&cut.id).is_err());
    assert_eq!(s.macro_list().len(), 3);
}

#[test]
fn macro_apply_look_hits_program_sources_through_real_pipeline() {
    let mut s = shell();
    let look_id = s
        .list_looks()
        .first()
        .expect("built-in looks loaded")
        .id
        .clone();
    let b = empty_scene(&mut s);
    let src = add_pattern(&mut s, &b, "P1");
    s.production_set_preview(&b).unwrap();
    s.transition_cut().unwrap(); // program = b

    s.macro_add(
        "Warm look",
        "F5",
        MacroAction::ApplyLook {
            look_id: look_id.clone(),
            intensity: 0.8,
        },
    )
    .unwrap();
    let r = s.macro_dispatch("F5").unwrap();
    assert!(r.happened.contains(&look_id), "got: {}", r.happened);

    // The look really landed on the program source's input stack.
    let input_id = s
        .scene_list()
        .into_iter()
        .find(|sc| sc.id == b)
        .unwrap()
        .sources
        .into_iter()
        .find(|x| x.id == src)
        .unwrap()
        .input_id
        .unwrap();
    let slots = s.stack_snapshot(&FilterTarget::Input(input_id)).unwrap();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].kind, "look");
    assert!((slots[0].intensity - 0.8).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// Video settings (Video menu panel)
// ---------------------------------------------------------------------------

#[test]
fn video_settings_defaults_and_roundtrip() {
    let mut s = shell();
    let d = s.video_settings_get();
    assert_eq!((d.output_width, d.output_height), (1920, 1080));
    assert_eq!(d.output_fps, 30);
    assert_eq!(d.encoder, "auto");
    assert_eq!(d.bitrate_kbps, 8000);
    assert_eq!(s.frame_interval_us(), 33_333);

    let mut upd = d.clone();
    upd.output_width = 1280;
    upd.output_height = 720;
    upd.output_fps = 60;
    upd.encoder = "libx264".to_string();
    upd.bitrate_kbps = 6000;
    s.video_settings_set(upd).expect("valid settings");
    let back = s.video_settings_get();
    assert_eq!((back.output_width, back.output_height), (1280, 720));
    assert_eq!(back.output_fps, 60);
    assert_eq!(back.encoder, "libx264");
    assert_eq!(s.frame_interval_us(), 16_666);
}

#[test]
fn video_settings_rejects_bad_values() {
    let mut s = shell();
    let d = s.video_settings_get();
    let mut bad = d.clone();
    bad.output_width = 999;
    assert!(s.video_settings_set(bad).is_err());

    let mut bad = d.clone();
    bad.output_fps = 25;
    assert!(s.video_settings_set(bad).is_err());

    let mut bad = d.clone();
    bad.encoder = "h264_foo".to_string();
    assert!(s.video_settings_set(bad).is_err());

    let mut bad = d.clone();
    bad.bitrate_kbps = 100;
    assert!(s.video_settings_set(bad).is_err());

    let mut bad = d;
    bad.camera_fps = 120;
    assert!(s.video_settings_set(bad).is_err());
}

#[test]
#[cfg_attr(windows, ignore = "uses fake camera names; on Windows these open real DirectShow devices")]
fn camera_reconfigure_updates_kind_and_lists_sources() {
    let mut s = shell();
    let scene = empty_scene(&mut s);
    // fps=0 → default; on non-Windows no producer opens but kind is stored.
    let id = s
        .source_add(&scene, "camera", "Cam", [0, 0, 0], None, None, None, None, 1280, 720, 0)
        .expect("add camera")
        .id;
    let cams = s.camera_sources();
    assert_eq!(cams.len(), 1);
    assert_eq!(cams[0].id, id);

    let negotiated = s
        .camera_reconfigure(&id, Some("FakeCam".to_string()), 1920, 1080, 60, None)
        .expect("reconfigure");
    assert!(!negotiated.is_empty());

    let cams = s.camera_sources();
    assert_eq!(cams.len(), 1);
    assert_eq!(cams[0].device.as_deref(), Some("FakeCam"));
    assert_eq!((cams[0].width, cams[0].height), (1920, 1080));
    assert_eq!(cams[0].fps, 60);

    // Non-camera source must be rejected.
    let pat = add_pattern(&mut s, &scene, "Pat");
    assert!(s.camera_reconfigure(&pat, None, 640, 480, 30, None).is_err());
    // Unknown source must be rejected.
    assert!(s.camera_reconfigure("nope", None, 640, 480, 30, None).is_err());
}

#[test]
#[cfg_attr(windows, ignore = "adds a camera source; on Windows this opens a real DirectShow device")]
fn camera_auto_audio_attaches() {
    let mut s = shell();
    let scene = s.scene_create("S").id;
    // No audio device passed → auto-attach attempts (None on this
    // platform without DirectShow, but must not fail or mis-store).
    let cam = s
        .source_add(
            &scene, "camera", "Cam", [0, 0, 0], None, None, None, None, 1280, 720, 30,
        )
        .expect("add camera")
        .id;
    assert_eq!(s.source_audio_device(&cam), s.auto_camera_audio(None));
    // Explicit device is kept as-is (no override).
    let cam2 = s
        .source_add(
            &scene,
            "camera",
            "Cam2",
            [0, 0, 0],
            None,
            None,
            None,
            Some("Explicit Mic"),
            1280,
            720,
            30,
        )
        .expect("add camera2")
        .id;
    assert_eq!(
        s.source_audio_device(&cam2),
        Some("Explicit Mic".to_string())
    );
}
