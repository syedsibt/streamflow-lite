/**
 * Typed Tauri IPC client.
 *
 * Command argument names are snake_case and must match the Rust signatures in
 * `apps/desktop/src-tauri/src/main.rs` exactly. Outside Tauri (plain browser
 * dev) every call rejects and event listening is a no-op — the UI shows its
 * placeholder states.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AiResultView,
  ColorGrade,
  EffectKindInfo,
  EnhancerSettings,
  Filter,
  FilterTarget,
  LookInfo,
  LutInfo,
  ColorPresetInfo,
  EffectParams,
  PreviewMode,
  SceneView,
  SlotView,
  SourceKind,
  SourceView,
  UiEvent,
  VideoSettings,
  CameraSourceInfo,
  Diagnostics,
  PlatformInfo,
  AvSync,
} from "./types";

declare global {
  interface Window {
    __TAURI_INTERNALS__?: unknown;
  }
}

export const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function cmd<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri) throw new Error(`tauri unavailable (tried ${name})`);
  return invoke<T>(name, args ?? {});
}

// ---------------------------------------------------------------------------
// Commands (spec §9)
// ---------------------------------------------------------------------------

export const api = {
  colorSetParam: (input_id: string, param: string, value: number) =>
    cmd<ColorGrade>("color_set_param", { input_id, param, value }),
  colorResetParam: (input_id: string, param: string) =>
    cmd<ColorGrade>("color_reset_param", { input_id, param }),
  colorSetCurve: (input_id: string, curve: string, points: [number, number][]) =>
    cmd<void>("color_set_curve", { input_id, curve, points }),
  colorLoadLut: (input_id: string, path: string, position: string, intensity: number) =>
    cmd<void>("color_load_lut", { input_id, path, position, intensity }),
  colorReset: (input_id: string, section: string | null) =>
    cmd<ColorGrade>("color_reset", { input_id, section }),
  colorAuto: (input_id: string) => cmd<AiResultView>("color_auto", { input_id }),
  colorMatch: (reference_id: string, target_id: string) =>
    cmd<AiResultView>("color_match", { reference_id, target_id }),
  gradeSnapshot: (input_id: string) =>
    cmd<ColorGrade>("grade_snapshot", { input_id }),
  /** Applies a full grade snapshot (A/B slots, before/after compare). */
  applyGrade: (input_id: string, grade: ColorGrade) =>
    cmd<ColorGrade>("apply_grade", { input_id, grade }),

  filterAdd: (target: FilterTarget, filter: Filter) =>
    cmd<string>("filter_add", { target, filter }),
  filterReorder: (target: FilterTarget, order: string[]) =>
    cmd<void>("filter_reorder", { target, order }),
  filterSetIntensity: (target: FilterTarget, slot_id: string, intensity: number) =>
    cmd<void>("filter_set_intensity", { target, slot_id, intensity }),
  filterToggle: (target: FilterTarget, slot_id: string, enabled: boolean) =>
    cmd<void>("filter_toggle", { target, slot_id, enabled }),
  filterRemove: (target: FilterTarget, slot_id: string) =>
    cmd<void>("filter_remove", { target, slot_id }),
  lookApply: (target: FilterTarget, look_id: string, intensity: number) =>
    cmd<string>("look_apply", { target, look_id, intensity }),
  stackSnapshot: (target: FilterTarget) =>
    cmd<SlotView[]>("stack_snapshot", { target }),
  listLooks: () => cmd<LookInfo[]>("list_looks"),
  listEffectKinds: () => cmd<EffectKindInfo[]>("list_effect_kinds"),

  scopesSubscribe: (input_id: string, kind: string) =>
    cmd<void>("scopes_subscribe", { input_id, kind }),
  scopesUnsubscribe: (input_id: string, kind: string) =>
    cmd<void>("scopes_unsubscribe", { input_id, kind }),

  /** JPEG data URL of the live processed frame (same as program/encoder). */
  previewGetFrame: (input_id: string, width: number, height: number, mode: PreviewMode = "after") =>
    cmd<string>("preview_get_frame", { input_id, width, height, mode }),
  lookClear: (target: FilterTarget) => cmd<void>("look_clear", { target }),
  filterSetParams: (target: FilterTarget, slot_id: string, params: EffectParams) =>
    cmd<void>("filter_set_params", { target, slot_id, params }),
  lutImport: (path: string) => cmd<LutInfo>("lut_import", { path }),
  lutLibrary: () => cmd<LutInfo[]>("lut_library"),
  lutRemove: (id: string) => cmd<void>("lut_remove", { id }),
  colorPresetSave: (input_id: string, name: string) =>
    cmd<ColorPresetInfo>("color_preset_save", { input_id, name }),
  colorPresetList: () => cmd<ColorPresetInfo[]>("color_preset_list"),
  colorPresetApply: (input_id: string, id: string) =>
    cmd<ColorGrade>("color_preset_apply", { input_id, id }),
  colorPresetDelete: (id: string) => cmd<void>("color_preset_delete", { id }),

  // -------------------------------------------------------------------------
  // Production (command names and snake_case args must match the Rust
  // signatures in `apps/desktop/src-tauri/src/main.rs` exactly)
  // -------------------------------------------------------------------------

  sceneList: () => cmd<SceneView[]>("scene_list"),

  sourceAdd: (scene_id: string, kind: SourceKind, name: string) => {
    // The backend takes flat args (kind as a snake_case string plus extras);
    // translate the tagged union here so components keep the nicer shape.
    // Unknown shapes fall back to test_pattern rather than failing.
    let kindStr = "test_pattern";
    let color_r = 0;
    let color_g = 0;
    let color_b = 0;
    let text: string | null = null;
    let path: string | null = null;
    let device: string | null = null;
    let audio_device: string | null = null;
    let width = 0;
    let height = 0;
    let fps = 0;
    if ("color" in kind) {
      kindStr = "color";
      color_r = kind.color.r;
      color_g = kind.color.g;
      color_b = kind.color.b;
    } else if ("camera" in kind) {
      kindStr = "camera";
      device = kind.camera.device ?? null;
      width = kind.camera.width ?? 0;
      height = kind.camera.height ?? 0;
      fps = kind.camera.fps ?? 0;
      audio_device = kind.camera.audio_device ?? null;
    } else if ("screen" in kind) {
      kindStr = "screen";
    } else if ("media" in kind) {
      kindStr = "media";
      path = kind.media.path;
    } else if ("text" in kind) {
      kindStr = "text";
      text = kind.text.text;
    } else if ("audio_input" in kind) {
      kindStr = "audio_input";
      device = kind.audio_input.device ?? null;
    }
    return cmd<SourceView>("source_add", {
      scene_id,
      kind: kindStr,
      name,
      color_r,
      color_g,
      color_b,
      text,
      path,
      device,
      audio_device,
      width,
      height,
      fps,
    });
  },
  listCameras: () => cmd<string[]>("list_cameras", {}),
  cameraFormat: (source_id: string) =>
    cmd<string | null>("camera_format", { source_id }),
  cameraReconfigure: (args: {
    source_id: string;
    device: string | null;
    width: number;
    height: number;
    fps: number;
    audio_device: string | null;
  }) => cmd<string>("camera_reconfigure", args),
  cameraSources: () => cmd<CameraSourceInfo[]>("camera_sources", {}),
  videoSettingsGet: () => cmd<VideoSettings>("video_settings_get", {}),
  videoSettingsSet: (settings: VideoSettings) =>
    cmd<void>("video_settings_set", { settings }),
  listAudioDevices: () => cmd<string[]>("list_audio_devices", {}),
  /** Measured pipeline diagnostics (stages, fps, drops, latency, CPU/RAM). */
  diagnostics: () => cmd<Diagnostics>("diagnostics", {}),
  sourceRemove: (scene_id: string, source_id: string) =>
    cmd<void>("source_remove", { scene_id, source_id }),
  sourceReorder: (scene_id: string, order: string[]) =>
    cmd<void>("source_reorder", { scene_id, order }),
  sourceSetVisible: (scene_id: string, source_id: string, visible: boolean) =>
    cmd<void>("source_set_visible", { scene_id, source_id, visible }),

  mediaSetPaused: (source_id: string, paused: boolean) =>
    cmd<boolean>("media_set_paused", { source_id, paused }),

  productionState: () => cmd<unknown>("production_state"),
  /** Program monitor frame only when newer than `after_seq`. */
  monitorFrameIfNew: (scene_id: string, after_seq: number) =>
    cmd<[number, string] | null>("monitor_frame_if_new", { scene_id, after_seq }),

  mixerSetGain: (source_id: string, gain: number) =>
    cmd<void>("mixer_set_gain", { source_id, gain }),
  mixerSetMute: (source_id: string, mute: boolean) =>
    cmd<void>("mixer_set_mute", { source_id, mute }),
  mixerSetMasterGain: (gain: number) =>
    cmd<void>("mixer_set_master_gain", { gain }),
  mixerSetMasterMute: (mute: boolean) =>
    cmd<void>("mixer_set_master_mute", { mute }),
  setMonitorEnabled: (enabled: boolean) =>
    cmd<boolean>("set_monitor_enabled", { enabled }),
  setEnhancerSettings: (settings: EnhancerSettings) =>
    cmd<EnhancerSettings>("set_enhancer_settings", { settings }),
  sourceAudioDevice: (source_id: string) =>
    cmd<string | null>("source_audio_device", { source_id }),
  avSyncSet: (auto: boolean, offset_ms: number) =>
    cmd<AvSync>("av_sync_set", { auto, offset_ms }),
  streamPlatforms: () => cmd<PlatformInfo[]>("stream_platforms", {}),

  streamSetSettings: (url: string, key: string, platform: string) =>
    cmd<void>("stream_set_settings", { url, key, platform }),
  streamStart: () => cmd<void>("stream_start"),
  streamStop: () => cmd<void>("stream_stop"),

  recordSetSettings: (path: string, format: string, quality: string) =>
    cmd<void>("record_set_settings", { path, format, quality }),
  recordStart: () => cmd<void>("record_start"),
  recordStop: () => cmd<void>("record_stop"),
  recordPause: () => cmd<void>("record_pause"),
  recordResume: () => cmd<void>("record_resume"),

};

/** Subscribe to the single "sf-event" channel; routes on the spec event tag. */
export async function onSfEvent(cb: (e: UiEvent) => void): Promise<() => void> {
  if (!isTauri) return () => {};
  const unlisten: UnlistenFn = await listen<UiEvent>("sf-event", (ev) => cb(ev.payload));
  return unlisten;
}
