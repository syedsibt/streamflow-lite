# StreamFlow Lite — Build Log

## C0–C3 — Color foundation (2026-10-09) — COMPLETE

`cargo test --workspace` → **62 passed, 0 failed**, zero build warnings.
GPU tests run headless on lavapipe (`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`).

| Phase | Scope | Status |
|---|---|---|
| C0 | wgpu pipeline + fused grade shader: `sf-render` (headless `RenderContext`, texture upload/download, `GradePass` with 3 WGSL compute passes: fused grade → optional unsharp detail → output encode; identity grade → pass skipped, `Ok(false)`), `sf-color` `GradeUniform` (160-byte `Pod`) + `is_identity()`, `sf-state` per-input color state, `sf-engine` minimal orchestrator | Done — 5 render GPU tests + 1 engine integration test pass |
| C1 | Wheels/tone/curves in `sf-color`: full Basic-tab params + Pro wheels in the shader math, `CurveSet` (RGB/R/G/B/luma/hue-vs-sat, ≤16 pts, Fritsch–Carlson monotone cubic), CPU bake to 1024×2 RGBA32F (row0=R,G,B,master; row1=luma,hue-vs-sat), re-baked only on edit; `apply_grade_cpu` f32 reference = exact WGSL math contract | Done — 29/29 sf-color tests pass |
| C2 | 3D LUT: `.cube` parser (1D + 3D; sizes 17/25/33/65 kept, others trilinear-resampled to 33³ with warning; bad files → typed `LutParseError`, engine keeps old LUT), position Before/After grade, intensity blend 0–100% | Done — GPU LUT-identity case: 0/255 vs CPU and vs input |
| C3 | Look filters + stack in `sf-filters`: `.sflook` JSON model per spec §5.1, `Filter`/`FilterSlot`/`FilterStack` (max 8 slots, max 2 looks per stack enforced in code), 30 built-in looks in `assets/looks/` (all 8 categories), `LookRegistry::load_dir`, `apply_look` intensity scaling → editable `ColorGrade` | Done — 14/14 sf-filters tests pass |
| — | `sf-ipc`: 12 UI→engine commands / 6 engine→UI events, serde `"cmd"`/`"event"` tags per spec §9 | Done — 6/6 tests pass |
| — | `sf-project`: `project.json` save/load incl. `inputs[].color`, `inputs[].filters`, `scenes[].filters`, `program.filters`; LUT/look paths stay project-relative strings | Done — 3/3 tests pass |
| — | `sf-state`: `InputColor`/`InputState`/`State`, `apply_grade_to_all_cameras` | Done — 3/3 tests pass |

Test command: `cargo test --workspace` (62 tests: 29 sf-color, 14 sf-filters, 6 sf-ipc, 3 sf-state, 3 sf-project, 6 sf-render incl. 5 real-GPU, 1 sf-engine).

## C4 — Scopes (2026-10-09) — COMPLETE

`cargo test --workspace` → **85 passed, 0 failed** (was 62), zero build warnings.
GPU tests run headless on lavapipe (`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`).

| Phase | Scope | Status |
|---|---|---|
| C4 | GPU scopes in `sf-scopes`: 6 WGSL compute shaders (box downscale to 256x144, waveform luma+RGB parade, 128x128 vectorscope, RGB+luma histogram, false-color exposure map, zebra overlay); `ScopePass` (downscale-once + per-kind `analyze`); `cpu` f32 reference module (the exact math contract); `ScopeKind` / `ScopePayload` / `ScopeFrame` data model; skin-tone guide constant 123° | Done — 9 cpu + 8 real-GPU tests pass |
| — | Engine wiring: `ScopeHub` (subscriptions, 15 Hz throttle per subscription, frame outbox, `gpu_invocations` counter); `Engine::scopes_subscribe` / `scopes_unsubscribe` / `drain_scope_frames`; scope tap in `grade_input_frame` on the **graded** frame (identity grades are still scoped); `scopes.unsubscribe` IPC command added | Done — 5 integration tests pass |
| — | `GradePass` dirty-flag caching (C0–C3 follow-up): uniform buffer / curves texture / LUT texture rebuilt only when their content hash changes; LUT keyed by `CubeFile` address (documented contract); `apply` now takes `&mut self`; `cache_stats()` → `(hits, misses)` | Done — cache test: 3/3 hits on repeat apply, exactly 1 rebuild on a param change, identity bypass untouched |

Test command: `cargo test --workspace` (85 tests: 29 sf-color, 14 sf-filters, 6 sf-ipc, 3 sf-state, 3 sf-project, 7 sf-render incl. 6 real-GPU, 17 sf-scopes incl. 8 real-GPU, 6 sf-engine).

### C4 decisions

- **Luma is never `dot(c, LUM)` in scope shaders** — explicit `c.r*0.2126 + c.g*0.7152 + c.b*0.0722` everywhere. Backends may lower `dot()` with FMA contraction or a different summation order, diverging from the CPU reference by 1 ulp and flipping pixels sitting exactly on histogram bin boundaries (measured: 2 pixels out of 36864 moved one bin). The explicit expansion keeps both sides on one op order; the reason is documented in every shader.
- **Luma histogram agreement is honestly tolerant**: r/g/b bins, waveform, and vectorscope assert exact equality; luma allows adjacent-bin ±1 flips (≤2 pixels of 36864, i.e. moved-mass ≤ 4) because luma is the only scope input computed with arithmetic. Documented in the test.
- **Zebra overlay alpha is 0.75** (191.25 → 191 on both sides). 0.7 would quantize to 178.5, where GPU round-half-even (178) and Rust `round()` (179) disagree — the 0.75 value sidesteps the trap.
- **Vectorscope chroma**: Rec.709 `Cb = (B−Y)/(2(1−Kb))`, `Cr = (R−Y)/(2(1−Kr))`, nominal [−0.5, 0.5] clamped to the 128x128 grid. Skin-tone guide (~123°) is a UI overlay drawn from `SKIN_TONE_ANGLE_DEG`, not scope data.
- **False color**: stops from middle gray `s = log2(max(L,1e-4)/0.18)`, 6 ARRI-inspired bands (red ≥2.42, yellow ≥1.42, green ≥0.5, gray ≥−0.5, cyan ≥−1.5, blue below).
- **Payload sizes**: waveform 8 KB, vectorscope 64 KB, histogram 4 KB, false-color / zebra 256x144 RGBA8 images (~147 KB each) at 15 Hz worst case — localhost-IPC-safe, and only while subscribed.
- **One real bug found by the new tests**: the engine's final RGBA8 target was created without `TEXTURE_BINDING`, so the scope downscale shader could not sample it (`scopes_tap_the_graded_frame` failed validation). Fixed in `RenderContext::create_rgba8_target`.

### C4 GPU verification evidence (lavapipe, headless)

| Test | Result |
|---|---|
| `downscale_agrees_with_cpu` (512x288 → 256x144) | max abs diff ≤ 1/255 |
| `waveform_agrees_with_cpu` (color bars) | max abs diff ≤ 1e-6; white bar reads 1.0, black bar 0.0 |
| `vectorscope_agrees_with_cpu` (color bars) | bins **exactly** equal; total = 36864 |
| `vectorscope_skin_tone_lands_near_guide` | GPU/CPU peak bin identical: (53, 77) for the (200,150,120) patch |
| `histogram_agrees_with_cpu` (gradient) | r/g/b exact; luma within adjacent-bin tolerance |
| `false_color_bands_agree_with_cpu` (6 bands) | pixel-exact; band colors spot-checked |
| `zebra_thresholds_agree_with_cpu` | pixel-exact; stripe pattern spot-checked |
| `no_scope_work_without_subscribers` | 0 GPU invocations, empty outbox; subscribe → work; unsubscribe → stops |
| `scope_cadence_throttles_to_15hz` | 5 rapid frames → 1 emission; +80 ms → next emits (seq 2) |
| `scopes_tap_the_graded_frame` | waveform luma_max rises under +1 EV grade (proves post-grade tap) |
| `grade_pass_caches_per_grade_resources` | (0,3) → (3,3) → (5,4) hits/misses across repeat/param-change/identity |

### C4 deviations from the spec

1. `scopes.frame` payloads travel in-process via `Engine::drain_scope_frames()`; the IPC event keeps `input_id` + `seq` (out-of-band transport arrives with the Tauri layer).
2. There is no IPC command dispatch loop yet — the engine exposes `scopes_subscribe` / `scopes_unsubscribe`, which the future UI layer calls when it receives `scopes.subscribe` / `scopes.unsubscribe`.

## C5 — Effect filters, look folding, engine integration (2026-10-09) — COMPLETE

`cargo test --workspace` → **125 passed, 0 failed** (was 98), zero build warnings.
GPU tests run headless on lavapipe (`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`).

| Phase | Scope | Status |
|---|---|---|
| C5 | Group-C effect filters in `sf-filterfx` (worker 3): chroma key (standard bind layout) + temporal noise reduction (temporal layout: src + prev + dst + uniform). 16x16 workgroups, `textureLoad` only, explicit `sqrt` (never `dot()`), math-contract shader headers; CPU references `chroma_key_cpu` / `noise_reduction_cpu` with the exact specified signatures | Done — GPU-vs-CPU: chroma 9.2e-5, NR 3.7e-4 (limit 3e-3) |
| — | Group-B effect filters in `sf-filterfx` (worker 2): sharpen, gaussian blur, bloom, crop/zoom/pan, lens correction, chromatic aberration, pixelate (two-pass H+V via the scratch texture for blur/bloom) | Done — GPU-vs-CPU all ≤ 1.1e-3 (see evidence) |
| — | Look folding: `ColorGrade::layer_look` delta composition (`sf-color`) + `sf_filters::resolve_grade` (base grade + stack looks in order; unknown preset ids skipped silently, disabled looks and effect slots ignored) | Done — 4 + 6 unit tests |
| — | `GradePass::linearize` (`sf-render`): grade-less display→linear (`transfer` 0 = `pow(c,2.4)`, 1 = sRGB EOTF inverse); encode-BGL binding shape with `rgba16float` storage | Done |
| — | Engine rewire (`sf-engine`): `grade_input_frame` = resolve effective grade → upload → grade **or** grade-less linearize → optional unsharp detail → effect slots in stack order → encode → download. New fields: `fx: EffectPass`, `look_registry: LookRegistry` (+ `look_registry_mut`), per-input `nr_prev` textures, per-input `frame_seq` (doubles as the effect frame seed), `ai_skipped` counter (+ `effect_passes()` / `ai_skipped()` accessors). Identity grade + no active effects → bit-exact bypass, zero GPU work beyond upload | Done — 8 integration tests pass |

Test command: `cargo test --workspace` (123 tests: 33 sf-color, 23 sf-filters, 6 sf-ipc, 3 sf-state, 3 sf-project, 10 sf-render incl. 6 real-GPU, 17 sf-scopes incl. 8 real-GPU, 14 sf-filterfx incl. 14 real-GPU, 14 sf-engine incl. 14 integration).

### C5 decisions

- **Linear-space effects between grade and encode** (the C5 brief): every effect slot runs on the linear working frame after the fused grade — or after the grade-less `linearize` pass when the effective grade is identity — and before output encode, so all 13 WGSL effects share one signal domain.
- **Scope tap moved post-effects**: scopes now analyze the final frame (exactly what the program outputs), not the graded-but-uneffected frame. Existing scope tests still pass (identity/no-effect frames are unchanged by the move).
- **First-frame NR uses prev == cur**: `apply_slot(NoiseReduction, prev=None)` returns `RenderError::SizeMismatch` (worker-1 contract, locked by `noise_reduction_requires_prev_texture`), so the engine always passes `Some(prev)`; on the first frame it passes the current frame itself. Read-only double-binding of one texture is legal, and the blend of a frame with itself is the frame. Measured: first NR frame == input, 0 u8 diff.
- **AI kinds skip gracefully**: each skipped AI slot bumps `Engine::ai_skipped`; the frame still processes with the non-AI slots (spec's "grey out, non-AI keep working"). `apply_slot` itself also bypasses AI kinds with zero GPU work.
- **Chroma smoothstep guard**: both sides implement `t = clamp((x-e0)/max(e1-e0, 1e-6), 0, 1)` so `key_softness = 0` degrades to a hard key edge instead of 0/0 NaN. Documented in the shader header and `cpu.rs`.
- **`nr_prev` textures carry `COPY_DST`** (built directly in the engine — `create_linear_target` lacks `COPY_DST`) and are recreated when the frame size changes; the post-effects frame is copied in at end of frame.
- **Stack look == baked grade, byte-identical**: `resolve_grade(identity, [look @1.0])` reproduces `apply_look(preset, 1.0)` field-exact (additive deltas around 0/100/6500 K, multiplicative gamma/wheels, last-wins curves/LUT, OR skin_protect), so both engine paths run the same dispatches.

### C5 GPU verification evidence (lavapipe, headless)

| Test | Result |
|---|---|
| `chroma_key_agrees_with_cpu` | max abs diff **9.2e-5** (limit 3e-3); green half keys to alpha ≈ 0, red half stays opaque |
| `noise_reduction_agrees_with_cpu` (u=0.5) | max abs diff **3.7e-4**; u=1 copies prev rgb exactly, keeps cur alpha |
| `noise_reduction_is_temporal` | u=1: frame 2 output == frame 1 output (≤ 2 u8); frame 2 vs no-NR = 170 u8 diff; first frame == input (0 u8) |
| `stack_order_honored` | vignette→mirror and mirror→vignette each match CPU composition (4.9e-4); the two orders differ by 0.70 (proves order) |
| `bypass_is_bit_identical` | output bytes == input bytes exactly; `effect_passes() == 0` |
| `ai_effects_skip_gracefully` | frame succeeds, `ai_skipped() == 1`, vignette still applied |
| `look_folding_matches_baked_grade` | stack look vs baked grade **byte-identical** |
| worker-2 filters, GPU-vs-CPU (measured in this run; worker 2's report is authoritative) | sharpen 9.3e-4, gaussian blur 8.6e-4, bloom 1.1e-3, crop/zoom/pan 0, lens correction 0, chromatic aberration 0, pixelate 0 |

### C5 deviations from the spec

1. `apply_slot(NoiseReduction, prev=None)` → `RenderError::SizeMismatch` (worker-1 implementation detail, not in the spec text); the engine therefore always passes `Some(prev)`. Locked by `noise_reduction_requires_prev_texture`.
2. Worker 2's in-progress `apply_two_pass` had a borrowck error (`&mut self` scratch borrow held across the pipeline lookup); fixed minimally by scoping the scratch borrow to an owned view before the lookup — internal only, no public API change. Worker 2 to review.
3. **FIXED (coordinator, post-handoff): `ColorGrade` serde defaults are now the neutral values.** `#[serde(default)]` on `f32`/`[f32;4]` fields yields 0.0, but neutral is saturation 100.0, temperature 6500 K, gamma 1.0, gain/gamma_wheel [1,1,1,1] — so nearly every built-in look (which omits these fields) deserialized to degenerate values at full intensity: gamma 0 → `pow(x, 1/0)` inf/NaN, gain [0,0,0,0] → black frame, temperature 0 K → extreme WB gains, saturation 0 → grayscale. Fixed with `#[serde(default = "...")]` neutral helpers in `sf-color/src/grade.rs`; regression tests `partial_grade_json_takes_neutral_defaults` + `builtin_looks_omitting_fields_expand_to_neutral` pin it. The `resolve_grade` tests no longer need to pin `"temperature": 6500` in temp-look JSON.

## C6 — AI color (auto color, camera match, skin protect, look suggest, drift guard) (2026-10-09) — COMPLETE

`cargo test --workspace` → **166 passed, 0 failed** (was 125), zero build warnings (coordinator independently re-ran the full suite: exit 0).
CPU-only analysis crates; GPU tests still run headless on lavapipe (`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`).

| Phase | Scope | Status |
|---|---|---|
| C6 | New `sf-ai-color` crate: `stats` (FrameStats: Rec.709 luma p1/p5/p50/p95/p99, channel means, mean chroma, skin fraction, gray-region means with global fallback; `rgb_to_hue_deg` = vectorscope angle atan2(Cr,Cb), NOT HSV hue) | Done |
| — | `auto` (§6.1): one-click auto color → editable `ColorGrade` + explanation strings. Exposure `log2(0.42/p50)` ±3 EV, skin-aware target pull; contrast `100·(0.65−spread)` gated to 0 on flat frames; WB via best-fit Kelvin search against `kelvin_to_gains` + tint from green imbalance | Done |
| — | `camera_match` (§6.2): stat-ratio matching (exposure log-ratio; WB from neutral-region channel ratios; contrast/saturation from **exposure-normalized** spread/chroma ratios), `interpolate_grade` with smoothstep easing (curves/LUT/`skin_protect` snap at t≥0.5), `solve_color_matrix` least-squares 3×3 via normal equations + Gaussian elimination with partial pivoting | Done |
| — | `skin` (§6.3): `skin_mask_hue` (raised-cosine falloff to 0 at ±40° around 123°, C¹-smooth, no hard edges), `skin_mask_combined` (ONNX `FaceMask` nearest-resampled, **union** via max — errs toward protection; malformed mask → hue fallback), `protection_strength`, `attenuate_for_skin` (scales saturation/vibrance deltas toward neutral; no-op when `skin_protect` off; documented: use it OR the GPU pass heuristic, not both) | Done |
| — | `suggest` (§6.4): score-based look ranking over the real 30 presets in `assets/looks/` (only verified ids; test pins `fix-warm-cast` absent), deterministic (score desc, id asc tiebreak) | Done |
| — | `drift` (§6.5): `DriftGuard` with self-priming baseline, ±0.5 EV exposure / 20% neutral-ratio WB thresholds, hysteresis until `rebaseline()`; cannot mutate a grade by construction (only reads `&FrameStats`) | Done |
| — | `sf-ipc`: new `Event::ColorDriftSuggest { input_id, message }` (`"event":"color.drift_suggest"`), added to the tag test | Done |
| — | `sf-ai-runtime`: `InferenceSession: Send` trait + `StubSession` (model-unavailable → `Err(AiError::ModelMissing)`, bad dims → `InvalidInput`, overflow-checked, never panics) + `FaceMask { width, height, weights }` | Done — 3/3 tests pass |

Test command: `cargo test --workspace` (166 tests: +38 sf-ai-color, +3 sf-ai-runtime; all 125 prior still green).

### C6 decisions

- **STUB-ONLY for `ort`** (no `ort` backend shipped). `ort` 2 is still prerelease (`2.0.0-rc.13`); the prebuilt-binary download from `cdn.pyke.io` failed twice (TLS EOF, then hang) inside a ~22 min timebox, so all `ort` wiring was removed. Nothing vendored in `models/`. The hue-based skin protection path is the tested default; a real backend plugs in via the `InferenceSession` trait. Documented in `sf-ai-runtime` module docs.
- **Canonical cross-crate contract** (reconciled post-handoff): `AiError::{ModelMissing { model }, InferenceFailed(String), InvalidInput(String)}`, `StubSession::new(model)`, `FaceMask { width, height, weights }` row-major 0..1. The `sf-ai-color` worker briefly overwrote `sf-ai-runtime` with a renamed variant (`ModelUnavailable`); the `sf-ai-runtime` worker restored the brief's naming and verified the sibling only uses `FaceMask` struct literals — nothing breaks. This naming is canonical going forward.
- **`rgb_to_hue_deg` is the vectorscope angle** `atan2(Cr, Cb)` in Rec.709 — the (200,150,120) patch measures 128.2°, near the 123° guide. Do NOT feed it to HSV-space logic (C7 note).
- **Skin mask union errs toward protection**: `skin_mask_combined` takes `max(hue_weight, face_weight)` — a false positive costs a little saturation, a false negative costs a face shift.
- **Drift guard is suggest-only by construction**: `DriftGuard::update` takes `&FrameStats` and returns `Option<DriftSuggestion>`; there is no code path that mutates a grade. The UI/Tauri layer converts the suggestion into the `color.drift_suggest` event.
- Engine `color.auto` / `color.match` command wiring skipped: `sf-ipc` has the commands but no dispatch loop exists yet (C4 deviation) — engine exposes the analysis via the new crates; wiring lands with the Tauri layer.

### C6 verification evidence (measured)

| Test | Result |
|---|---|
| Auto color, dark uniform 0.1 frame | exposure **2.042** vs expected `log2(4.2)` = 2.070 (Δ 0.028 < 0.1) |
| Auto color, 0.42-gray frame | \|exposure\| < 0.05, contrast 0, WB neutral, explanation non-empty |
| Camera match, synthetic +1 EV & 1.2× red | exposure correction **exactly −1.0**, temperature > 6500 K (cooling), contrast/saturation neutral to 1e-3 |
| `solve_color_matrix`, diag(1.1, 0.9, 1.0) | recovered **< 1e-4**; non-diagonal < 1e-3; <3 patches → clean panic-free error path (tested) |
| Skin mask, (200,150,120) patch | mean weight **≈ 0.96** (> 0.7); pure blue **0.0** (< 0.05); skin→blue gradient yields strictly-interior weights (no hard edge) |
| `attenuate_for_skin` | saturation 140 → 100 at strength 1, → 120 at 0.5, no-op when flag off |
| Look suggest, dark frame | `fix-low-light` in top 3 with "Low light…" reason; two runs byte-identical; all ids resolve in the real registry |
| Drift guard | stable → None; p50 halved → `Some(Exposure)` ("…re-match?"); R/B-shifted → `Some(WhiteBalance)`; repeat → None until `rebaseline()`; grade value asserted unchanged |
| AI runtime stub | `model_available()` false; `face_mask` → `ModelMissing`; mismatched dims → `InvalidInput` (no panic); `Box<dyn InferenceSession>` object-safe |

### C6 deviations from the spec

1. No `ort` backend and no vendored model (see decision above) — stub-only is the spec's sanctioned graceful-degradation path.
2. `camera_match` contrast/saturation use exposure-normalized ratios (refinement: they correct true mismatch, not the spread change exposure already explains).
3. `auto_color` leaves contrast at 0 on near-flat frames (spread < 0.03 carries no contrast information).
4. `fit_temperature` logic is duplicated in `auto.rs` / `camera_match.rs` (modules stay self-contained; acceptable duplication).
5. `DriftGuard` self-primes its baseline on the first `update()` (documented).
6. Skin-tone hue uses the vectorscope angle convention rather than HSV hue (more consistent with the scope guide at 123°).

### Known flake (not caused by C6)
`sf-engine`'s `scope_integration` GPU test flakes intermittently (~1 in 5 cold-start runs on lavapipe, with `XDG_RUNTIME_DIR` loader noise; e.g. "4 passed; 1 failed" then 5/5 on immediate retry). No GPU-path files were touched in C6; it flaked identically before. Candidate for a retry/warmup in the test harness in C7/C8.

## C7 — AI background effects + region masks (2026-10-09) — COMPLETE

`cargo test --workspace` → **187 passed, 0 failed** (was 166), zero build warnings (coordinator independently re-ran the full suite: exit 0; fixed one `unused variable` warning in the new GPU tests during verification).

| Phase | Scope | Status |
|---|---|---|
| C7 | `sf-filterfx` GPU worker: `masked_mix.wgsl` (`dst = mix(a, b, mask.r · amount)`), `bg_replace.wgsl` (plate sample + spill suppression at mask edges), `gen_region_mask.wgsl` (analytic Circle/Rectangle → Rgba16Float mask texture, dedicated 32-byte uniform); `EffectPass::{apply_background_blur, apply_background_replace, apply_skin_smooth, gen_region_mask, masked_mix}` — exact signatures per the cross-worker contract; `apply_slot`/`FxUniform` untouched | Done — 14 → 20 sf-filterfx tests |
| — | `sf-filterfx` CPU twins in `cpu.rs` (`masked_mix_cpu`, `bg_replace_cpu`, `gen_region_mask_cpu`) + 6 GPU-vs-CPU tests | Done — all ≤ 1.32e-3 |
| — | `sf-filters`: `MaskRef::Rectangle { x, y, w, h, feather }` (top-left + size, normalized, feather 0..1) with serde; Circle/Person untouched | Done |
| — | `sf-engine` `ai_masks.rs` (new): `Box<dyn InferenceSession>` field (default `StubSession("person-seg")`, `set_ai_session` clears cache), per-input `(String, MaskKind)` mask cache (Rgba16Float, `.r` = weight), refresh on size change or every `MASK_REFRESH_FRAMES = 15` frames (~0.5 s @ 30 fps, seq-driven not wall-clock), `mask_uploads` counter | Done — 6 unit tests |
| — | Mask policy in `grade_input_frame`: BackgroundBlur → person mask (model → hue fallback, always functional); SkinSmooth → `skin_mask_combined` (always functional); BackgroundReplace → model only, else `ai_skipped += 1`, zero GPU work (spec grey-out). Region-masked non-AI slots: `apply_slot` → scratch → `gen_region_mask`/cached person mask → `masked_mix(a=original, b=effected)` | Done — 8 integration tests |

Test command: `cargo test --workspace` (187 tests: +6 sf-filterfx GPU, +6 sf-engine ai_masks unit, +8 sf-engine ai_effects integration, +1 sf-filters serde; 1 updated: `ai_effects_skip_gracefully` → `ai_effects_c7_policy`).

### C7 decisions

- **One compositing primitive**: everything mask-based funnels through `masked_mix` (`dst = mix(a, b, m)`). Background blur = 2-pass gaussian → `masked_mix(a=blurred, b=sharp)`; skin smooth = 2-pass gaussian (radius fallback 2.0) → `masked_mix(a=sharp, b=blurred)` so the mask selects the *smoothed* pixels; region masks = effect → scratch → `masked_mix(a=original, b=effected)`.
- **Mask textures are Rgba16Float, `.r` = weight** (consistent with the effect working space; `test_utils` upload/download reused).
- **Analytic region masks need no model**: Circle/Rectangle render to a mask texture via `gen_region_mask.wgsl`; Person reuses the cached AI/hue person mask.
- **Mask refresh policy**: every 15 frames or on size change — person/skin geometry moves slowly vs frame rate, bounding CPU cost; deterministic (frame counter, never wall-clock), so cache tests are exact (uploads 1→1→2 across frames 1/2–15/16).
- **BackgroundReplace stays model-gated**: the hue heuristic is a skin mask, not person segmentation — too unreliable for replacement, so the spec's grey-out rule applies (`ai_skipped`). Blur/skin-smooth stay functional via the documented fallback.
- **Look slots with masks**: grade-level masking is out of C7 scope — documented TODO in code, not implemented.
- **Cache key is `(String, MaskKind)`** — a plain input-id key would collide when one input caches both person and skin masks.

### C7 verification evidence (lavapipe, headless)

| Test | Result |
|---|---|
| background_blur vs CPU | max abs diff **0.00110** (limit 3e-3); bg variance 0.0748 → 0.00483 (≥50% ✓); person variance bit-identical; edge band strictly interior (no binary seam) |
| bg_replace solid plate vs CPU | **0** (bit-exact); bg == plate ±1/255; person pixels bit-identical |
| bg_replace image plate (4×4 tiled, feathered mask → spill path) | max abs diff **0.000487** |
| skin_smooth vs CPU | max abs diff **0.00132**; skin variance reduced; blue patch 0 diff |
| gen_region_mask circle / rectangle vs CPU | **0.000477** / **0.000427** (limit 1/255); feather yields interior values |
| masked_mix vs CPU | **0.000488** (limit 1/255) |
| Region circle + Vignette (engine) | outside region bit-identical (0 diff); deep inside == full-effect run; feather gradual |
| Mask caching (engine) | uploads exactly 1 → 1 → 2 across frames 1 / 2–15 / 16 |
| ai-unavailable (stub) + BackgroundReplace | `ai_skipped == 1`, frame processes (vignette applied); output byte-identical to vignette-only run |
| Bypass (amount 0) | `Ok(false)`, `passes_executed` unchanged, all three apply methods |

### C7 genuine findings (fixed, not papered over)

1. **Skin-smooth blend order was inverted in the coordinator brief** (`a=blurred, b=src` would have smoothed everything *except* the skin). The GPU worker caught it from the required test behavior, implemented `a=src, b=blurred`, and documented it. The same inversion was corrected in the engine worker's region-mask call (`a=original, b=effected`) via a mid-task message.
2. **Latent engine bug**: the effect-slot loop swapped `cur`/`other` unconditionally even when an fx method returned `Ok(false)` (zero GPU work, e.g. `amount = 0`), promoting an uninitialized texture into the pipeline. Now swaps only on `Ok(true)`.
3. **Pre-existing test race** in sf-filters' `half_ev_registry()` (parallel tests sharing one temp dir via `remove_dir_all`, failing consistently) — serialized with a static Mutex, test-only change.
4. Doc nit (left as-is, flagged): `MaskRef::Circle` docs say "radius = fraction of frame height" but `gen_region_mask` uses uv-space distance with no aspect correction (identical on square frames).

## C8 — Profile + optimize, preset packs, polish (2026-10-09) — COMPLETE

`cargo test --workspace` → **189 passed, 0 failed, 3 ignored** (was 187; +2 new: LUT + packs integration), zero build warnings, `cargo fmt --check` clean. The 3 ignored are by design: 2 perf-profile harnesses + the soak test (run explicitly, see below).

### C8 pipeline profile (1080p, lavapipe software rasterizer — RELATIVE numbers only)

Measured with the new `#[ignore]`d harness `crates/sf-engine/tests/perf_profile.rs` (CPU wall-clock around each stage with `device.poll(Maintain::Wait)`; no timestamp queries — backend-agnostic for the DX12 shipping target). Stack: teal-orange look + vignette + film grain + sharpen; waveform + vectorscope subscribed. The harness drives the public building blocks through exactly the dispatch sequence `Engine::grade_input_frame` executes, and asserts its output is **byte-identical** to the engine's — so it measures the real pipeline, not a lookalike. 30 measured frames after 3 warmup:

| stage    | dispatches/frame | mean ms | % of frame |
|----------|------------------|---------|------------|
| upload   | 0 | 2.27 | 0.3% |
| grade (fused) | 1 | 255.59 | 39.4% |
| vignette | 1 | 86.45 | 13.3% |
| film grain | 1 | 60.40 | 9.3% |
| sharpen (unsharp) | 1 | 115.29 | 17.8% |
| output encode | 1 | 92.17 | 14.2% |
| download | 0 | 8.70 | 1.3% |
| scopes (downscale + waveform + vectorscope) | 3 | 27.52 | 4.2% |
| **TOTAL** | **8** | **648.39** | 100% |

Engine end-to-end (`grade_input_frame`, same stack, no scope subscribers): **607.55 ms** mean frame on lavapipe. Relative read: the fused grade dominates (39%), sharpen is the costliest single effect (17.8%, neighbor taps), scopes are cheap (4.2% for 2 kinds at 15 Hz). On a real GPU these absolute numbers collapse — the value here is the dispatch budget: **8 dispatches/frame** for this stack, which is the number to defend on DX12.

### C8 optimizations applied (low-risk only; pixel output provably unchanged — full suite green)

1. **Reused per-frame working targets** (`WorkTargets` in sf-engine): the two linear ping-pong textures + final RGBA8 target persist across frames, recreated only when the frame size changes — instead of allocating ~40 MB of textures fresh every frame at 1080p. Every pass fully overwrites its target each frame, so reuse cannot change pixels.
2. **Shared 16-byte param buffer** (`GradePass::params_buffer`): allocated once, rewritten via `queue.write_buffer` only when the value changes, instead of one buffer allocation per call.

### C8 scope-cadence flake — fixed properly

`ScopeHub` gained an injectable `ScopeClock` (wall clock in production; tests install a manual clock via `set_manual_clock`/`advance_manual_clock`, and advancing without a pinned clock panics so wall/manual time can never mix). `scope_cadence_throttles_to_15hz` now pins the clock and advances 16.6 ms/frame — 5 frames span 66.4 ms < 66,667 µs period, so exactly 1 emission is **guaranteed regardless of software-GL speed** (the old wall-clock version flaked ~1-in-5 on cold lavapipe). Same assertions, same proof — deterministic now. `scopes_tap_the_graded_frame`'s 80 ms sleep likewise replaced with a clock advance.

### C8 shipped `podcast_warm.cube` (deviation carried since C0 — closed)

Committed generator `tools/gen-lut/` (new workspace member): writes a deterministic 33³ LUT to `assets/luts/podcast_warm.cube` — amber shadows lift, midtone warmth bump, +6% saturation around luma, matching the look's params (5400 K, warm gain). Note: the engine resolves LUT paths relative to the process cwd; the `.sflook`'s `color/luts/podcast_warm.cube` is the project-bundle convention. Covered by `lut_integration.rs`: generate → parse → spot-check → committed-asset == fresh-output byte check → `podcast-warm.sflook` loads via `LookRegistry` and applies in the engine (missing/unparseable LUT would fail — success proves found+parsed+GPU-uploaded).

### C8 preset packs — `assets/packs/`

- `creator-podcast.sfstack` — Podcast Warm @0.8 → Vignette → Skin Smooth
- `creator-vlog.sfstack` — Vlog Fresh @0.9 → Sharpen
- `cinematic-teal-orange.sfstack` — Teal & Orange @1.0 → Film Grain → Vignette → Letterbox 2.39:1
- `golden-hour-glow.sflook` (Creator, 5200 K) and `arctic-clean.sflook` (Natural, 7200 K) — community-style looks
- Validated by `packs_integration.rs`: every pack file parses, respects the stack rules (1–8 slots, ≤2 looks, intensities 0–1), and applies end-to-end on the real GPU.

### C8 soak test — PASS

New `#[ignore]`d `crates/sf-engine/tests/soak.rs` (`STREAMFLOW_SOAK_FRAMES` overrides, default 1000). Ran once headless at N=1000: **PASS in 491.9 s (2.0 fps on lavapipe)** — zero panics/errors; effect_passes exactly 6000 (6/frame, linear); mask_uploads exactly 67 (ceil(1000/15)); grade cache 2997 hits / 3 misses; 2000 scope frames drained with contiguous seqs, never exceeding the 15 Hz throttle; ai_skipped == 0. 2-hour variant: `STREAMFLOW_SOAK_FRAMES=216000 cargo test -p sf-engine --test soak -- --ignored --nocapture` (2 h of *footage*; on lavapipe ≈33 h wall-clock — realistic only on a real GPU).

### C8 deviations / notes

- Timing table is lavapipe-relative by construction (documented in the harness); no DX12 numbers exist yet — first real-GPU run should re-profile.
- Before/after per-optimization deltas were not separately measured: both optimizations eliminate allocations, not shader work, and their safety case is "pixel-identical output" (proven by the green suite), not a timing delta.
- `tools/gen-lut` is a build/asset tool, not engine code; it runs at asset-authoring time.
- Out of scope (not started, per the C8 brief): Tauri+React shell, sf-capture/sf-encode/sf-output/sf-audio/sf-replay/sf-graphics/sf-monitor, ONNX model vendoring — see "What's stubbed".

## Decisions

- **wgpu 22.1.0** (crates.io, resolves clean). Verified API notes: `DeviceDescriptor` has `memory_hints` (using `MemoryHints::Performance`); `device.poll(Maintain::Wait)`; `texture_storage_2d<rgba8unorm, write>` needs no extra features; `RequestAdapterOptions` is a type alias (`compatible_surface: None` works unturbosfished).
- **Curves texture is 1024×2 RGBA32F**, sampled with `textureLoad` + manual lerp (no filtered sampling → no row bleed). `BakedCurves.data` is channel-major on CPU; the render upload interleaves to RGBA texels.
- **LUT normalized to 33³ rgba16float** at load; 3D upload pads rows to 256-byte alignment (33·8=264 → 512). LUT sampling is texel-center-correct trilinear `(c·32+0.5)/33`, exactly matching the CPU `x = c·32` mapping.
- **Temperature semantics (corrected during implementation):** the temp slider is a *creative* control — lower K = warmer image (matches spec's "Podcast Warm" @ 5400 K). `kelvin_to_gains` uses the green-normalized ratio `(bb(K)/bb(6500))/green`, giving exactly `[1,1,1]` at 6500 K and `[1.384, 1.0, 0.681]` at 3200 K. Computed on CPU, passed to the shader as `wb_gain` — WGSL never implements color-temperature math.
- **`rgb_to_hsv` quirk:** the specified branch structure is not standard HSV (red-max branch omits /6). Implemented identically on CPU and GPU — agreement is the contract; do not "fix" one side.
- **Sharpness is a second compute pass** (unsharp in linear light), not folded into the fused pass — it needs neighbor taps. `apply_grade_cpu` does not model it (documented).
- **Identity grade short-circuits before any GPU work** (`GradePass::apply` → `Ok(false)`; engine returns input bytes unchanged → bit-exact).

## GPU verification evidence (lavapipe, Vulkan, headless — 2026-10-09)

Real shader execution (no mocks), 64×64 gradient frames, GPU output vs `apply_grade_cpu`:

| Test | Result |
|---|---|
| `upload_download_roundtrip` | byte-exact |
| `identity_pass_skipped` | `Ok(false)`, no GPU work |
| `exposure_matches_cpu` (+1 EV) | max abs diff **1/255** (limit 2) |
| `curves_match_cpu` (master lift + R pull) | max abs diff **1/255** (limit 3) |
| `lut_identity_matches_cpu` (17³ identity cube) | **0/255** vs CPU, **0/255** vs input (limit 4) |
| engine integration (identity → exact bytes; +1 EV → mean luma 126.00 → 160.12) | pass |

(XDG_RUNTIME_DIR warnings in test output are harmless lavapipe/loader stderr noise.)

## Deviations from the spec

1. Curves texture is 1024×**2** RGBA (not 1024×1): second row carries luma + hue-vs-sat. Same data, documented layout.
2. LUT applies in linear working space at both positions (spec is silent on the signal domain); the FFmpeg-`lut3d` ΔE<1 comparison is approximated by GPU-vs-CPU-reference instead (documented in code).
3. Temperature default 6500 K numeric (spec: "camera native" — a UI concept; 6500 K = D65 = identity gains).
4. `podcast_warm.cube` referenced by `podcast-warm.sflook` does not exist yet (ships with LUT library work); the JSON reference is in place.

## What's stubbed (later phases)

`sf-capture`, `sf-audio`, `sf-encode`, `sf-output`, `sf-replay`, `sf-graphics`, `sf-monitor`, `apps/desktop` (Tauri+React, later). `sf-ai-runtime` ships the `InferenceSession` trait + `StubSession` (no vendored ONNX model — C6 decision); the C7 AI effect kinds consume it with documented model→hue-fallback policy. Per-frame LUT file reads are cached in `Engine.lut_cache`; grade uniform/curve/LUT GPU resources are dirty-flag cached in `GradePass` (C4); AI masks are cached per input and refreshed every 15 frames (C7).

## UI shell — Tauri v2 + React + Vite + TypeScript (2026-10-09)

`apps/desktop/` implements the Color Studio screen (spec §8, PDF p.11).
Verified headless (no display on dev machines — the app window itself can't
run here).

### Architecture decision (the important one)

ALL engine-binding logic lives in the plain Rust crate
`apps/desktop/backend` (`sf-ui-backend`) — **no `tauri` dependency** — so it
is fully unit-testable headless. `apps/desktop/src-tauri` is a thin adapter:
Tauri commands → `Shell` methods, and a 66 ms background thread polls
`Shell::drain_events()` and re-emits each [`UiEvent`] as a Tauri `"sf-event"`
payload (the frontend routes on the spec §9 `event` tag). The adapter is
small enough to verify by inspection.

### What's built

- **Backend crate** (`sf-ui-backend`): `Shell` wraps the real engine —
  `color.set_param` (spec §4.1 ranges, clamped; wheel channels as
  `lift_r`/`gain_g`/… suffixes), `color.set_curve`, `color.load_lut`,
  `color.reset` (basic/wheels/curves/lut/all), `color.auto` and
  `color.match` (land as editable params + explanations), full filter-stack
  ops (add/reorder/intensity/toggle/remove), `look.apply`, scopes
  subscribe/unsubscribe, `preview.get_frame` (PNG data URL, ~10 Hz
  throttled), `apply_grade` (A/B slots + before/after compare), look/effect
  catalogs, stack snapshots. GPU-cost warning emitted past 6 stack slots
  (spec §5.3). Input pixels are a deterministic synthetic test chart
  (`Shell::synthetic_frame`) until `sf-capture` lands — the preview still
  runs the real `grade_input_frame` pipeline.
- **Tauri adapter** (`src-tauri`): 20 commands (snake_case args matching the
  Rust signatures), event pump, `tauri.conf.json` (identifier
  `com.streamflow.lite`, 1440×900 window, `../dist` frontend).
- **Frontend** (`src/`): zustand store (optimistic sliders, trailing 60 Hz
  backend sync per spec §8, double-click = reset), typed IPC client,
  Header (A/B slots, Before/After, Reset), PreviewPane, ScopesRow
  (waveform/vectorscope w/ 123° skin-tone guide/histogram in channel colors),
  Basic/Pro/Curves/LUT/Filters tabs, LooksStrip (30 built-ins), Toasts.
  Dark theme, accent `#3B82F6`.

### Verification (all green, 2026-10-09)

| Check | Result |
|---|---|
| `npm run build` (`tsc --noEmit` + vite) | ✅ clean, 243 KB JS (76 KB gzip) |
| `cargo test -p sf-ui-backend` | ✅ 11/11 (param roundtrip/clamp, reset, curves, LUT ref, stack add/reorder/intensity/toggle/remove, stack limit + warning, look apply, PNG validity + throttle, scope subscribe → frames, auto-color applies, event serde tags) |
| `cargo check --workspace` | ✅ no errors/warnings |
| `cargo check` on `src-tauri` | ✅ clean (GTK/WebKit dev packages apt-installed: `libgtk-3-dev`, `libwebkit2gtk-4.1-dev`, `librsvg2-dev`, `patchelf`; `tauri` 2.12.x + `tauri-build` 2.7.x — note the build crate is versioned independently) |

### Deviations / notes

- `FilterTarget::Scene`/`Program` stacks are wired through (state supports
  them); the UI's Filters tab targets the input stack only for now.
- LUT tab takes a typed project-relative path — the Tauri dialog plugin
  (real file picker) and drag-and-drop import are still to come.
- Pro tab uses per-channel sliders (1:1 with engine wheels); puck-style
  wheels are UI polish for later. Spec §4.2 color-balance wheels and
  selective color have no engine fields yet — not shown.
- Single `cam1` input until `sf-capture` lands; "Apply to all cameras" is
  therefore absent (the backend `apply_grade_to_all_cameras` exists).
- Preview is PNG polling (~10 Hz); production gets a zero-copy path — the
  `preview_get_frame` command shape stays.

### Explicitly next

Remaining screens (multiview, streaming/recording controls), Tauri dialog
plugin, multi-input switching, ONNX model vendoring, `sf-capture` /
`sf-encode` / `sf-output` / `sf-audio` / `sf-replay` / `sf-graphics` /
`sf-monitor`, Windows installer/CI (GitHub Actions Windows runner).

## Production screens — backend (2026-10-09)

`apps/desktop/backend` (`sf-ui-backend`) gained the full production state
machine; `apps/desktop/src-tauri` is still a thin adapter (32 new commands,
`transition_tick()` called in the 66 ms poll loop before `drain_events()` —
**not** exposed as a command). All logic is headless-unit-testable; no
engine crates were modified.

### What's built

- **Scenes** (backend-owned `ProdScene{id, name, sources, filters: FilterStack}`,
  reusing the `sf-filters` stack type): `scene_create/rename/delete/duplicate/list`.
  Deleting the last scene errors; deleting the program/preview scene falls back
  to the first remaining scene. Duplicate deep-copies sources with fresh ids,
  fresh engine inputs, and fresh mixer strips.
- **Sources** `SourceKind::{TestPattern, Color{r,g,b}, Camera, Screen, Media{path}, Text{text}}`.
  REAL: `TestPattern` (reuses the synthetic test-chart generator) and `Color`
  (solid frame) — each gets an engine input via `add_input` and renders through
  the real `grade_input_frame` pipeline. PENDING: `Camera`/`Screen`/`Media`/`Text`
  report `status: "pending"` with reason `"capture backend not implemented
  (sf-capture stub)"`, get no engine input, and are skipped in the composite —
  never faked. CRUD: `source_add/remove/reorder/set_visible` (`order` = bottom-to-top).
- **Scene render**: `scene_render_png(scene_id, w, h)` — every visible real source
  through the real pipeline, CPU alpha-"over" composite in source order, PNG-encoded
  (reuses the existing `image`/`png` path). Command `scene_preview_png` wraps it
  in a PNG data URL.
- **Preview/program + transitions**: `production_set_preview/set_program`,
  `transition_cut` (instant, emits `transition_done`), `transition_auto(duration_ms,
  kind="mix")`. `transition_tick()` advances by wall-clock elapsed with smoothstep
  easing (`t²(3−2t)`); emits `transition_progress` per tick, commits program = preview
  + `transition_done` on completion. The easing math is a pure function
  (`transition_progress(elapsed_ms, duration_ms) -> f32`) tested without sleeping.
  During a transition the program monitor frame is the live crossfade
  (`program_frame_png`, backend-internal, not a command).
- **Mixer**: per-source `{gain: 0.0..=2.0 clamped, mute}` + master; `mixer_set_gain/mute`,
  master variants; meters always `0.0` with `meters_live: false` (honest stub until
  `sf-audio` — never faked).
- **Streaming**: `StreamSettings{url, key, platform}` — key has `#[serde(skip_serializing)]`
  and a custom `Debug` impl redacting to `"[redacted]"` (both tested). Lifecycle
  `Idle|Connecting|Live|Reconnecting|Stopping|Error{message}`; `stream_start` fails
  honestly → `Error{"streaming backend not implemented (sf-output stub)"}` + `Err`
  (no double-start); `stream_stop` → `Idle`.
- **Recording**: `RecordSettings{path, format: Mp4|Mkv, quality}`, lifecycle
  `Idle|Recording{elapsed_secs}|Paused{elapsed_secs}|Error{message}` (elapsed excludes
  pauses, tracked from `Instant`; `Instant` never serialized). `record_start` fails
  honestly like stream (`sf-encode stub`); `pause/resume/stop` validate transitions.
- **Macros** (minimal, real): `Macro{id, name, hotkey, action}` with
  `MacroAction::{Cut, AutoTransition{duration_ms}, ToggleStream, ToggleRecord,
  ApplyLook{look_id, intensity}}`; `macro_add/remove/list/dispatch(hotkey)`.
  Dispatch executes against the state machine and returns a `happened` summary;
  `ApplyLook` lands on every visible real source of the program scene through the
  real `look_apply` pipeline (spec §7 live look-switch).
- **Events** (all `#[serde(tag="event")]`): `production.scene_changed`,
  `production.source_changed`, `production.transition_progress{progress}`,
  `production.transition_done`, `production.stream_state{state}`,
  `production.record_state{state}`, `production.mixer_changed`.

### Verification (all green, 2026-10-09)

| Check | Result |
|---|---|
| `cargo test -p sf-ui-backend` | ✅ 24/24 (11 existing + 13 new: scene CRUD + duplicate/delete-last guards, source add/remove/reorder/visibility + pending honesty, real-pipeline PNG validity + non-blank decode + composite order + visibility, cut, pure easing math 0→1 + live 200 ms transition commit, mixer clamp/mute, stream/record honest-error paths + key redaction in Debug + serde, macro dispatch cut/auto/apply_look) |
| `cargo test --workspace` | ✅ 213/213, 0 failed |
| `cargo check --workspace` | ✅ no errors/warnings |
| `cargo check` on `src-tauri` | ✅ clean |
| `cargo fmt --check` | ✅ clean (backend + src-tauri) |

### Deviations / notes

- Scene-level `FilterStack`s are modeled and stored (spec §5.3) but **not applied**
  in the composite — applying an arbitrary stack to a composited frame needs
  engine support that doesn't exist yet. Input-level stacks (incl. macro
  `ApplyLook`) run through the real pipeline.
- The initial shell state has one "Scene 1" with a default test-pattern source,
  so the program monitor is never blank.
- `transition_auto` to a new target replaces nothing: starting while one is
  active errors; `production_set_program` cancels the in-flight transition;
  `production_set_preview` retargets it.
- `program_frame_png` (crossfade monitor) is a backend method, not a command —
  the frontend polls `scene_preview_png` per monitor; wiring the live crossfade
  into the program monitor is frontend work.

### Explicitly next

Frontend production screens (Worker 2): scenes/sources panels, preview/program
monitors, transition controls, audio mixer UI, streaming/recording panels,
macro bar — consuming the 32 commands above.

## Production screens — coordinator integration (2026-10-09)

Backend (Worker 1) and frontend (Worker 2) were built in parallel against a
shared command-name contract. All 32 command names matched, but a consistency
audit found **5 argument/shape mismatches** that would have failed at runtime
(tsc cannot see across the IPC boundary). Fixed on the frontend side (backend
is the tested source of truth):

1. `source_add`: frontend sent `{scene_id, kind: <tagged union>, name}`; the
   adapter takes flat `(scene_id, kind: "test_pattern"|"color"|..., name,
   color_r/g/b: u8, text?, path?)`. Fixed: `ipc.ts` translates the tagged
   `SourceKind` union into flat args (null -> None for the Options).
2. `macro_add`: frontend sent a free-text action string; the backend expects
   the internally-tagged `MacroAction` (`{"action": "cut"}` /
   `{"action": "auto_transition", "duration_ms": n}` / ...). Fixed: typed
   `MacroActionInput` in `types.ts` + an action selector with conditional
   fields in `MacrosPanel` (no more free text).
3. `production_state()` shape: the backend returns flat
   `stream_settings`/`stream_lifecycle`/`record_settings`/`record_lifecycle`
   (lifecycles are `{"state": ...}` objects), `mixer.channels` as a
   `{source_id: strip}` map, and `SourceState.kind` as a string-or-tagged
   object — the panels expected nested `stream`/`record` objects, a channels
   array, and a plain kind string (`.toLowerCase()`/`.map()` would have
   crashed at runtime). Fixed: `normalizeProductionState()` in
   `productionStore.ts` reconciles everything in one place (channels gain
   their display names by joining scene sources; kind/macros get display
   labels); lifecycle error messages surface as dismissible error pills.
4. `stream_set_settings`: backend parses lowercase platform ids
   (`youtube|twitch|facebook|custom_rtmp`); the UI sent display labels
   (`"YouTube"`). Fixed: mapped at the store call site.
5. `transition_auto`: backend accepts only `"mix"`; the UI sent `"Mix"`.
   Fixed: lowercased at the call site.

### Verification (all green, 2026-10-09, independently re-run by coordinator)

| Check | Result |
|---|---|
| `cargo test --workspace` | ✅ 213/213, 0 failed (exit 0) |
| `cargo test -p sf-ui-backend` | ✅ 24/24 |
| `cargo check` on `src-tauri` | ✅ clean, 0 warnings |
| `npm run build` (tsc --noEmit + vite) | ✅ clean |
| Command-name audit (52 frontend invokes vs 57 adapter fns) | ✅ every invoked command exists; arg shapes reconciled (items 1–5 above) |

### What's real vs. stub (production screens)

- REAL: scene/source CRUD, preview/program buses, cut + smoothstep mix
  transitions (66 ms tick in the adapter poll loop), CPU over-composite of
  real graded source frames, mixer state, macro dispatch incl. ApplyLook
  through the real look pipeline, stream-key redaction (tested).
- HONEST STUBS: camera/screen/media/text sources (pending badge, skipped in
  composite), `stream_start`/`record_start` → lifecycle Error with
  "backend not implemented (sf-output/sf-encode stub)" + Err return (UI shows
  a red error pill, never a fake LIVE badge), mixer meters at 0 with
  `meters_live: false`.
- PLACEHOLDER: scene-level filter stacks stored but not applied in the
  composite (needs engine support); live crossfade in the program monitor
  (backend has `program_frame_png`, frontend polls per-scene PNGs);
  PNG-preview polling (zero-copy later); Windows installer/CI.

## Real AV backends — sf-capture / sf-encode / sf-output / sf-audio + shell wiring (2026-10-10)

The four backend crates went from stubs to real, and `sf-ui-backend` grew a
new `av_backends` module that wires them into the shell. `record.start` now
records for real; `stream.start` attempts a real RTMP connection; media-file
sources decode real video + audio; mixer meters are live.

### What was built

- **`sf-capture`**: `FrameProducer` trait (timestamped RGBA frames, PTS,
  documented drop/duplicate policy). `MediaFileProducer` — real FFmpeg
  decode (video via `ffmpeg-next` 6 + swscale, audio resampled to 48 kHz
  stereo f32). `CameraProducer` (v4l2) and `ScreenProducer` (x11grab):
  code-complete, logic-tested (device-enum parsing, format negotiation,
  timestamp math); hardware paths untestable on this VM. Windows
  MediaFoundation/DXGI scaffolds under `cfg(windows)` — honest `todo!()`s.
- **`sf-encode`**: `VideoEncoder`/`AudioEncoder` on FFmpeg. Real encoder
  selection (nvenc → qsv → amf → libx264 → mpeg4, injection-tested). Real
  libx264 roundtrip verified: 60 frames in, 60 out, 41 fps, PSNR 31.4 dB.
  AAC audio encoder.
- **`sf-output`**: `Recorder` (fragmented MP4 + MKV, ffprobe-verified).
  `Streamer` with `Transport` trait: `RealTransport` (RTMP/FLV via
  libavformat) + `ReconnectPolicy` exponential backoff (fake-transport
  tested). URL redaction (`redact_url`), stream key never logged/serialized.
- **`sf-audio`**: real `Mixer` — per-source gain/mute, master, hard-clamp,
  peak/RMS meters on real decoded audio; AI insert points documented.
- **`sf-ui-backend::av_backends`**: `Shell::av_tick()` (called from the
  adapter's 66 ms poll loop after `transition_tick`) drives audio metering,
  recording, and streaming. Media producers open on `source_add` (bad file
  fails before the source is pushed), freeze on last frame at EOS, drop on
  `source_remove`. Program bus captured at 640×360@15 fps (libx264 ultrafast
  fits the poll budget on 2 CPUs — production moves to a thread at 1080p60).
  Recording: fragmented MP4/MKV via libx264 + AAC. Streaming: real RTMP
  attempt; unreachable → clean `Error` lifecycle with redacted message,
  reconnect policy live. Meters flow to the mixer UI (`meters_live` flips
  true once media audio flows); frontend `MixerPanel` now renders real
  levels (type gap `MixerView.meters` also fixed in `types.ts`).

### Verification (all green, 2026-10-10, independently re-run by coordinator)

| Check | Result |
|---|---|
| `cargo test --workspace` | ✅ 277/277, 0 failed (exit 0) — 272 baseline + 5 new |
| `cargo test -p sf-ui-backend` | ✅ 29/29 (13 production + 11 lib + 5 new av_backends) |
| `cargo fmt --check` | ✅ clean |
| `npm run build` (tsc + vite) | ✅ clean |
| Record proof | ✅ 5 ticks → `/tmp/sf-be-record.mp4`, ffprobe: h264, **exactly 5 frames** |
| Stream failure proof | ✅ `rtmp://127.0.0.1:1935` refused → `Error` lifecycle, key absent from message + lifecycle |
| Media proof | ✅ generated testsrc+sine file → source `ready`, program PNG non-blank, `meters_live: true`, peak > 0.01 |

Four old tests asserted the honest-stub contract (`*_fails_honestly`); they were
rewritten to the new real contract rather than deleted (same coverage, real
behavior). One self-inflicted incident: two concurrent `cargo test --workspace`
runs raced on the fixed `/tmp` fixture paths — never run the suite twice in
parallel.

### VERIFIED vs. WRITTEN-FOR-WINDOWS

| Component | Status on this VM | Notes |
|---|---|---|
| Media-file decode (video+audio) | ✅ VERIFIED | frame count, PTS order, real meters |
| libx264 + AAC encode → decode roundtrip | ✅ VERIFIED | 41 fps, PSNR 31.4 dB |
| MP4/MKV recording to file | ✅ VERIFIED | ffprobe: playable, exact frame count |
| RTMP connect-failure path + key redaction | ✅ VERIFIED | connection-refused → clean Error |
| Reconnect/backoff state machine | ✅ VERIFIED | fake transport, unit-tested |
| Mixer math + meters | ✅ VERIFIED | hand-computed values matched |
| v4l2 camera / x11grab screen | ⚠️ CODE-COMPLETE, logic-tested | no camera/X server here; end-to-end unverified |
| MediaFoundation/DXGI capture | ❌ WINDOWS-ONLY scaffold | `cfg(windows)` `todo!()`s, compiles-by-shape only |
| NVENC/QSV/AMF encoder selection | ⚠️ LOGIC-TESTED | selection real; HW encoders absent here |
| RTMP to a live server | ❌ UNTESTED | no reachable RTMP endpoint on this VM |
| 1080p60 record/stream on a thread | ❌ NOT YET | shell polls at 640×360@15 fps by design |

### What's next

ONNX model vendoring, Windows CI, real-GPU DX12 profiling + 2-hour soak,
`sf-graphics`/`sf-replay`/`sf-monitor` stubs, zero-copy preview path.

### Integration find: FFmpeg contexts are `!Send` (fixed 2026-10-10)

Wiring the producers/encoders into `Shell` broke `src-tauri`'s `cargo
check`: Tauri's `State` requires `Send`, but `MediaFileProducer` and
`VideoEncoder` hold FFmpeg contexts (raw `*mut` pointers, conservatively
`!Send`). Fix: `SendProducer` newtype + `unsafe impl Send` on the
crate-private `ActiveRecording`/`ActiveStream` session structs in
`av_backends.rs`, each with a SAFETY comment. The argument: FFmpeg
contexts carry no thread affinity, and every session is only ever touched
through the shell's `Mutex` (commands + poll loop lock it exclusively), so
serialized cross-thread use is sound. `src-tauri` check is clean again.

### Realtime blueprint batch (2026-10-11, commit f503302)

Per `stucture_for_smooth_video.md` v1.0 (never accumulate frames):

- Media tick split: `av_tick` on dedicated fixed 33.3ms thread (30fps master
  tick); UI events/transitions on the 66ms poll loop. `AV_FRAME_US` 66_667
  -> 33_333.
- dshow: MJPEG-first negotiation (`vcodec=mjpeg` -> yuv420p -> size+fps ->
  negotiated); read timeout 100ms -> 25ms; negotiated codec/res/fps recorded
  and exposed via new `camera_format` Tauri command.
- Preview independence: live program feed serves preview from the tick's
  cached frame (downscaled); idle preview still pulls directly.
- Drop counters: `skipped_render` vs `dropped_network` in perf stats + panel.
- composite.wgsl 4-binding shader reconciled with composite_pass.rs;
  `tests/composite_gpu.rs` validates blend math on lavapipe (2/2 pass).

Tests: 279/279 workspace pass. dshow path is Windows-only: compiles
untouched on Linux CI; API use verified against ffmpeg-next 6.1.1 sources.
Hardware verification (AMD PC) still pending — no installer cut from this.
