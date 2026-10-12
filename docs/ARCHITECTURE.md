# StreamFlow Lite — Architecture (distilled from the spec PDF)

Source of truth: `~/workspace/user/files/StreamFlow_Lite___Color_Studio___Filters_Architecture.pdf`
(14 pages; page 11 is the Color Studio UI layout diagram.)

## Goal

StreamFlow Lite's biggest differentiator is **Color**. The spec defines the Color Studio
and filter system in full detail, with the rest of the app as a lite structure around it.

## Process model

**UI process (Tauri + React) ↔ Engine process (Rust) ↔ AI worker (ONNX).**
The UI only sends parameters; the engine applies everything on the GPU.

## Crate layout (`crates/`)

| Crate | Responsibility |
|---|---|
| `sf-engine` | Ties everything together; engine lifecycle |
| `sf-state` | Project state: inputs, scenes, per-input color, filter stacks |
| `sf-capture` | Camera / screen / media capture (Windows: MediaFoundation/DXGI) |
| `sf-render` | wgpu context, pipelines, texture upload/download, fused grade pass |
| `sf-color` | Color science: `ColorGrade` params, curves bake, `.cube` parsing, CPU reference |
| `sf-filters` | Look presets (`.sflook`), `FilterStack` rules, effect kind/param models |
| `sf-filterfx` | Effect-filter GPU passes (WGSL) + CPU references |
| `sf-scopes` | GPU scopes: waveform, vectorscope, histogram, false color, zebra |
| `sf-audio` | Audio engine: clocked bus (exact samples per tick), live-input capture threads, mixer, real-time voice chain (RNNoise AI denoise → gate → EQ → compressor, 20 ms fixed latency) |
| `sf-encode` | Hardware encoders (NVENC/QuickSync/AMF via wgpu-interop or FFmpeg) |
| `sf-output` | Streaming / recording outputs |
| `sf-replay` | Replay buffer |
| `sf-graphics` | Text / lower-thirds / overlays |
| `sf-ipc` | UI↔Engine command/event types (serde JSON) |
| `sf-project` | `.streamflow` project format (`project.json` + `color/luts/`, `color/looks/`, `color/stacks/`) |
| `sf-ai-runtime` | ONNX Runtime: face/skin mask, segmentation |
| `sf-monitor` | Perf telemetry, GPU cost warnings |

Assets: `assets/luts/` (built-in `.cube`), `assets/looks/` (built-in `.sflook` JSON),
`assets/packs/` (preset packs: `.sfstack` stacks + community `.sflook` looks),
`assets/test-charts/` (reference images for QA).

## Color pipeline — per-input chain (order matters, §3.1)

1. Decode NV12/YUY2/BGRA → RGB texture (GPU)
2. Range fix Limited (16–235) → Full (0–255) if needed
3. Linearize (gamma 2.4 / sRGB / Rec.709 → linear light)
4. Exposure (EV multiply)
5. White balance: temperature + tint (chromatic adaptation)
6. Primary grade: Lift / Gamma / Gain / Offset wheels
7. Tone: contrast, highlights, shadows, whites, blacks
8. Saturation, vibrance, hue, color balance
9. Curves: RGB, R, G, B, luma, hue-vs-sat (1D LUT lookup, 1024×4 RGBA)
10. 3D LUT `.cube` (33³), intensity blend; position before/after grade (setting)
11. Detail: sharpness (unsharp mask)
12. Output encode linear → display gamma, clamp

The whole chain is **fused into one shader** (compute or fragment); only sharpness and
the LUT texture fetch add extra memory access. Internal precision: `rgba16float`.

Working space: Rec.709/sRGB primaries, display gamma 2.4 (or sRGB, user setting).
8-bit and 10-bit inputs accepted. HDR/10-bit camera support is Phase 2+ (v2).

### Performance design

- Parameters → one uniform buffer per input, updated only on change.
- Curves baked on CPU to a 1024×4 1D LUT texture, re-baked only on curve edit.
- 3D LUT: 33³ `rgba16float`, created once at load.
- Identity grade → color pass bypassed entirely.
- One input reused in several scenes → graded texture reused.
- Target: <1 ms GPU per input color pass at 1080p60 on a mid-range GPU
  (measure in C8 — a target, not a guarantee).
- Scopes run on a 256×144 downscaled frame at 15 Hz, only while the scope panel is visible.
  The scope tap is post-effects (scopes see exactly the program output), and
  with zero subscribers no GPU scope work happens at all.

## Color Studio controls

**Basic tab:** Exposure (−3…+3 EV), Brightness (−100…+100), Contrast (−100…+100),
Saturation (0…200%), Vibrance (−100…+100), Temperature (2500K…10000K), Tint (−100…+100),
Highlights/Shadows (−100…+100), Whites/Blacks (−100…+100), Gamma (0.5…2.5), Sharpness (0…100).

**Pro tab:** Lift/Gamma/Gain/Offset wheels (hue/sat puck + luma slider each),
Shadows/Midtones/Highlights color balance, Hue-vs-Hue / Hue-vs-Sat / Hue-vs-Lum curves,
selective color (6 ranges: R,Y,G,C,B,M — hue/sat/lum shift).

**Curves tab:** RGB master, R, G, B, luma, hue/sat. Max 16 spline points per curve,
monotone cubic interpolation. Double-click = delete point, Shift = fine adjust,
per-curve reset.

**LUT tab:** import `.cube` (1D and 3D; sizes 17/25/33/65 — resample to 33³ with warning),
intensity slider, bypass, position before/after grade, library with thumbnail preview
(current camera frame with LUT applied), drag-and-drop import, relative project paths.

**Scopes:** waveform (luma + RGB parade), vectorscope (with skin-tone line guide),
histogram, false color (exposure check), zebra overlay.

**Compare tools:** before/after split slider or toggle (backslash), reference still
freeze for side-by-side, A/B grade slots (two saved grades, one-click switch).

## Filter system

Two kinds:

**Look filters (color-only)** = parameter preset + optional LUT. One click applies a full
look; intensity slider scales it. Categories: Natural/Clean, Cinematic, Creator, Vintage,
B&W, Event, Gaming/Screen, Fix. Each look is a `.sflook` JSON:

```json
{
  "name": "Podcast Warm", "version": 1, "category": "Creator",
  "params": { "exposure": 0.1, "contrast": 8, "saturation": 104,
              "temperature": 5400, "tint": 2,
              "lift": [0.0, 0.0, 0.01, 0.0], "gain": [1.03, 1.0, 0.97, 1.0] },
  "lut": { "file": "color/luts/podcast_warm.cube", "intensity": 0.6 },
  "skin_protect": true
}
```

**Effect filters (spatial/temporal):** sharpen/unsharp, gaussian blur, AI background blur,
AI background replace, chroma key (+spill suppress), vignette, film grain, bloom/glow,
noise reduction (temporal), AI skin smooth, mirror/flip, crop/zoom/pan (digital PTZ),
lens correction, letterbox, chromatic aberration, pixelate/mosaic. Each carries an
estimated GPU-cost badge (Low / Med / High).

**Filter stack architecture** — filters attach at three levels: input level (per
camera/source), scene level (layers/overlays), program level (global look on final output).
Rules: order is drag-reorderable; each slot has enable/disable + intensity (0–100%);
max **8 filters per stack** (lite limit, warn "GPU cost high" beyond); max **2 look
filters** per stack; whole stack savable as `.sfstack`.

```rust
pub struct ColorGrade {
    pub exposure: f32, pub brightness: f32, pub temperature: f32, pub tint: f32,
    pub contrast: f32, pub saturation: f32, pub vibrance: f32,
    pub highlights: f32, pub shadows: f32, pub whites: f32, pub blacks: f32,
    pub gamma: f32,
    pub lift: [f32; 4], pub gamma_wheel: [f32; 4],
    pub gain: [f32; 4], pub offset: [f32; 4],
    pub curves: CurveSet, pub lut: Option<LutRef>, pub sharpness: f32,
    pub skin_protect: bool,
}
pub enum Filter { Look { preset_id: String, intensity: f32 },
                 Effect { kind: EffectKind, params: EffectParams } }
pub struct FilterSlot { pub id: Uuid, pub filter: Filter, pub enabled: bool,
                        pub intensity: f32, pub mask: Option<MaskRef> }
pub struct FilterStack { pub slots: Vec<FilterSlot> } // max 8
```

## AI color features

- **AI Auto Color (one click):** luma percentiles (p1/p50/p99) → exposure offset
  `log2(target_median/current_median)` (target ~0.42, skin-aware); contrast from p5–p95
  spread; white balance from neutral/gray region or face estimate. Result lands as
  **editable parameters**, never black-box.
- **AI Camera Match:** 2–3 s of frames from reference + target → median stats →
  exposure (median luma ratio), WB (per-channel average ratio on neutral/face region),
  contrast (p5–p95 ratio), saturation (mean chroma ratio) applied smoothly (0.5 s).
  Pro mode: 24-patch ColorChecker → 3×3 matrix via least squares.
- **Skin tone protection:** face-detector (ONNX) skin mask, or hue range around the
  vectorscope skin line (~123°); hue shift/saturation boost limited inside the mask with
  soft falloff. `skin_protect: true` default on every look.
- **AI Look Suggest:** analyze current scene → suggest 3 looks with thumbnails.
- **Auto Drift Guard:** lighting drift during live → *suggest* re-match
  ("Cam 2 exposure drift ho gaya, re-match?") — never auto-applies.

## New features list (§7)

Look intensity slider, A/B grade slots, scene look lock, live look-switch macro
(hotkey, 0.3 s smooth fade), look morph slider, zebra + false color, broadcast-safe
clamp toggle, region masks (v2), auto lighting match for screen share, look
import/export (`.sflook` community packs), camera profiles (per-model starter presets),
per-scene color memory, skin-glow slider, night-mode boost, preview compare wall
(4 looks side by side).

## IPC

UI → Engine: `color.set_param`, `color.set_curve`, `color.load_lut`, `color.reset`,
`color.match`, `color.auto`, `filter.add`, `filter.reorder`, `filter.set_intensity`,
`filter.toggle`, `look.apply`, `scopes.subscribe`, `scopes.unsubscribe`.
Engine → UI: `color.changed`, `filter.stack_changed`, `scopes.frame` (15 Hz),
`color.match_progress`, `color.match_done`, `color.drift_suggest`, `gpu.cost_warning`.

## Project format (`.streamflow`)

`project.json` → `inputs[].color`, `inputs[].filters`, `scenes[].filters`,
`program.filters`; `color/luts/*.cube` (relative paths), `color/looks/*.sflook`,
`color/stacks/*.sfstack`.

## Error states

Bad `.cube` → show parse error, keep old LUT, log. Unsupported LUT size → resample to
33³ with warning. GPU overload → "Filter cost high" warning, suggest disabling heavy
filters (with user permission). AI model missing → AI filters grey out, non-AI keep
working. Camera disconnect → color params saved, re-applied on reconnect. Match fail
on dark frame → "frame too dark/blank", retry.

## Testing & quality

- Reference charts in `assets/test-charts/`; `.cube` accuracy: ΔE < 1 vs FFmpeg `lut3d`.
- Identity test: default params → output == input (bit-exact or ±1).
- Stress: 6 inputs × 4 filters, 2 h, log GPU time + drops.
- Visual regression: golden image per look, compared at build time.
- Skin test: apply looks on 10 skin tones, measure hue shift.

## UI layout — Color Studio (spec §8, PDF p.11)

Header: `Color Studio | [CAM 1 ▾] | A | B | Before/After | Reset`.
Left: large preview. Below preview: scopes (waveform | vectorscope | histogram).
Right: tabs [Basic][Pro][Curves][LUT][Filters] + sliders (Exposure +0.3, Contrast +8,
Temp 5400K, Saturation 104%, …). Bottom: looks strip ([Studio Clean][Podcast Warm]
[Cinematic][B&W] …).
Rules: slider drag → realtime GPU update, IPC throttled to 60 Hz; double-click slider
= reset; per-camera color tab + "Apply to all cameras"; compact mode shows only
Exposure, Temp, Saturation, Look picker; accent `#3B82F6`, dark theme, scopes use
natural channel colors.

## Audio pipeline (real-time)

```text
mic / camera audio ─► LiveInput thread (dshow, 20 ms packets) ─┐
media file audio  ─► paced pull (fills one block + 10 ms) ─────┼─► AudioBus
                                                               │   per-source jitter FIFO,
                                                               │   exact frames per tick
                                                               │   (1600 @ 30 fps), silence on
                                                               │   underrun, trim > 100 ms
voice sources only ─► Enhancer: RNNoise AI denoise → voice-aware gate → EQ → compressor
                    ─► Mixer (equal-length blocks) ─► AAC encoder / monitor / meters
```

* Enhancement runs on-device. A cloud/HTTP enhancement API cannot sit in a
  live audio path: each 33 ms block must be finished before the next one, and
  a network round-trip (100–500 ms + jitter) would either delay the whole
  stream by that much or drop audio.
* RNNoise is vendored in `crates/sf-audio/src/rnnoise` (from `nnnoiseless`
  0.5.2, BSD-3) with a built-in 960-point FFT, so `sf-audio` keeps zero
  dependencies. It is verified against Xiph's reference output.
* Fixed latencies: device packet 10–20 ms, bus queue typically 10–45 ms,
  AI stage 20 ms. Nothing can accumulate: the bus trims past 100 ms, the
  monitor past 120 ms.
