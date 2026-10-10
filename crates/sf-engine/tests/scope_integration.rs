//! Engine-level scope tests: subscriptions, 15 Hz cadence, and the
//! no-subscribers-means-no-GPU-work guarantee. Real headless GPU throughout.

use std::time::{Duration, Instant};

use sf_engine::Engine;
use sf_scopes::{ScopeKind, ScopePayload};

fn engine() -> Engine {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let mut e = Engine::new_headless().expect("headless engine");
    e.state_mut().add_input("cam1", "Camera 1");
    e
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

#[test]
fn subscribe_validates_input_and_kind() {
    let mut e = engine();
    assert!(
        e.scopes_subscribe("nope", ScopeKind::Waveform).is_err(),
        "unknown input must fail"
    );
    assert!(
        e.scopes_subscribe("cam1", ScopeKind::Waveform).is_ok(),
        "known input subscribes"
    );
    // Idempotent: subscribing twice does not duplicate.
    e.scopes_subscribe("cam1", ScopeKind::Waveform).unwrap();
    assert_eq!(e.scopes().subscription_count(), 1);
    e.scopes_subscribe("cam1", ScopeKind::Histogram).unwrap();
    assert_eq!(e.scopes().subscription_count(), 2);
    // Unsubscribe removes exactly one.
    assert!(e.scopes_unsubscribe("cam1", ScopeKind::Waveform));
    assert!(!e.scopes_unsubscribe("cam1", ScopeKind::Waveform));
    assert_eq!(e.scopes().subscription_count(), 1);
}

#[test]
fn no_scope_work_without_subscribers() {
    let mut e = engine();
    let px = gradient64();
    for _ in 0..3 {
        e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    }
    assert_eq!(
        e.scopes().gpu_invocations(),
        0,
        "zero subscribers must mean zero GPU scope work"
    );
    assert!(
        e.drain_scope_frames().is_empty(),
        "no frames queued without subscribers"
    );

    // Subscribing starts the work.
    e.scopes_subscribe("cam1", ScopeKind::Waveform).unwrap();
    e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    assert!(
        e.scopes().gpu_invocations() > 0,
        "a subscribed scope must run on the GPU"
    );
    let frames = e.drain_scope_frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].input_id, "cam1");
    assert_eq!(frames[0].kind, ScopeKind::Waveform);
    assert_eq!(frames[0].seq, 1);
    assert!(matches!(frames[0].payload, ScopePayload::Waveform(_)));

    // Unsubscribing stops it again.
    e.scopes_unsubscribe("cam1", ScopeKind::Waveform);
    let before = e.scopes().gpu_invocations();
    e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    assert_eq!(
        e.scopes().gpu_invocations(),
        before,
        "no scope work after unsubscribe"
    );
    assert!(e.drain_scope_frames().is_empty());
}

#[test]
fn scope_cadence_throttles_to_15hz() {
    let mut e = engine();
    let px = gradient64();
    e.scopes_subscribe("cam1", ScopeKind::Histogram).unwrap();

    // Deterministic throttle test: pin the hub's clock and advance it
    // 16.6 ms per frame (a 60 Hz feed). Five frames span 4 x 16.6 = 66.4 ms,
    // just under the 66.667 ms throttle period, so exactly the first frame
    // may emit — regardless of how slow the (software GL) frame processing
    // actually is. The old wall-clock version of this test flaked ~1-in-5
    // on cold lavapipe when the 5 frames spanned the real 66.7 ms boundary.
    e.scopes_mut().set_manual_clock(Instant::now());
    for _ in 0..5 {
        e.grade_input_frame("cam1", 64, 64, &px).unwrap();
        e.scopes_mut()
            .advance_manual_clock(Duration::from_micros(16_600));
    }
    let frames = e.drain_scope_frames();
    assert_eq!(
        frames.len(),
        1,
        "60 Hz frame feed must throttle scopes to ~15 Hz, got {}",
        frames.len()
    );
    assert_eq!(frames[0].seq, 1);

    // After a full throttle period elapses, the next frame emits again.
    e.scopes_mut()
        .advance_manual_clock(Duration::from_millis(80));
    e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    let frames = e.drain_scope_frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].seq, 2);
}

#[test]
fn scopes_tap_the_graded_frame() {
    let mut e = engine();
    let px = gradient64();
    e.scopes_subscribe("cam1", ScopeKind::Waveform).unwrap();
    // Pin the throttle clock so the second frame is guaranteed to be past
    // the 15 Hz budget — no wall-clock sleep needed.
    e.scopes_mut().set_manual_clock(Instant::now());

    // Identity grade: baseline waveform.
    e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    let base = e.drain_scope_frames();
    let base_max = match &base[0].payload {
        ScopePayload::Waveform(w) => w.luma_max[0],
        _ => panic!("expected waveform"),
    };

    // +1 EV grade: the scope must see the BRIGHTER graded frame.
    e.scopes_mut()
        .advance_manual_clock(Duration::from_millis(80));
    e.state_mut()
        .input_color_mut("cam1")
        .expect("cam1 exists")
        .grade
        .exposure = 1.0;
    e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    let graded = e.drain_scope_frames();
    let graded_max = match &graded[0].payload {
        ScopePayload::Waveform(w) => w.luma_max[0],
        _ => panic!("expected waveform"),
    };
    println!("waveform luma_max[0]: identity={base_max:.3} graded={graded_max:.3}");
    assert!(
        graded_max > base_max,
        "scopes must analyze the graded frame: {base_max} -> {graded_max}"
    );
}

#[test]
fn all_scope_kinds_emit_payloads() {
    let mut e = engine();
    let px = gradient64();
    for kind in ScopeKind::all() {
        e.scopes_subscribe("cam1", kind).unwrap();
    }
    e.grade_input_frame("cam1", 64, 64, &px).unwrap();
    let mut frames = e.drain_scope_frames();
    assert_eq!(frames.len(), 5, "one frame per subscribed kind");
    frames.sort_by_key(|f| f.kind.as_str().to_string());
    for f in &frames {
        assert_eq!(f.payload.kind(), f.kind, "payload must match its kind");
        assert_eq!(f.seq, 1);
    }
    // Five analyses ran on the GPU for one frame (one shared downscale).
    assert_eq!(e.scopes().gpu_invocations(), 5);
}
