# StreamFlow Lite — Desktop Shell (`apps/desktop`)

Tauri v2 + React + Vite + TypeScript UI shell for the StreamFlow Lite engine.
Windows-only product; the `.exe`/installer is produced on a Windows runner
(GitHub Actions) — this directory scaffolds and verifies compilation only
(no display on headless dev machines).

## Layout

| Path | What |
|---|---|
| `src/` | React+TS frontend: Color Studio screen (spec §8) + Production view (scenes/sources, preview/program, transitions, mixer, stream/record, multiview, macros) |
| `src/lib/types.ts` | TS mirrors of the Rust IPC shapes (snake_case) |
| `src/lib/ipc.ts` | Typed Tauri `invoke` wrappers + the single `"sf-event"` listener |
| `src/lib/store.ts` | Zustand studio state (60 Hz slider throttle, double-click reset) |
| `src/lib/productionStore.ts` | Zustand production state; `normalizeProductionState()` reconciles backend serde shapes in one place |
| `src/components/` | Header, PreviewPane, ScopesRow, tabs/*, LooksStrip, Toasts, Slider |
| `src/components/production/` | ScenesPanel, SourcesPanel, Monitors, TransitionBar, MixerPanel, StreamPanel, RecordPanel, MacrosPanel, MultiviewGrid, ProductionView, TopBar |
| `backend/` | **`sf-ui-backend`**: ALL engine-binding logic, NO tauri dependency, fully unit-tested headless |
| `src-tauri/` | Thin Tauri adapter: commands → `Shell` methods, event pump → frontend |

## Architecture decision

`src-tauri` must stay thin. Every command handler delegates to
`sf_ui_backend::Shell`; engine → UI events are polled via
`Shell::drain_events()` on a 66 ms thread and re-emitted as Tauri
`"sf-event"` payloads (the frontend routes on the spec §9 `event` tag).
This keeps the binding layer verifiable without GTK/WebKit.

## Verify

```sh
# Frontend
npm install
npm run build        # tsc --noEmit && vite build

# Backend bindings (headless GPU via lavapipe)
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json
cargo test -p sf-ui-backend

# Tauri adapter (needs GTK/WebKit dev packages on Linux)
cd src-tauri && cargo check
```

## What's real vs. placeholder

- REAL: every command drives the real engine (real wgpu passes). Production
  screens: scene/source CRUD, preview/program buses, cut + mix transitions,
  CPU over-composite of graded source frames, mixer state, macro dispatch.
  AV backends are real: media-file sources decode video+audio
  (`sf-capture`); `record.start` records the program bus to MP4/MKV
  (libx264 + AAC, `sf-encode` + `sf-output`); `stream.start` attempts a real
  RTMP connection with live reconnect/backoff; mixer meters are computed
  from real decoded audio (`sf-audio`) and render live in the MixerPanel.
- HONEST STUBS (never faked): camera/screen/text sources show a pending
  badge (Linux v4l2/x11grab backends are code-complete but need hardware;
  Windows MediaFoundation/DXGI are `cfg(windows)` scaffolds); unreachable
  RTMP servers fail cleanly into the Error lifecycle (key always redacted).
- STAND-IN: input pixels for test-pattern/color sources come from a
  deterministic synthetic test chart (`Shell::synthetic_frame`); previews
  render through the real `grade_input_frame` pipeline as PNG (~10 Hz).
  The shell records/streams the program bus at 640×360@15 fps from the
  66 ms poll loop (production moves to a dedicated thread at 1080p60).
- PLACEHOLDER: Tauri dialog plugin (LUT file picker), drag-and-drop LUT
  import, scene-level filter stacks in the composite, zero-copy preview
  path, Windows installer/CI.
