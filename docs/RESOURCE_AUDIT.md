# Resource audit (lightweight requirement)

Scope: every cache, buffer, per-frame copy, background loop and disk write
in StreamFlow Lite. Each item: what it is, whether it is needed, its bound,
and when it is released. Measurements: see "Before / after" at the end.

## Persistent (disk)

| Item | Needed? | Bound / cleanup |
|---|---|---|
| Temporary files | None are created by the app (checked: only tests write temp files). | Nothing to clean on startup or exit. |
| Error log `%LOCALAPPDATA%\StreamFlow\streamflow-errors.log` | Yes: crash evidence. | Rotates at 256 KB, keeps one `.old.log` (≤ 512 KB total). |
| LUT library, colour presets | User content (imported LUT copies, saved presets). | Never deleted automatically. |
| WebView2 cache | Not needed (the UI is bundled, not downloaded). | `--disk-cache-size=1048576`, `--disable-gpu-shader-disk-cache`, background networking and component updates off. |

## In memory (shown live in Diagnostics → Caches and buffers)

| Item | Needed? | Bound | Released |
|---|---|---|---|
| Newest frame per live source | Yes (mailbox, newest wins) | 1 frame per source | Source removed |
| Resized source frames | Yes (a source reused at another size in the same tick) | Current frame only, ≤ 16 entries (was unbounded during a resize drag of a frozen source) | Every new frame; source removed |
| Decoded images | Yes (decode once) | One per file in use | Last source using the file removed (was: kept forever) |
| Resized images | Yes (placement size) | ≤ 8 sizes and ≤ 64 MB (was 32 sizes, up to 64 MB each) | Image removed |
| Text rasters | Yes (text changes rarely) | ≤ 4 (was 32) | Source removed |
| Test-signal base | Yes (static part) | 1 | Size change |
| Colour-tool frame / preview PNG per input | Only for the Color page | 1 per input | Source removed (was: never) |
| Program monitor frame | Yes | 1 | Not rendered at all while nobody watches |
| Audio bus per channel | Yes (A/V alignment) | Hard cap 1 s of audio per source (alignment delay ≤ 800 ms) — unchanged | Source removed |
| Live capture mailbox | Yes | 1 frame | Source removed |
| Encoder → network queue | Yes | ≤ 256 packets; non-keyframe video dropped past 900 ms buffered (audio/keyframes never) — unchanged | Stream stop |

## Per-frame work removed

1. **GPU round trip for ungraded sources.** Every source, every frame, was
   uploaded to the GPU and read back (blocking) even with no grade — the
   result was a bit-exact copy. Now skipped when the grade is identity, no
   LUT, no effect and no scope is open.
2. **GPU composite for full-frame layers.** An opaque full-frame layer
   replaced everything below it but was still blended on the GPU (two
   uploads + a readback). Opaque layers now replace; transparent overlays
   blend on the CPU with fast paths (transparent pixels skipped, opaque
   copied).
3. **Rendering for no one.** With no stream/recording and no visible
   monitor (window minimised, other page), the program picture is not
   rendered. A request renders on demand.
4. **Monitor downscale every tick while streaming.** Now done only when the
   UI asks, once per new frame.
5. **Monitor resolution.** Follows the drawn size (640×360 / 960×540 /
   1280×720) instead of a fixed 960×540: render, JPEG and transfer scale with it.
6. **Monitor transfer.** Raw JPEG bytes over binary IPC instead of base64
   text inside JSON (≈ 33 % smaller, no string decoding); blob URLs are
   revoked so they never accumulate.
7. **Unseen captures.** A camera/screen not shown on Preview, Program, a
   running transition or the Color page reads and drops packets (the device
   never backs up) but decodes nothing; it resumes within one frame
   (measured 61 ms in a test).

## Background loops

| Loop | Rate | Note |
|---|---|---|
| Media tick | output fps | Does no video work when nothing needs a picture. |
| UI event pump | 15 Hz | Drains engine events only. |
| System metrics | 1 Hz | Own thread (PDH can be slow); never blocks the tick. |
| Program monitor poll | 30 Hz, 15 Hz under load | Only while the Production page is visible and the window is not hidden; "unchanged" replies carry 0 bytes. |
| Meters | 25 Hz | Only while a meter is on screen. |
| Diagnostics | 1 Hz | |
| Full state refresh | 0.2 Hz (1 Hz while recording) | Was 0.5 Hz; changes arrive as events. |

## Adaptive behaviour

* Monitor polling drops to 15 fps when the engine misses frames (preview
  smoothness gives way before the stream).
* Monitor resolution follows its drawn size.
* Unseen captures stop decoding.
* Stream resolution and bitrate are never lowered automatically.

## Not done (and why)

* **FFmpeg MJPEG for the monitor.** Measured slower (20.5 ms vs 14.6 ms per
  960×540 frame) than the current encoder on the test machine, so not used.
* **Zero-copy GPU path end to end** (keeping frames on the GPU from capture
  to encoder). Large rewrite; the bigger wins above came first.
* **VRAM measurement.** Not available from the cross-platform GPU layer;
  not measured.

## Before / after (measured)

Machine: 2-core Xeon VM, **software GPU (lavapipe)**, so GPU work shows up as
CPU here. Tool: `apps/desktop/backend/tests/bench_longrun.rs`, which mirrors
the app's threads (tick thread + UI thread sharing the engine lock),
release build, 5 min per run. "Before" = v0.1.27 code (git `82b62b2`), same
benchmark. CPU = % of one core, this process.

| Scenario | CPU | Engine time / frame | Late frames / min | RAM (start→end) |
|---|---:|---:|---:|---:|
| Idle (test signal), before | 101.6 % | 16.20 ms | 4.6 | 159 → 161 MB |
| Idle, after, same 960×540 monitor | 51.9 % | 0.93 ms | 0.8 | 144 → 144 MB |
| Idle, after, monitor drawn ≈590 px wide (→640×360) | 23.5 % | 0.42 ms | 0.6 | 135 → 136 MB |
| Busy (720p file + PiP + logo + text + preview scene), before | 124.9 % | 29.16 ms | 701.3 | 224 → 228 MB |
| Busy, after, 960×540 monitor | 102.4 % | 13.96 ms | 14.4 | 214 → 214 MB |
| Busy, after, 640×360 monitor | 58.0 % | 8.82 ms | 3.2 | 207 → 207 MB |

10-minute runs (earlier, single-threaded benchmark) showed RAM flat both
before and after: no leak found; the work was CPU per frame, not memory.

Not measured: GPU %, VRAM, audio latency and A/V sync under these loads
(this VM has no real GPU or audio device); behaviour on Windows hardware.
