# StreamFlow Lite

Windows-native, GPU-first live production software — **Color Studio + Filters** as the
number-one differentiator. Separate product line from StreamFlow v0.1.0 (C++/Qt/Linux).

- **UI:** Tauri + React (Windows-only shell lives in `apps/desktop/`)
- **Engine:** Rust workspace (`crates/`)
- **GPU:** wgpu — DirectX 12 on Windows, Vulkan for headless dev/CI on Linux
- **Spec:** `~/workspace/user/files/StreamFlow_Lite___Color_Studio___Filters_Architecture.pdf`
  (distilled in `docs/ARCHITECTURE.md`)

## Build order (from the spec)

| Phase | Work | Done when |
|---|---|---|
| C0 | wgpu pipeline + basic fused grade shader (exposure, contrast, saturation, temp) | live slider on webcam, no drops |
| C1 | Wheels (lift/gamma/gain/offset), tone controls, curves bake, per-input state | full Basic + Pro tab |
| C2 | 3D LUT `.cube` import, position before/after, intensity | import matches FFmpeg `lut3d` |
| C3 | Look filters (`.sflook`), 30 built-in looks, intensity, 8-slot stack UI | look browser working |
| C4 | Scopes (waveform, vectorscope, histogram, false color, zebra) | 15 Hz, subscription-gated — zero GPU work with no subscribers |
| C5 | Effect filters (sharpen, blur, vignette, grain, chroma key…) | 8-slot stack stable |
| C6 | AI: auto color, camera match, skin protect | two cameras visibly match |
| C7 | AI background blur/replace, skin smooth, region masks | masks cached, refreshed every 15 frames |
| C8 | Profile + optimize, scope-cadence flake fix, `podcast_warm.cube` ship, preset packs, polish | full suite green, warning-free, fmt-clean |

## Dev quickstart (Linux headless)

```sh
# Rust toolchain (once)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
export PATH="$HOME/.cargo/bin:$PATH"

# Build + test (headless GPU via lavapipe/Vulkan)
cargo test --workspace
```

The Windows `.exe`/installer is produced on a Windows runner (GitHub Actions);
this repo's engine code is backend-agnostic wgpu, so the same code targets DX12 there.

## Rules (from the spec, non-negotiable)

- Color processing 100% on GPU, one fused shader pass per input.
- Identity grade (no changes) → pass skipped entirely.
- No frame drops while dragging any slider live.
- Every filter has an intensity slider (0–100%) and one-click bypass.
- All AI/color features apply as **editable parameters**, never black-box.
