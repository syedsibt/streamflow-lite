//! Long-run resource benchmark (ignored; run on demand):
//!
//! `BENCH_SECS=600 BENCH_SCENARIO=busy BENCH_MONITOR_PX=590 BENCH_OUT=run.csv cargo test -p sf-ui-backend --release --test bench_longrun -- --ignored --nocapture`
//!
//! Mirrors the desktop app's threading: the engine sits behind one mutex;
//! a media-tick thread runs at a fixed 30 fps deadline clock (a tick that
//! misses its slot counts as late, exactly like the app); a UI thread does
//! what the UI triggers (program monitor 30 Hz with JPEG encode outside the
//! lock, meters 25 Hz, diagnostics 1 Hz, state 0.2 Hz, preview scene 10 Hz
//! in `busy`). Every 10 s: process CPU, RSS and app caches. Growth that does
//! not level off is a leak.
//!
//! Scenarios: `idle` = one test signal (the reporting user's screen);
//! `static` = colour background + PNG + text (nothing moves — the v0.1.28
//! report's screenshot);
//! `busy` = 720p video file (with audio) + test-signal PiP + PNG logo +
//! text, a second scene in preview, and a 20 s simulated PiP drag.
//! `BENCH_MONITOR_PX` = drawn monitor width the UI reports (omit = default).

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sf_ui_backend::{Shell, SourceTransform};

fn tmp(name: &str) -> String {
    std::env::temp_dir().join(name).to_string_lossy().into_owned()
}

fn proc_cpu_s() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let Some(i) = stat.rfind(')') else { return 0.0 };
    let f: Vec<&str> = stat[i + 2..].split_whitespace().collect();
    let u: f64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let s: f64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    (u + s) / 100.0
}

fn rss_mb() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    pages * 4096.0 / 1048576.0
}

fn lock(m: &Mutex<Shell>) -> std::sync::MutexGuard<'_, Shell> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sum of app cache bytes (`BENCH_OLD` builds of v0.1.27 have no report).
fn cache_bytes(s: &Shell) -> u64 {
    // BENCH_OLD_START
    s.resource_report().iter().map(|c| c.bytes).sum()
    // BENCH_OLD_END
}

#[test]
#[ignore]
fn longrun_resources() {
    let secs: u64 = std::env::var("BENCH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
    let scenario = std::env::var("BENCH_SCENARIO").unwrap_or_else(|_| "idle".into());
    let busy = scenario == "busy";
    let still = scenario == "static";
    let out = std::env::var("BENCH_OUT").ok();
    let mut s = Shell::new().expect("engine");
    let a = s.scene_create("A").id;
    if still {
        s.source_add(&a, "color", "Blue", [0, 115, 255], None, None, None, None, 0, 0, 0).unwrap();
        let png = tmp("sf-bench-still.png");
        let mut px = vec![0u8; 400 * 300 * 4];
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            p.copy_from_slice(&[(i % 400) as u8, 120, 200, 255]);
        }
        image::save_buffer(&png, &px, 400, 300, image::ExtendedColorType::Rgba8).unwrap();
        s.source_add(&a, "image", "download.png", [0; 3], None, Some(&png), None, None, 0, 0, 0).unwrap();
        s.source_add(&a, "text", "Title", [0; 3], Some("StreamFlow"), None, None, None, 0, 0, 0).unwrap();
    }
    let pat = if still {
        s.scene_list().into_iter().find(|x| x.id == a).unwrap().sources[0].clone()
    } else {
        s.source_add(&a, "test_pattern", "Test", [0; 3], None, None, None, None, 0, 0, 0).unwrap()
    };
    let mut b_scene = None;
    if busy {
        let media = tmp("sf-bench-720p.mp4");
        if !std::path::Path::new(&media).exists() {
            let st = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=900", "-f", "lavfi", "-i", "sine=frequency=440:duration=900"])
                .args(["-pix_fmt", "yuv420p", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest", &media])
                .status()
                .unwrap();
            assert!(st.success());
        }
        s.source_add(&a, "media", "Clip", [0; 3], None, Some(&media), None, None, 0, 0, 0).unwrap();
        let logo = tmp("sf-bench-logo.png");
        let mut px = vec![0u8; 200 * 100 * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[240, 200, 40, 200]);
        }
        image::save_buffer(&logo, &px, 200, 100, image::ExtendedColorType::Rgba8).unwrap();
        s.source_add(&a, "image", "Logo", [0; 3], None, Some(&logo), None, None, 0, 0, 0).unwrap();
        s.source_add(&a, "text", "Lower third", [0; 3], Some("Ali Khan — Host"), None, None, None, 0, 0, 0).unwrap();
        let ids: Vec<String> = s.scene_list().into_iter().find(|x| x.id == a).unwrap().sources.iter().map(|x| x.id.clone()).collect();
        s.source_reorder(&a, vec![ids[1].clone(), ids[0].clone(), ids[2].clone(), ids[3].clone()]).unwrap();
        s.source_set_transform(&a, &pat.id, Some(SourceTransform::rect(0.66, 0.66, 0.3, 0.3))).unwrap();
        let b = s.scene_duplicate(&a).unwrap().id;
        s.production_set_preview(&b).unwrap();
        b_scene = Some(b);
    }
    s.production_set_program(&a).unwrap();
    // BENCH_OLD_START
    if let Some(px) = std::env::var("BENCH_MONITOR_PX").ok().and_then(|v| v.parse().ok()) {
        println!("monitor size {:?}", s.monitor_set_display_width(px));
    }
    // BENCH_OLD_END

    let shell = Arc::new(Mutex::new(s));
    let stop = Arc::new(AtomicBool::new(false));
    let late = Arc::new(AtomicU64::new(0));
    let tick_us = Arc::new(AtomicU64::new(0));
    let ui_us = Arc::new(AtomicU64::new(0));
    println!("scenario {scenario} for {secs}s");

    // Media tick: same absolute-deadline clock as the app.
    let tick_thread = {
        let (shell, stop, late, tick_us) = (shell.clone(), stop.clone(), late.clone(), tick_us.clone());
        std::thread::spawn(move || {
            let interval = Duration::from_micros(33_333);
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                lock(&shell).av_tick();
                tick_us.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
                next += interval;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    let over = now.duration_since(next).as_micros() as u64;
                    let missed = over / interval.as_micros() as u64 + 1;
                    late.fetch_add(missed, Ordering::Relaxed);
                    next += interval * missed as u32;
                }
            }
        })
    };

    // UI: what the desktop UI triggers, on its own thread; JPEG encode is
    // done after releasing the lock (as `monitor_frame_if_new` does).
    let ui_thread = {
        let (shell, stop, ui_us) = (shell.clone(), stop.clone(), ui_us.clone());
        let (a, pat, b_scene) = (a.clone(), pat.id.clone(), b_scene.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut n = 0u64;
            let mut last_seq = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                let frame = {
                    let mut g = lock(&shell);
                    let prog = g.production_state().program_scene_id.clone();
                    g.monitor_snapshot(&prog).ok()
                };
                // Like `monitor_frame_if_new`: only a new frame is encoded
                // and sent.
                if let Some(f) = frame.filter(|f| f.seq > last_seq) {
                    last_seq = f.seq;
                    let _ = f.to_jpeg();
                }
                let _ = lock(&shell).audio_meters();
                if let Some(b) = &b_scene {
                    if n % 3 == 0 {
                        let f = lock(&shell).monitor_snapshot(b).ok();
                        if let Some(f) = f {
                            let _ = f.to_jpeg();
                        }
                    }
                    if start.elapsed() < Duration::from_secs(20) {
                        let k = 0.2 + (n % 60) as f32 / 300.0;
                        let _ = lock(&shell).source_set_transform(&a, &pat, Some(SourceTransform::rect(0.6, 0.6, k, k)));
                    }
                }
                if n % 30 == 0 {
                    let _ = lock(&shell).diagnostics();
                }
                if n % 150 == 0 {
                    let _ = lock(&shell).production_state();
                }
                let _ = lock(&shell).drain_events();
                ui_us.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
                n += 1;
                let spent = t0.elapsed();
                if spent < Duration::from_millis(33) {
                    std::thread::sleep(Duration::from_millis(33) - spent);
                }
            }
        })
    };

    let start = Instant::now();
    let mut last_cpu = proc_cpu_s();
    let mut last = Instant::now();
    let mut csv = String::from("t_s,cpu_pct,rss_mb,cache_mb,late_ticks,tick_ms_avg,ui_ms_avg\n");
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(10));
        let cpu = proc_cpu_s();
        let wall = last.elapsed().as_secs_f64();
        last = Instant::now();
        let pct = (cpu - last_cpu) / wall * 100.0; // % of ONE core
        last_cpu = cpu;
        let cache = cache_bytes(&lock(&shell));
        let line = format!(
            "{:.0},{:.1},{:.1},{:.2},{},{:.2},{:.2}",
            start.elapsed().as_secs_f64(),
            pct,
            rss_mb(),
            cache as f64 / 1048576.0,
            late.load(Ordering::Relaxed),
            tick_us.swap(0, Ordering::Relaxed) as f64 / 1000.0 / (wall * 30.0),
            ui_us.swap(0, Ordering::Relaxed) as f64 / 1000.0 / (wall * 30.0),
        );
        println!("{line}");
        csv.push_str(&line);
        csv.push('\n');
    }
    stop.store(true, Ordering::Relaxed);
    let _ = tick_thread.join();
    let _ = ui_thread.join();
    if let Some(p) = out {
        std::fs::write(p, csv).unwrap();
    }
}
