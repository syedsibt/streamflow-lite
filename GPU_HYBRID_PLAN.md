# GPU+CPU Hybrid Pipeline Plan

## Goal
Out-class performance: no lag, perfect colors, GPU+CPU working together.
Target: 1080p60 without lag (OBS-level performance).

## Current (CPU-heavy) Pipeline
```
Camera (dshow, CPU)
  → Decode → RGBA (CPU swscale)
  → Resize (CPU resize_rgba_nearest)
  → Upload to GPU
  → Color grade (GPU)
  → Download to CPU
  → Composite (CPU composite_over)
  → Preview PNG encode (CPU)
  → Stream x264 encode (CPU)
```

Problems:
- CPU↔GPU roundtrip per source (expensive)
- CPU compositing (slow for 1080p)
- CPU PNG preview (slow)
- CPU x264 (slow, no hardware accel)

## Target Hybrid Pipeline
```
Camera (dshow, CPU) — must be CPU (capture API)
  → Decode → RGBA (CPU swscale) — must be CPU (FFmpeg)
  → Upload to GPU (ONCE)
  → Scale to composite size (GPU)
  → Color grade (GPU) — already done
  → Effects/filters (GPU) — already done
  → Composite all sources (GPU) — NEW
  → Download to CPU (ONCE)
  → Stream encode (CPU x264, or GPU NVENC) 
  → Record encode (CPU)
  → Preview (GPU texture share, or low-res CPU)
```

Division:
- **GPU (DirectX 12 via wgpu)**: scale, grade, effects, composite
- **CPU**: capture, decode, encode, audio, UI

## Phases

### Phase 1: GPU Compositor (Week 1)
- New `sf-composite` crate or extend `sf-render`
- GPU shader for alpha blending (replace `composite_over`)
- API: `composite(sources: &[GpuTexture]) -> GpuTexture`
- Keep frames on GPU through grade→composite

### Phase 2: Pipeline Integration (Week 2)
- Modify production backend to use GPU compositor
- Eliminate per-source CPU↔GPU roundtrip
- Single download per frame for encode

### Phase 3: Hardware Encode (Week 3)
- NVENC (NVIDIA), QSV (Intel), AMF (AMD) via FFmpeg
- Fallback to CPU x264
- GPU→encoder zero-copy where possible

### Phase 4: Zero-copy Preview (Week 4)
- Share GPU textures with UI (avoid PNG encode)
- Or: low-res CPU preview at 10fps

## Success Criteria
- 1080p60 camera → preview with <100ms latency
- CPU usage <30% during 1080p stream
- Colors match (no GPU/CPU color shift)
