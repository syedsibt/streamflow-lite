//! C8 soak test (worker 1): the full engine pipeline under sustained load.
//!
//! Ignored by default — run explicitly:
//!
//! ```sh
//! export PATH="$HOME/.cargo/bin:$PATH"
//! export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json
//! cargo test -p sf-engine --test soak -- --ignored --nocapture
//! ```
//!
//! `STREAMFLOW_SOAK_FRAMES` overrides the frame count (default 1000).
//!
//! The 2-hour variant (216000 frames = 2 h of footage at 30 fps):
//!
//! ```sh
//! STREAMFLOW_SOAK_FRAMES=216000 cargo test -p sf-engine --test soak -- \
//!     --ignored --nocapture
//! ```
//!
//! NOTE: 216000 is 2 hours of *footage*. On lavapipe (software rasterizer,
//! ~1.8 fps at 1080p for this stack) that is ~33 h of wall-clock time — the
//! 2-hour figure is realistic only on a real GPU. The default N=1000 takes
//! ~10 min on lavapipe.
//!
//! What runs per frame: upload -> teal-orange look folded into the fused
//! grade -> vignette -> film grain -> sharpen -> AI background blur (hue
//! fallback, exercises the C7 mask cache) -> output encode -> download,
//! with waveform + vectorscope subscribed (15 Hz throttle).
//!
//! What is asserted:
//!
//! * every frame returns `Ok` (zero panics/errors) and differs from the
//!   input (work actually happened);
//! * consecutive frames differ (the grain seed advances — the pipeline is
//!   not frozen on one frame);
//! * scope frames keep arriving: per-kind sequence numbers are exactly the
//!   contiguous `1..=n`, and no kind ever exceeds its 15 Hz throttle;
//! * no unbounded growth, via coarse counters that must match exact
//!   closed forms:
//!   - `effect_passes() == 6 * N` — exactly 6 effect dispatches per frame
//!     (vignette 1 + grain 1 + sharpen 1 + background blur 3), i.e. work
//!     grows exactly linearly, never super-linearly;
//!   - `mask_uploads() == ceil(N / 15)` — the person mask is recomputed and
//!     re-uploaded only every 15 frames (the C7 mask refresh cadence),
//!     never per frame;
//!   - `grade_cache_stats() == (3 * (N - 1), 3)` — the grade uniform, curves
//!     texture and LUT are built once (3 misses) and then hit the dirty-flag
//!     cache on every later frame (3 hits each), never rebuilt;
//!   - `ai_skipped() == 0` — nothing in this stack skips.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use sf_engine::Engine;
use sf_filters::{EffectKind, Filter, FilterSlot, FilterStack, LookRegistry};
use sf_scopes::ScopeKind;
use uuid::Uuid;

const W: u32 = 1920;
const H: u32 = 1080;

fn looks_dir() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.pop();
    d.pop();
    d.join("assets/looks")
}

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

/// Expected per-frame effect dispatches for the soak stack:
/// vignette 1 + film grain 1 + sharpen 1 + background blur 3
/// (two-pass gaussian + masked composite).
const DISPATCHES_PER_FRAME: u64 = 6;
/// The C7 mask refresh cadence: the person mask is recomputed every
/// 15 frames, so uploads are exactly ceil(N / 15).
const MASK_REFRESH_FRAMES: u64 = 15;

#[test]
#[ignore]
fn soak_full_pipeline() {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let n: u64 = std::env::var("STREAMFLOW_SOAK_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    assert!(n >= 1, "need at least one frame");

    let mut e = Engine::new_headless().expect("headless engine");
    *e.look_registry_mut() = LookRegistry::load_dir(&looks_dir()).expect("built-in looks");
    e.state_mut().add_input("cam1", "Camera 1");
    set_stack(
        &mut e,
        "cam1",
        vec![
            FilterSlot {
                id: Uuid::new_v4(),
                filter: Filter::Look {
                    preset_id: "teal-orange".to_string(),
                    intensity: 1.0,
                },
                enabled: true,
                intensity: 1.0,
                mask: None,
            },
            effect_slot(EffectKind::Vignette, 0.8),
            effect_slot(EffectKind::FilmGrain, 0.6),
            effect_slot(EffectKind::Sharpen, 0.5),
            effect_slot(EffectKind::BackgroundBlur, 0.7),
        ],
    );
    e.scopes_subscribe("cam1", ScopeKind::Waveform)
        .expect("subscribe waveform");
    e.scopes_subscribe("cam1", ScopeKind::Vectorscope)
        .expect("subscribe vectorscope");

    let px = synthetic_frame(W, H);
    let mut prev: Vec<u8> = Vec::new();
    let mut seqs: HashMap<ScopeKind, Vec<u64>> = HashMap::new();
    let mut drained_total: u64 = 0;

    let t0 = Instant::now();
    for i in 0..n {
        let frame = e
            .grade_input_frame("cam1", W, H, &px)
            .expect("soak frame must not error");
        // Work happened: the output is never the input.
        assert_ne!(frame.pixels, px, "frame {i}: output must differ from input");
        // The pipeline is not frozen: the grain seed advances per frame.
        if !prev.is_empty() {
            assert_ne!(
                frame.pixels, prev,
                "frame {i}: consecutive frames must differ (animated grain)"
            );
        }
        prev = frame.pixels;

        for sf in e.drain_scope_frames() {
            seqs.entry(sf.kind).or_default().push(sf.seq);
            drained_total += 1;
        }
        if (i + 1) % 100 == 0 {
            println!(
                "soak: frame {}/{}  effect_passes={}  mask_uploads={}  scope_frames={}",
                i + 1,
                n,
                e.effect_passes(),
                e.mask_uploads(),
                drained_total
            );
        }
    }
    let elapsed = t0.elapsed();

    // --- scope cadence: contiguous seqs, never faster than the 15 Hz throttle.
    for kind in [ScopeKind::Waveform, ScopeKind::Vectorscope] {
        let s = seqs.get(&kind).cloned().unwrap_or_default();
        assert!(
            !s.is_empty(),
            "{kind:?}: no scope frames arrived over {n} frames"
        );
        let expected: Vec<u64> = (1..=s.len() as u64).collect();
        assert_eq!(
            s, expected,
            "{kind:?}: scope seqs must be exactly contiguous 1..=n"
        );
        let max_allowed = elapsed.as_secs_f64() * 15.0 + 1.0 + 1e-9;
        assert!(
            s.len() as f64 <= max_allowed,
            "{kind:?}: {} emissions in {:.1}s exceeds the 15 Hz throttle",
            s.len(),
            elapsed.as_secs_f64()
        );
    }
    let n_kinds = seqs.len() as u64;
    assert_eq!(
        drained_total,
        seqs.values().map(|v| v.len() as u64).sum::<u64>(),
        "drained count must equal the per-kind sums"
    );
    assert_eq!(n_kinds, 2, "both subscribed kinds must have emitted");

    // --- bounded growth: exact closed forms, not just "looks fine".
    assert_eq!(
        e.effect_passes(),
        DISPATCHES_PER_FRAME * n,
        "effect dispatches must be exactly {DISPATCHES_PER_FRAME}/frame (linear, never super-linear)"
    );
    assert_eq!(
        e.mask_uploads(),
        n.div_ceil(MASK_REFRESH_FRAMES),
        "person-mask uploads must be exactly ceil(N/15)"
    );
    let (hits, misses) = e.grade_cache_stats();
    assert_eq!(
        (hits, misses),
        (3 * (n - 1), 3),
        "grade resources: 3 misses on frame 1, then 3 cache hits per frame"
    );
    assert_eq!(e.ai_skipped(), 0, "nothing in this stack may skip");

    println!();
    println!(
        "=== StreamFlow Lite C8 soak: {n} frames, {:.1}s elapsed ({:.1} fps) ===",
        elapsed.as_secs_f64(),
        n as f64 / elapsed.as_secs_f64()
    );
    println!(
        "effect_passes      : {} (exactly 6/frame)",
        e.effect_passes()
    );
    println!(
        "mask_uploads       : {} (exactly ceil(N/15))",
        e.mask_uploads()
    );
    println!("grade cache        : {hits} hits / {misses} misses");
    println!(
        "scope frames       : {drained_total} drained (waveform + vectorscope, seqs contiguous)"
    );
    println!("ai_skipped         : {}", e.ai_skipped());
    println!("zero panics/errors : yes");
    println!();
}
