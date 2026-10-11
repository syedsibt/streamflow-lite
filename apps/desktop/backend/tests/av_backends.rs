//! Integration tests for the real AV backends wired into the shell:
//! media-file sources (decode → composite → meters), recording the
//! program bus to MP4, RTMP streaming attempts, and live audio meters.
//! All against the real engine (headless lavapipe) and real FFmpeg.

use std::path::Path;
use std::process::Command;

use sf_ui_backend::{RecordLifecycle, Shell, StreamLifecycle};

fn shell() -> Shell {
    Shell::new().expect("headless engine must open (lavapipe)")
}

/// Generate a small test file with video + audio using the ffmpeg CLI.
fn make_test_media(path: &str) {
    let _ = std::fs::remove_file(path);
    let st = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            path,
        ])
        .status()
        .expect("ffmpeg CLI must exist");
    assert!(st.success(), "ffmpeg failed to generate test media");
    assert!(Path::new(path).exists(), "test media not written");
}

fn scene_with_pattern(s: &mut Shell, name: &str) -> String {
    let sc = s.scene_create(name).id;
    s.source_add(&sc, "test_pattern", "pat", [0, 0, 0], None, None, None, 0, 0, 0)
        .expect("add test pattern source");
    s.production_set_program(&sc).expect("set program");
    sc
}

#[test]
fn record_produces_real_mp4() {
    let mut s = shell();
    scene_with_pattern(&mut s, "Rec");
    let out = "/tmp/sf-be-record.mp4";
    let _ = std::fs::remove_file(out);
    s.record_set_settings(out, "mp4", "High").unwrap();
    s.record_start().expect("record_start");
    for _ in 0..5 {
        s.av_tick();
    }
    s.record_stop().expect("record_stop");
    // Lifecycle is back to idle (or recording, if ticks are still draining).
    assert!(matches!(
        s.production_state().record_lifecycle,
        RecordLifecycle::Idle
    ));
    let meta = std::fs::metadata(out).expect("recording file exists");
    assert!(meta.len() > 1_000, "recording too small: {}", meta.len());
    // It is a real MP4: ffprobe finds an h264 video stream with exactly
    // the 5 frames we fed it (proves render → encode → flush → mux).
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,nb_read_frames",
            "-of",
            "csv=p=0",
            out,
        ])
        .output()
        .expect("ffprobe");
    let txt = String::from_utf8_lossy(&probe.stdout);
    assert!(
        txt.contains("h264"),
        "expected h264 stream in recording, got: {txt}"
    );
    assert!(
        txt.contains(",5"),
        "expected 5 frames in recording, got: {txt}"
    );
}

#[test]
fn record_requires_output_path() {
    let mut s = shell();
    scene_with_pattern(&mut s, "Rec");
    s.record_set_settings("", "mp4", "High").unwrap();
    let err = s.record_start().expect_err("empty path must fail");
    assert!(err.to_string().contains("no recording output path"));
    assert!(matches!(
        s.production_state().record_lifecycle,
        RecordLifecycle::Error { .. }
    ));
}

#[test]
fn stream_start_unreachable_fails_cleanly_without_key_leak() {
    use sf_output::ReconnectPolicy;
    let mut s = shell();
    s.stream_set_settings(
        "rtmp://127.0.0.1:1935/live",
        "secret-key-xyz",
        "custom_rtmp",
    )
    .unwrap();
    // Connect now happens on the async send worker: start returns Ok and
    // the failure surfaces through the lifecycle.
    s.stream_start_with_policy(ReconnectPolicy {
        max_attempts: 0,
        ..ReconnectPolicy::default()
    })
    .expect("async start must not fail synchronously");
    // Poll the worker until it reports the (fast-failing) result.
    let mut lifecycle = s.production_state().stream_lifecycle;
    for _ in 0..200 {
        s.stream_drain_worker();
        lifecycle = s.production_state().stream_lifecycle;
        if matches!(lifecycle, StreamLifecycle::Error { .. }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    match &lifecycle {
        StreamLifecycle::Error { message } => assert!(
            !message.contains("secret-key-xyz"),
            "stream key leaked in lifecycle: {message}"
        ),
        other => panic!("expected Error lifecycle, got {other:?}"),
    }
    // Recovery path stays valid.
    s.stream_stop().expect("stream_stop");
    assert!(matches!(
        s.production_state().stream_lifecycle,
        StreamLifecycle::Idle
    ));
}

#[test]
fn media_source_renders_and_meters_go_live() {
    let media = "/tmp/sf-be-media.mp4";
    make_test_media(media);
    let mut s = shell();
    let sc = s.scene_create("Media").id;
    let src = s
        .source_add(&sc, "media", "clip", [0, 0, 0], None, Some(media), None, 0, 0, 0)
        .expect("add media source");
    assert_eq!(src.status, "ready", "media source should be ready");
    s.production_set_program(&sc).unwrap();
    // Tick: video frames flow into the composite, audio into the mixer.
    for _ in 0..8 {
        s.av_tick();
    }
    let st = s.production_state();
    assert!(
        st.mixer.meters_live,
        "meters should be live with media audio flowing"
    );
    let peak = st.mixer.meters.get(&src.id).copied().unwrap_or(0.0);
    assert!(peak > 0.01, "media source meter should move, got {peak}");
    // Program frame carries real decoded pixels (non-trivial PNG).
    let png = s.program_frame_png(320, 240).expect("program frame");
    assert!(png.len() > 1_000, "program PNG suspiciously small");
}

#[test]
fn media_source_bad_path_fails_cleanly() {
    let mut s = shell();
    let sc = s.scene_create("Media").id;
    let err = s
        .source_add(
            &sc,
            "media",
            "clip",
            [0, 0, 0],
            None,
            Some("/tmp/does-not-exist.mp4"),
            None,
            0,
            0,
            0,
        )
        .expect_err("bad path must fail");
    assert!(err.to_string().contains("cannot open media"));
    // No half-added source left behind.
    assert!(s
        .scene_list()
        .iter()
        .find(|x| x.id == sc)
        .unwrap()
        .sources
        .is_empty());
}
