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
    make_test_media_len(path, 2, "320x240");
}

fn make_test_media_len(path: &str, seconds: u32, size: &str) {
    let _ = std::fs::remove_file(path);
    let st = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={size}:rate=30:duration={seconds}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={seconds}"),
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
    s.source_add(&sc, "test_pattern", "pat", [0, 0, 0], None, None, None, None, 0, 0, 0)
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
    // Five ticks on the media clock, one frame interval apart.
    let t0 = sf_ui_backend::media_clock_us(std::time::Instant::now());
    for i in 0..5 {
        s.av_tick_at(t0 + i * 33_333);
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
        .source_add(&sc, "media", "clip", [0, 0, 0], None, Some(media), None, None, 0, 0, 0)
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

#[test]
fn program_monitor_serves_live_jpeg_from_tick_cache() {
    let mut s = shell();
    let sc = scene_with_pattern(&mut s, "Mon");
    // Tick a few times: the idle tick renders the program monitor frame.
    for _ in 0..4 {
        s.av_tick();
    }
    // Program scene is served from the tick cache as JPEG (magic bytes
    // FF D8 FF), without re-rendering.
    let jpg = s.scene_monitor_jpg(&sc).expect("program monitor jpg");
    assert!(
        jpg.len() > 1_000 && jpg[0] == 0xFF && jpg[1] == 0xD8 && jpg[2] == 0xFF,
        "expected JPEG bytes, got {} bytes starting {:02X?}",
        jpg.len(),
        &jpg[..3.min(jpg.len())]
    );
    // A second scene (not program) renders on demand and also yields JPEG.
    let other = s.scene_create("Other").id;
    let jpg2 = s.scene_monitor_jpg(&other).expect("other scene jpg");
    assert!(
        jpg2.len() > 100 && jpg2[0] == 0xFF && jpg2[1] == 0xD8,
        "expected JPEG bytes for non-program scene"
    );
}

#[test]
fn source_audio_device_labels() {
    let mut s = shell();
    let sc = s.scene_create("S").id;
    // Audio input source with an explicit device.
    let mic = s
        .source_add(
            &sc, "audio_input", "Mic", [0, 0, 0], None, None, Some("My USB Mic"), None, 0, 0, 0,
        )
        .expect("add mic")
        .id;
    assert_eq!(s.source_audio_device(&mic), Some("My USB Mic".to_string()));
    // Camera without audio device → None.
    let cam = s
        .source_add(
            &sc, "camera", "Cam", [0, 0, 0], None, None, None, None, 1280, 720, 30,
        )
        .expect("add camera")
        .id;
    assert_eq!(s.source_audio_device(&cam), None);
    // Camera with an audio device keeps it.
    let cam2 = s
        .source_add(
            &sc, "camera", "Cam2", [0, 0, 0], None, None, None, Some("Capture Card Audio"), 1280,
            720, 30,
        )
        .expect("add camera2")
        .id;
    assert_eq!(
        s.source_audio_device(&cam2),
        Some("Capture Card Audio".to_string())
    );
    // Reconfigure with the same audio device keeps it; None clears it.
    s.camera_reconfigure(&cam2, None, 1280, 720, 30, Some("Capture Card Audio".to_string()))
        .expect("reconfigure keeps audio device");
    assert_eq!(
        s.source_audio_device(&cam2),
        Some("Capture Card Audio".to_string())
    );
    s.camera_reconfigure(&cam2, None, 1280, 720, 30, None)
        .expect("reconfigure clears audio device");
    assert_eq!(s.source_audio_device(&cam2), None);
    // Media source → None.
    let pat = s
        .source_add(
            &sc, "test_pattern", "Pat", [0, 0, 0], None, None, None, None, 0, 0, 0,
        )
        .expect("add pattern")
        .id;
    assert_eq!(s.source_audio_device(&pat), None);
    // Unknown id → None.
    assert_eq!(s.source_audio_device("nope"), None);
    // Monitor starts off; enabling with no output device returns Err
    // (never panics) — on this headless VM there is no device.
    assert!(!s.monitor_enabled());
    let _ = s.set_monitor_enabled(true);
}

fn ffprobe_streams(path: &str) -> Vec<(String, f64, f64)> {
    // (codec_type, start_time, duration) per stream
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,start_time,duration",
            "-of",
            "csv=p=0",
            path,
        ])
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some((
                f.first()?.to_string(),
                f.get(1)?.parse().unwrap_or(0.0),
                f.get(2)?.parse().unwrap_or(f64::NAN),
            ))
        })
        .collect()
}

/// Packet-level duration (works for FLV, where stream duration is N/A).
fn ffprobe_span(path: &str, kind: &str) -> (f64, f64, usize) {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            kind,
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
            path,
        ])
        .output()
        .expect("ffprobe");
    let ts: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect();
    let first = ts.iter().cloned().fold(f64::INFINITY, f64::min);
    let last = ts.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    (first, last, ts.len())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// End to end: media → grade → ONE encoder → RTMP (local FFmpeg server)
/// and, joining 2 s later, a recording from the same encoder.
///
/// A 400 ms stall is injected into the tick loop (plus whatever load this
/// 2-core VM adds). The stream's media timeline must still match the wall
/// clock: with the old frame-counted PTS every skipped tick shortened the
/// stream timeline, which is how viewers ended up a minute behind.
#[test]
fn stream_and_record_share_one_encoder_and_follow_wall_clock() {
    use std::time::{Duration, Instant};
    let media = "/tmp/sf-e2e-media.mp4";
    make_test_media_len(media, 12, "1280x720");
    let port = free_port();
    let received = format!("/tmp/sf-e2e-rtmp-{port}.flv");
    let rec_path = "/tmp/sf-e2e-record.mp4";
    let _ = std::fs::remove_file(&received);
    let _ = std::fs::remove_file(rec_path);
    let mut server = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-listen",
            "1",
            "-i",
            &format!("rtmp://127.0.0.1:{port}/live/e2ekey"),
            "-c",
            "copy",
            &received,
        ])
        .spawn()
        .expect("ffmpeg rtmp server");
    std::thread::sleep(Duration::from_millis(700));

    let mut s = shell();
    let sc = s.scene_create("E2E").id;
    s.source_add(&sc, "media", "clip", [0, 0, 0], None, Some(media), None, None, 0, 0, 0)
        .expect("media source");
    s.production_set_program(&sc).unwrap();
    let mut vs = s.video_settings_get();
    vs.output_width = 1280;
    vs.output_height = 720;
    vs.output_fps = 30;
    vs.encoder = "libx264".into();
    vs.bitrate_kbps = 2500;
    s.video_settings_set(vs).unwrap();
    s.stream_set_settings(&format!("rtmp://127.0.0.1:{port}/live"), "e2ekey", "custom_rtmp")
        .unwrap();
    s.record_set_settings(rec_path, "mp4", "High").unwrap();
    s.stream_start().expect("stream_start");

    // Same deadline clock as the app's media thread.
    let interval = Duration::from_micros(33_333);
    let start = Instant::now();
    let mut next = start;
    let mut live_at: Option<Instant> = None;
    let mut rec_started = false;
    let mut ticks = 0u32;
    let mut lagged = 0u64;
    let mut stalled = false;
    let run = Duration::from_secs(5);
    let mut live_elapsed = Duration::ZERO;
    while live_elapsed < run && start.elapsed() < Duration::from_secs(20) {
        s.av_tick_at(sf_ui_backend::media_clock_us(next));
        ticks += 1;
        if live_at.is_none()
            && matches!(s.production_state().stream_lifecycle, StreamLifecycle::Live)
        {
            live_at = Some(next);
        }
        if let Some(t) = live_at {
            live_elapsed = next.duration_since(t);
            if !stalled && live_elapsed > Duration::from_millis(1500) {
                stalled = true;
                std::thread::sleep(Duration::from_millis(400)); // injected stall
            }
            if !rec_started && live_elapsed > Duration::from_secs(2) {
                rec_started = true;
                s.record_start().expect("record_start while streaming");
            }
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            let missed = sf_ui_backend::missed_intervals(
                now.duration_since(next).as_micros(),
                33_333,
            );
            next += interval * missed as u32;
            lagged += missed;
        }
    }
    let live_at = live_at.expect("stream never went live");
    let wall_live = next.duration_since(live_at).as_secs_f64();
    let rec_wall = wall_live - 2.0;
    let diag = s.diagnostics();
    eprintln!(
        "e2e: ticks={ticks} lagged={lagged} encoder={:?} render_fps={} stages ms: capture={:.1} render={:.1} gpu={:.1} audio={:.1} encode={:.1} latency={:?}",
        diag.encoder.as_ref().map(|e| (&e.name, e.encode_ms, e.fps)),
        diag.render_fps,
        diag.capture_ms,
        diag.render_ms,
        diag.gpu_ms,
        diag.audio_ms,
        diag.encode_ms,
        diag.latency,
    );
    assert!(diag.encoder.is_some(), "encoder diagnostics missing");
    s.record_stop().unwrap();
    s.stream_stop().unwrap();
    // The server exits when the publisher disconnects.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if server.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = server.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let (v0, v1, vn) = ffprobe_span(&received, "v");
    let (a0, a1, an) = ffprobe_span(&received, "a");
    eprintln!("rtmp received: video {v0:.3}..{v1:.3} ({vn} pkts), audio {a0:.3}..{a1:.3} ({an} pkts), wall {wall_live:.3}s");
    assert!(vn > 20 && an > 20, "stream carried too little media");
    // Timeline = wall clock (±10 %), even with the stall and load.
    let vspan = v1 - v0;
    assert!(
        (vspan - wall_live).abs() < wall_live * 0.10 + 0.1,
        "video timeline {vspan:.2}s vs wall {wall_live:.2}s — stream drifts from real time"
    );
    let aspan = a1 - a0;
    assert!(
        (aspan - wall_live).abs() < wall_live * 0.10 + 0.1,
        "audio timeline {aspan:.2}s vs wall {wall_live:.2}s"
    );
    assert!((v0 - a0).abs() < 0.15, "A/V start offset {:.3}s", v0 - a0);

    // Recording joined late: starts at 0 on a keyframe, same encoder.
    let streams = ffprobe_streams(rec_path);
    eprintln!("recording streams: {streams:?}");
    let (r0, r1, rn) = ffprobe_span(rec_path, "v");
    assert!(rn > 10, "recording has too few frames");
    assert!(r0.abs() < 0.05, "recording should start at 0, starts at {r0}");
    assert!(
        ((r1 - r0) - rec_wall).abs() < rec_wall * 0.15 + 0.15,
        "recording span {:.2}s vs wall {rec_wall:.2}s",
        r1 - r0
    );
}

/// Measurement (run explicitly, ideally `--release`):
/// `cargo test --release --test av_backends latency_bench -- --ignored --nocapture`
///
/// Streams 1080p30 media through the full pipeline to a local RTMP
/// receiver (ffprobe printing every packet as it arrives) for
/// `SF_BENCH_SECS` (default 20 s) with a 500 ms stall injected every 5 s.
/// Reports per-stage timings and the packet delay growth: arrival time vs
/// media timestamp, relative to the first packet. A growing backlog shows
/// up as a rising delay; a live-first pipeline stays flat.
#[test]
#[ignore]
fn latency_bench() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let secs: u64 = std::env::var("SF_BENCH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    let enc = std::env::var("SF_BENCH_ENCODER").unwrap_or_else(|_| "libx264".into());
    let media = "/tmp/sf-bench-media.mp4";
    if !Path::new(media).exists() {
        make_test_media_len(media, 60, "1920x1080");
    }
    let port = free_port();
    let mut probe = Command::new("ffprobe")
        .args([
            "-v", "error", "-probesize", "32", "-analyzeduration", "0",
            "-fflags", "nobuffer", "-listen", "1", "-i",
            &format!("rtmp://127.0.0.1:{port}/live/k"),
            "-show_entries", "packet=codec_type,pts_time", "-of", "csv=p=0",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffprobe");
    let stdout = probe.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<(Instant, String, f64)>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let now = Instant::now();
            let mut f = line.split(',');
            let kind = f.next().unwrap_or("").to_string();
            if let Some(t) = f.next().and_then(|v| v.parse::<f64>().ok()) {
                let _ = tx.send((now, kind, t));
            }
        }
    });
    std::thread::sleep(Duration::from_millis(700));
    let mut s = shell();
    let sc = s.scene_create("Bench").id;
    s.source_add(&sc, "media", "clip", [0, 0, 0], None, Some(media), None, None, 0, 0, 0).unwrap();
    s.production_set_program(&sc).unwrap();
    let mut vs = s.video_settings_get();
    vs.output_width = 1920;
    vs.output_height = 1080;
    vs.output_fps = 30;
    vs.encoder = enc;
    vs.bitrate_kbps = 6000;
    s.video_settings_set(vs).unwrap();
    s.stream_set_settings(&format!("rtmp://127.0.0.1:{port}/live"), "k", "custom_rtmp").unwrap();
    s.stream_start().unwrap();

    let interval = Duration::from_micros(33_333);
    let start = Instant::now();
    let mut next = start;
    let mut lagged = 0u64;
    let mut stalls = 0u32;
    let mut stage_sum = [0f64; 6];
    let mut n = 0f64;
    let mut max_tick = 0f32;
    let mut cpu_samples: Vec<f32> = Vec::new();
    while start.elapsed() < Duration::from_secs(secs) {
        s.av_tick_at(sf_ui_backend::media_clock_us(next));
        let d = s.diagnostics();
        if d.stream_status == "live" {
            for (i, v) in [d.capture_ms, d.render_ms, d.gpu_ms, d.audio_ms, d.encode_ms, d.tick_ms].iter().enumerate() {
                stage_sum[i] += *v as f64;
            }
            n += 1.0;
            max_tick = max_tick.max(d.tick_ms);
            cpu_samples.push(d.system.cpu_percent);
        }
        let el = start.elapsed().as_secs();
        if el > 0 && el % 5 == 0 && stalls < (el / 5) as u32 {
            stalls += 1;
            std::thread::sleep(Duration::from_millis(500));
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            let m = sf_ui_backend::missed_intervals(now.duration_since(next).as_micros(), 33_333);
            next += interval * m as u32;
            lagged += m;
        }
    }
    let d = s.diagnostics();
    s.stream_stop().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let _ = probe.kill();
    let pkts: Vec<(Instant, String, f64)> = rx.try_iter().filter(|p| p.1 == "video").collect();
    assert!(pkts.len() > 30, "too few packets received: {}", pkts.len());
    let (t0, _, p0) = pkts[0].clone();
    let delays: Vec<f64> = pkts
        .iter()
        .map(|(t, _, p)| t.duration_since(t0).as_secs_f64() - (p - p0))
        .collect();
    let q = |a: &[f64]| {
        let mut v = a.to_vec();
        v.sort_by(|x, y| x.partial_cmp(y).unwrap());
        (v[v.len() / 2], v[v.len() * 95 / 100], v[v.len() - 1])
    };
    let half = delays.len() / 2;
    let (m1, _, _) = q(&delays[..half]);
    let (m2, p95, mx) = q(&delays[half..]);
    let names = ["capture", "render", "gpu", "audio", "encode+send", "tick"];
    eprintln!("=== latency_bench: {secs}s 1080p30, encoder {:?}, {} video pkts, lagged ticks {lagged}, injected stalls {stalls}", d.encoder.as_ref().map(|e| e.name.clone()), pkts.len());
    for (i, nme) in names.iter().enumerate() {
        eprintln!("  {nme:>12}: {:.2} ms avg", stage_sum[i] / n.max(1.0));
    }
    eprintln!("  max tick {max_tick:.1} ms; process CPU avg {:.0}% of {} cores; RAM {:.0} MB", cpu_samples.iter().sum::<f32>() / cpu_samples.len().max(1) as f32, d.system.cores, d.system.ram_mb);
    eprintln!("  delay vs first packet: 1st half median {:.0} ms | 2nd half median {:.0} ms, p95 {:.0} ms, max {:.0} ms", m1 * 1e3, m2 * 1e3, p95 * 1e3, mx * 1e3);
    // No backlog: the delay must not grow over the run.
    assert!(m2 - m1 < 0.25, "delay grew by {:.0} ms — backlog building", (m2 - m1) * 1e3);
}
