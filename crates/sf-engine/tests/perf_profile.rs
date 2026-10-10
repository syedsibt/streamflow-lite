//! C8 GPU pipeline profiler (worker 1).
//!
//! Ignored by default — run explicitly:
//!
//! ```sh
//! export PATH="$HOME/.cargo/bin:$PATH"
//! export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json
//! cargo test -p sf-engine --test perf_profile -- --ignored --nocapture
//! ```
//!
//! `STREAMFLOW_PROFILE_FRAMES` overrides the measured frame count (default
//! 30, plus 3 warmup frames).
//!
//! This test measures the per-pass cost of the full engine pipeline —
//! upload -> grade -> effect slots -> output encode -> download -> scopes —
//! on a 1080p synthetic frame with a representative stack (teal-orange look
//! folded into the grade + vignette + film grain + sharpen, waveform and
//! vectorscope subscribed). It drives the public building blocks
//! ([`RenderContext`], [`GradePass`], [`EffectPass`], [`ScopePass`]) through
//! exactly the dispatch sequence [`Engine::grade_input_frame`] executes, and
//! then asserts its output is **byte-identical** to the engine's output for
//! the same stack — so the replica is proven to measure the real pipeline,
//! not a lookalike.
//!
//! Timing method: CPU wall-clock (`Instant`) around each stage with
//! `device.poll(wgpu::Maintain::Wait)` before stopping the clock, forcing
//! GPU completion. No timestamp queries: the wall-clock method is
//! backend-agnostic (DirectX 12 is the shipping target) and works on
//! lavapipe, the headless test backend. lavapipe is a SOFTWARE rasterizer,
//! so absolute timings here are relative numbers for pass-vs-pass
//! comparison, not predictions of real-GPU performance.

use std::path::PathBuf;
use std::time::Instant;

use sf_engine::Engine;
use sf_filterfx::EffectPass;
use sf_filters::{EffectKind, Filter, FilterSlot, FilterStack, LookRegistry};
use sf_render::{GradePass, RenderContext};
use sf_scopes::{ScopeKind, ScopeParams, ScopePass};
use uuid::Uuid;

const W: u32 = 1920;
const H: u32 = 1080;
const WARMUP: usize = 3;

fn looks_dir() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.pop(); // crates/sf-engine
    d.pop(); // streamflow-lite
    d.join("assets/looks")
}

/// 1080p synthetic test frame: r/g gradients + checkerboard + a warm patch
/// (gives the grain/grade shaders something varied to chew on).
fn synthetic_frame(w: u32, h: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = ((x as f32 / w as f32) * 255.0) as u8;
            let g = ((y as f32 / h as f32) * 255.0) as u8;
            let b = ((((x / 16) + (y / 16)) % 2) as u8) * 96 + 32;
            px.extend_from_slice(&[r, g, b, 255]);
        }
    }
    // A warm "skin-ish" patch so skin-adjacent math paths see real data.
    for y in (h / 3)..(2 * h / 3) {
        for x in (w / 3)..(2 * w / 3) {
            let i = ((y * w + x) * 4) as usize;
            px[i] = 200;
            px[i + 1] = 150;
            px[i + 2] = 120;
        }
    }
    px
}

fn effect_slot(kind: EffectKind, intensity: f32) -> FilterSlot {
    FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Effect {
            kind: kind.clone(),
            params: kind.default_params(),
        },
        enabled: true,
        intensity,
        mask: None,
    }
}

fn look_slot(preset_id: &str, intensity: f32) -> FilterSlot {
    FilterSlot {
        id: Uuid::new_v4(),
        filter: Filter::Look {
            preset_id: preset_id.to_string(),
            intensity,
        },
        enabled: true,
        intensity: 1.0,
        mask: None,
    }
}

fn set_stack(engine: &mut Engine, input_id: &str, slots: Vec<FilterSlot>) {
    let input = engine
        .state_mut()
        .inputs
        .iter_mut()
        .find(|i| i.id == input_id)
        .expect("input exists");
    let mut stack = FilterStack::new();
    for slot in slots {
        stack.push(slot).unwrap();
    }
    input.filters = stack;
}

/// One pipeline stage's samples.
struct Stage {
    name: &'static str,
    /// Dispatches per frame (engine-code-verified constant).
    dispatches_per_frame: u64,
    samples: Vec<f64>,
}

impl Stage {
    fn mean_ms(&self) -> f64 {
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }
}

#[test]
#[ignore]
fn profile_full_pipeline() {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let frames: usize = std::env::var("STREAMFLOW_PROFILE_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let ctx = RenderContext::new_headless().expect("headless render context");
    let mut grade_pass = GradePass::new(&ctx).expect("grade pass");
    let mut fx = EffectPass::new(&ctx).expect("effect pass");
    let scope_pass = ScopePass::new(&ctx).expect("scope pass");
    let scope_params = ScopeParams::default();

    let registry = LookRegistry::load_dir(&looks_dir()).expect("built-in looks");

    // The representative stack: teal-orange look (folds into the grade),
    // vignette, film grain, sharpen. `apply_look` at 1.0 is exactly what the
    // engine's resolve_grade produces for an identity base grade + the look
    // slot (proven byte-identical in C5's `look_folding_matches_baked_grade`).
    let preset = registry.get("teal-orange").expect("teal-orange exists");
    let effective = sf_filters::apply_look(preset, 1.0);
    assert!(
        !effective.is_identity(),
        "teal-orange look must be non-identity"
    );
    let baked = effective.curves.bake();

    let px = synthetic_frame(W, H);

    let mut stages = vec![
        Stage {
            name: "upload",
            dispatches_per_frame: 0,
            samples: Vec::new(),
        },
        Stage {
            name: "grade",
            dispatches_per_frame: 1,
            samples: Vec::new(),
        },
        Stage {
            name: "vignette",
            dispatches_per_frame: 1,
            samples: Vec::new(),
        },
        Stage {
            name: "grain",
            dispatches_per_frame: 1,
            samples: Vec::new(),
        },
        Stage {
            name: "sharpen",
            dispatches_per_frame: 1,
            samples: Vec::new(),
        },
        Stage {
            name: "encode",
            dispatches_per_frame: 1,
            samples: Vec::new(),
        },
        Stage {
            name: "download",
            dispatches_per_frame: 0,
            samples: Vec::new(),
        },
        Stage {
            name: "scopes",
            dispatches_per_frame: 3,
            samples: Vec::new(),
        },
    ];

    // Honesty anchors: every dispatch must really be a GPU dispatch, and
    // the replica must match the engine byte-for-byte. The first warmup
    // frame (seq 1) is the one compared: the engine below runs exactly one
    // frame (also seq 1), and the film-grain seed depends on seq.
    let mut replica_first: Vec<u8> = Vec::new();

    for frame in 0..(WARMUP + frames) {
        let measure = frame >= WARMUP;
        let seq = frame as u32 + 1; // engine's frame seed is 1-based

        // --- upload ---
        let t = Instant::now();
        let src = ctx.upload_rgba8(W, H, &px).expect("upload");
        ctx.device().poll(wgpu::Maintain::Wait);
        let upload_ms = t.elapsed().as_secs_f64() * 1000.0;

        // --- grade (fused) ---
        let lin_a = ctx.create_linear_target(W, H);
        let lin_b = ctx.create_linear_target(W, H);
        let t = Instant::now();
        assert!(grade_pass
            .apply(&ctx, &effective, &baked, None, &src, &lin_a)
            .expect("grade"));
        ctx.device().poll(wgpu::Maintain::Wait);
        let grade_ms = t.elapsed().as_secs_f64() * 1000.0;

        // --- effect slots, ping-ponging cur/other like the engine ---
        let (mut cur, mut other) = (lin_a, lin_b);
        let fx_before = fx.passes_executed();

        let t = Instant::now();
        assert!(fx
            .apply_slot(
                &ctx,
                &EffectKind::Vignette,
                &EffectKind::Vignette.default_params(),
                0.8,
                &cur,
                &other,
                None,
                seq
            )
            .expect("vignette"));
        ctx.device().poll(wgpu::Maintain::Wait);
        let vignette_ms = t.elapsed().as_secs_f64() * 1000.0;
        std::mem::swap(&mut cur, &mut other);

        let t = Instant::now();
        assert!(fx
            .apply_slot(
                &ctx,
                &EffectKind::FilmGrain,
                &EffectKind::FilmGrain.default_params(),
                0.6,
                &cur,
                &other,
                None,
                seq
            )
            .expect("grain"));
        ctx.device().poll(wgpu::Maintain::Wait);
        let grain_ms = t.elapsed().as_secs_f64() * 1000.0;
        std::mem::swap(&mut cur, &mut other);

        let t = Instant::now();
        assert!(fx
            .apply_slot(
                &ctx,
                &EffectKind::Sharpen,
                &EffectKind::Sharpen.default_params(),
                0.5,
                &cur,
                &other,
                None,
                seq
            )
            .expect("sharpen"));
        ctx.device().poll(wgpu::Maintain::Wait);
        let sharpen_ms = t.elapsed().as_secs_f64() * 1000.0;
        std::mem::swap(&mut cur, &mut other);

        // Exactly one dispatch per effect slot (verified, not assumed).
        assert_eq!(
            fx.passes_executed() - fx_before,
            3,
            "vignette+grain+sharpen must be exactly 3 dispatches"
        );

        // --- encode ---
        let out = ctx.create_rgba8_target(W, H);
        let t = Instant::now();
        grade_pass
            .encode_output(&ctx, effective.to_uniform().transfer, &cur, &out)
            .expect("encode");
        ctx.device().poll(wgpu::Maintain::Wait);
        let encode_ms = t.elapsed().as_secs_f64() * 1000.0;

        // --- download ---
        let t = Instant::now();
        let pixels = ctx.download_rgba8(&out).expect("download");
        let download_ms = t.elapsed().as_secs_f64() * 1000.0;

        // --- scopes: downscale once + waveform + vectorscope (subscribed) ---
        let t = Instant::now();
        let small = scope_pass.downscale(&ctx, &out).expect("scope downscale");
        let _wf = scope_pass
            .analyze(&ctx, ScopeKind::Waveform, &small, &scope_params)
            .expect("waveform");
        let _vs = scope_pass
            .analyze(&ctx, ScopeKind::Vectorscope, &small, &scope_params)
            .expect("vectorscope");
        ctx.device().poll(wgpu::Maintain::Wait);
        let scopes_ms = t.elapsed().as_secs_f64() * 1000.0;

        if frame == 0 {
            replica_first = pixels;
        } else if measure {
            let vals = [
                upload_ms,
                grade_ms,
                vignette_ms,
                grain_ms,
                sharpen_ms,
                encode_ms,
                download_ms,
                scopes_ms,
            ];
            for (s, v) in stages.iter_mut().zip(vals) {
                s.samples.push(v);
            }
        }
    }

    // --- replica-vs-engine byte-equality (the honesty anchor) ---
    let mut e = Engine::new_headless().expect("headless engine");
    *e.look_registry_mut() = LookRegistry::load_dir(&looks_dir()).expect("built-in looks");
    e.state_mut().add_input("cam1", "Camera 1");
    set_stack(
        &mut e,
        "cam1",
        vec![
            look_slot("teal-orange", 1.0),
            effect_slot(EffectKind::Vignette, 0.8),
            effect_slot(EffectKind::FilmGrain, 0.6),
            effect_slot(EffectKind::Sharpen, 0.5),
        ],
    );
    let frame = e
        .grade_input_frame("cam1", W, H, &px)
        .expect("engine frame");
    // Compare without dumping megabytes of pixels on failure.
    assert_eq!(
        frame.pixels.len(),
        replica_first.len(),
        "frame size must match"
    );
    let max_diff: u8 = frame
        .pixels
        .iter()
        .zip(replica_first.iter())
        .map(|(a, b)| (*a as i16 - *b as i16).unsigned_abs() as u8)
        .max()
        .unwrap_or(0);
    assert_eq!(
        max_diff, 0,
        "replica must match the engine byte-for-byte for the measured stack"
    );

    // --- report ---
    let total: f64 = stages.iter().map(Stage::mean_ms).sum();
    println!();
    println!("=== StreamFlow Lite C8 pipeline profile (1080p, lavapipe software rasterizer) ===");
    println!("=== ABSOLUTE TIMINGS ARE RELATIVE ONLY — lavapipe != real GPU              ===");
    println!(
        "stack: teal-orange look + vignette + film grain + sharpen; scopes: waveform + vectorscope"
    );
    println!(
        "{n} measured frames after {WARMUP} warmup",
        n = stages[0].samples.len()
    );
    println!("| stage    | dispatches/frame | mean ms | % of frame |");
    println!("|----------|------------------|---------|------------|");
    for s in &stages {
        println!(
            "| {:<8} | {:>16} | {:>7.2} | {:>9.1}% |",
            s.name,
            s.dispatches_per_frame,
            s.mean_ms(),
            100.0 * s.mean_ms() / total
        );
    }
    let total_disp: u64 = stages.iter().map(|s| s.dispatches_per_frame).sum();
    println!(
        "| TOTAL    | {:>16} | {:>7.2} | {:>9.1}% |",
        total_disp, total, 100.0
    );
    println!();

    assert!(!stages.is_empty());
    assert!(total > 0.0, "pipeline must do real work");
}

/// Engine end-to-end frame timing (the real [`Engine::grade_input_frame`],
/// same stack as [`profile_full_pipeline`], no scope subscribers).
///
/// The replica above measures per-pass costs; this measures the engine's
/// actual per-frame cost including its own target allocation and state
/// handling. `download_rgba8` inside `grade_input_frame` already forces GPU
/// completion, so no extra poll is needed.
///
/// Run explicitly:
/// `cargo test -p sf-engine --test perf_profile engine_end_to_end -- --ignored --nocapture`
#[test]
#[ignore]
fn profile_engine_end_to_end() {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let frames: usize = std::env::var("STREAMFLOW_PROFILE_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let mut e = Engine::new_headless().expect("headless engine");
    *e.look_registry_mut() = LookRegistry::load_dir(&looks_dir()).expect("built-in looks");
    e.state_mut().add_input("cam1", "Camera 1");
    set_stack(
        &mut e,
        "cam1",
        vec![
            look_slot("teal-orange", 1.0),
            effect_slot(EffectKind::Vignette, 0.8),
            effect_slot(EffectKind::FilmGrain, 0.6),
            effect_slot(EffectKind::Sharpen, 0.5),
        ],
    );
    let px = synthetic_frame(W, H);

    let mut samples = Vec::new();
    for frame in 0..(WARMUP + frames) {
        let t = Instant::now();
        let out = e
            .grade_input_frame("cam1", W, H, &px)
            .expect("engine frame");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(out.pixels.len(), (W * H * 4) as usize);
        if frame >= WARMUP {
            samples.push(ms);
        }
    }
    let mean: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
    println!();
    println!("=== engine end-to-end (1080p, lavapipe; relative numbers only) ===");
    println!(
        "mean frame: {mean:.2} ms over {} frames ({} warmup)",
        samples.len(),
        WARMUP
    );
    println!();
    assert!(mean > 0.0);
}
