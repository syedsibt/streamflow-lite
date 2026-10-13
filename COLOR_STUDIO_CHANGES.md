# Color Studio: professional, lightweight, real-time

## What's new for the user
* **Basic** (white balance → tone → color): Temperature, Tint, Exposure,
  Brightness, Contrast, Highlights, Shadows, Whites, Blacks, Saturation,
  Vibrance. Every control has a slider, an exact numeric box, a ↺ reset, and
  fine adjustment (hold **Shift**).
* **Creative**: active look + intensity, Faded Film (new), Sharpen,
  Vibrance, Saturation, Gamma.
* **Curves**: RGB/master, R, G, B, Luma (+ Hue vs Sat) — now behave like
  every editor: the 0.5 point is on-screen mid-grey.
* **Wheels**: Shadows / Midtones / Highlights (+ Global) color wheels with hue,
  saturation and luminance; per-channel values under "Advanced".
* **Filters**: 51 one-click filters; new families **Cinematic, Portrait,
  Creative, Social** (Cinematic, Film, Movie, Dramatic, Skin, Warm Skin, Clean
  Skin, Soft/Natural Portrait, Vintage, Retro, Cold, Warm, Dreamy, Fade, Glow,
  Clean, Vibrant, Bright, High Contrast, Soft …). One active filter with a
  live 0–100 % intensity; 0 % = no effect, 100 % = full, proportional between.
* **LUT**: Import `.cube` (file picker) → validated → saved to your LUT library
  → click to preview live → intensity 0–100 % → Input (before grade) or
  Creative (after grade). Clear errors for `.3dl`, `.look`, `.csp`, broken
  files.
* **Glow**: on/off, Intensity, Threshold, Softness, Radius (GPU).
* **Skin / Portrait**: one-knob natural Intensity, plus skin warmth,
  saturation, brightness, smoothness — applied only inside skin tones.
* **Presets**: save / apply / delete complete looks (basic, curves, wheels,
  LUT + intensity, filter + intensity, glow, skin).
* **Before / Split / After** — preview only; the stream always keeps your grade.
* The Color Studio preview is now the **same processed frame** that goes to the
  main preview and the encoder (~10 fps, JPEG), not a separate pipeline.

## Bugs fixed (found while building this)
1. **LUTs were applied to linear-light values.** `.cube` creative LUTs
   (Premiere/Lumetri, Resolve) expect display-referred input. Measured on a
   real photo with a test LUT: old engine was 17.7/255 off on average, new
   engine 0.00 vs. ground truth.
2. **`.cube` DOMAIN_MIN/MAX rescaled the output** instead of mapping the input
   domain; Resolve `LUT_3D_INPUT_RANGE` and 1D-shaper + 3D files were not
   handled.
3. **A bad or missing LUT failed the whole program frame** (live video stopped).
   Now it is rejected on load with a clear message and, if it breaks later,
   skipped while the rest of the grade continues.
4. **"Before" LUT position never worked** (UI sent "Before", backend matched
   "before").
5. **Editing any curve scrambled hues** — `rgb_to_hsv` lacked the `/6` per
   sector (orange → green, skin → cyan); the old test only checked primaries.
6. **Default hue-vs-sat curve multiplied saturation by hue** as soon as any
   other curve was edited.
7. **Look intensity didn't scale curves** (30 % look applied 100 % of its curve).
8. **Clicking looks stacked them** (compounding) and failed on the 3rd click
   (2-look limit). Now one active look, swapped in place.
9. **Before/After applied a neutral grade to the live input** — the stream went
   ungraded while comparing. Now preview-only.
10. **Color Studio preview pulled camera frames itself** (stealing them from the
    program feed when idle) and graded them a second time, at 2 fps PNG.
11. **Glow radius did nothing beyond ±4 px**; hard threshold. Now real radius,
    soft knee, smooth at large radii.
12. One malformed `.sflook` file hid every look.

## Performance notes
* Grade stays one fused compute pass; new steps (skin, faded film, display
  encode for LUT/curves) only run when used.
* Glow: 25 taps × 2 passes, only when enabled.
* Preview: zero extra GPU work (frame moved from the tick); downscale under the
  lock, JPEG encode outside it; requests never queue up.

## Verified here
* `sf-color` 45, `sf-filters` 28, `sf-state` 3, `sf-ipc` 6 tests pass
  (incl. new LUT, curves, HSV, skin, faded film, intensity tests).
* All 28 WGSL shaders validate with wgpu 22's naga.
* CPU reference (the shaders' twin) rendered on a real photo for every
  filter, LUT, wheels, skin, faded film and glow — see the contact sheets.
* Frontend type-checks.

## Not verified here
The GPU crates (`sf-render`, `sf-engine`, `sf-filterfx`) and the desktop backend
could not be compiled in this environment (wgpu/ffmpeg/cpal not reachable).
Run on Windows: `cargo test --workspace` and `cargo tauri dev`. New/updated
tests to watch: `sf-engine` `bad_lut_is_skipped_not_fatal`, `sf-filterfx`
`bloom_agrees_with_cpu`, `sf-render` `grade_gpu`, backend `shell.rs`
(LUT library, one active look, presets, preview modes).
