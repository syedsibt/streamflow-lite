/**
 * TypeScript mirrors of the Rust IPC shapes (`sf-ipc`, `sf-ui-backend`).
 * Field names are snake_case to match the Rust serde exactly.
 */

// ---------------------------------------------------------------------------
// Color grade (sf-color::ColorGrade, UI units)
// ---------------------------------------------------------------------------

export type Vec4 = [number, number, number, number];

export interface Curve {
  points: [number, number][];
}

export interface CurveSet {
  master: Curve;
  r: Curve;
  g: Curve;
  b: Curve;
  luma: Curve;
  hue_vs_sat: Curve;
}

export type LutPosition = "Before" | "After";

export interface LutRef {
  file: string;
  intensity: number; // 0..1
  position: LutPosition;
}

export interface ColorGrade {
  exposure: number; // -3..3 EV
  brightness: number; // -100..100
  contrast: number; // -100..100
  saturation: number; // 0..200 %
  vibrance: number; // -100..100
  temperature: number; // 2500..10000 K
  tint: number; // -100..100
  highlights: number; // -100..100
  shadows: number; // -100..100
  whites: number; // -100..100
  blacks: number; // -100..100
  gamma: number; // 0.5..2.5
  sharpness: number; // 0..100
  lift: Vec4;
  gamma_wheel: Vec4;
  gain: Vec4;
  offset: Vec4;
  curves: CurveSet;
  lut: LutRef | null;
  skin_protect: boolean;
  faded_film: number; // 0..100
  skin_warmth: number; // -100..100 (skin only)
  skin_saturation: number; // -100..100 (skin only)
  skin_brightness: number; // -100..100 (skin only)
}

/** A LUT in the user's library (imported .cube, copied into app data). */
export interface LutInfo {
  id: string;
  name: string;
  path: string;
  warnings: string[];
}

/** A saved color preset (grade + filter stack). */
export interface ColorPresetInfo {
  id: string;
  name: string;
}

/** Color Studio preview comparison mode (preview only — never the stream). */
export type PreviewMode = "after" | "before" | "split";

export const CURVE_NAMES = [
  "master",
  "r",
  "g",
  "b",
  "luma",
  "hue_vs_sat",
] as const;
export type CurveName = (typeof CURVE_NAMES)[number];

// ---------------------------------------------------------------------------
// Filters (sf-filters)
// ---------------------------------------------------------------------------

/** Where a filter-stack command applies. Externally-tagged like the Rust. */
export type FilterTarget = { input: string } | { scene: string } | "program";

export type EffectKindName =
  | "Sharpen"
  | "GaussianBlur"
  | "BackgroundBlur"
  | "BackgroundReplace"
  | "ChromaKey"
  | "Vignette"
  | "FilmGrain"
  | "Bloom"
  | "NoiseReduction"
  | "SkinSmooth"
  | "MirrorFlip"
  | "CropZoomPan"
  | "LensCorrection"
  | "Letterbox"
  | "ChromaticAberration"
  | "Pixelate";

/** Externally-tagged Filter enum. */
export type Filter =
  | { Look: { preset_id: string; intensity: number } }
  | { Effect: { kind: EffectKindName; params: Record<string, unknown> } };

export type GpuCost = "low" | "medium" | "high";

export interface SlotView {
  id: string; // UUID
  label: string;
  kind: string; // "look" | lowercase effect kind
  enabled: boolean;
  intensity: number; // 0..1
  cost: GpuCost;
  is_ai: boolean;
  /** Current effect parameters (effects only). */
  params?: EffectParams | null;
  /** Look preset id (look slots only). */
  look_id?: string | null;
}

/** sf_filters::EffectParams — sent whole by `filter_set_params`. */
export interface EffectParams {
  amount: number;
  radius: number;
  softness: number;
  center: [number, number];
  color: [number, number, number];
  threshold: number;
  zoom: number;
  aspect: number;
  k1: number;
  k2: number;
  axis: string;
  spill: number;
  tolerance: number;
  key_softness: number;
}

export interface EffectKindInfo {
  kind: string;
  label: string;
  cost: GpuCost;
  is_ai: boolean;
  default_params: Record<string, unknown>;
}

export interface LookInfo {
  id: string;
  name: string;
  category: string;
}

// ---------------------------------------------------------------------------
// Scopes (sf-scopes, UI projection from sf-ui-backend::ScopeDataView)
// ---------------------------------------------------------------------------

export type ScopeKindName =
  | "waveform"
  | "vectorscope"
  | "histogram"
  | "false_color"
  | "zebra";

export type ScopeDataView =
  | {
      kind: "waveform";
      luma_min: number[];
      luma_max: number[];
      r_min: number[];
      r_max: number[];
      g_min: number[];
      g_max: number[];
      b_min: number[];
      b_max: number[];
    }
  | { kind: "vectorscope"; bins: number[] } // 128x128 row-major
  | {
      kind: "histogram";
      r: number[];
      g: number[];
      b: number[];
      luma: number[];
    }
  | { kind: "false_color"; width: number; height: number; rgb: number[] }
  | { kind: "zebra"; width: number; height: number; rgba: number[] };

// ---------------------------------------------------------------------------
// UI events (spec §9, via the single "sf-event" Tauri channel)
// ---------------------------------------------------------------------------

export type UiEvent =
  | { event: "color.changed"; input_id: string }
  | { event: "filter.stack_changed"; target: FilterTarget }
  | {
      event: "scopes.frame";
      input_id: string;
      seq: number;
      kind: ScopeKindName;
      data: ScopeDataView;
    }
  | { event: "color.match_done"; input_id: string }
  | { event: "gpu.cost_warning"; message: string }
  | { event: "color.drift_suggest"; input_id: string; message: string }
  // Production events (the UI refreshes production_state() on these).
  | { event: "production.scene_changed" }
  | { event: "production.source_changed" }
  | { event: "production.transition_progress" }
  | { event: "production.transition_done" }
  | { event: "production.stream_state" }
  | { event: "production.record_state" }
  | { event: "production.mixer_changed" };

export interface AiResultView {
  grade: ColorGrade;
  explanation: string[];
}

// ---------------------------------------------------------------------------
// Parameter metadata for the Basic tab (spec §4.1 ranges)
// ---------------------------------------------------------------------------

export interface ParamDef {
  param: string;
  label: string;
  min: number;
  max: number;
  step: number;
  /** Value the reset button returns to. */
  neutral?: number;
  format: (v: number) => string;
}


const f0 = (v: number) => v.toFixed(0);

const signed0 = (v: number) => `${v >= 0 ? "+" : ""}${f0(v)}`;

/** Basic correction, in grading order: white balance → tone → color. */
export const BASIC_GROUPS: { title: string; params: ParamDef[] }[] = [
  {
    title: "White balance",
    params: [
      { param: "temperature", label: "Temperature", min: 2500, max: 10000, step: 50, neutral: 6500, format: (v) => `${f0(v)} K` },
      { param: "tint", label: "Tint", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
    ],
  },
  {
    title: "Tone",
    params: [
      { param: "exposure", label: "Exposure", min: -3, max: 3, step: 0.05, neutral: 0, format: (v) => `${v >= 0 ? "+" : ""}${v.toFixed(2)} EV` },
      { param: "brightness", label: "Brightness", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
      { param: "contrast", label: "Contrast", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
      { param: "highlights", label: "Highlights", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
      { param: "shadows", label: "Shadows", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
      { param: "whites", label: "Whites", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
      { param: "blacks", label: "Blacks", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
    ],
  },
  {
    title: "Color",
    params: [
      { param: "saturation", label: "Saturation", min: 0, max: 200, step: 1, neutral: 100, format: (v) => `${f0(v)}%` },
      { param: "vibrance", label: "Vibrance", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
    ],
  },
];

/** Flat list (kept for callers that iterate every basic control). */
export const BASIC_PARAMS: ParamDef[] = BASIC_GROUPS.flatMap((g) => g.params);

/** Creative section (Lumetri-style). Look + intensity live in the looks strip. */
export const CREATIVE_PARAMS: ParamDef[] = [
  { param: "faded_film", label: "Faded Film", min: 0, max: 100, step: 1, neutral: 0, format: f0 },
  { param: "sharpness", label: "Sharpen", min: 0, max: 100, step: 1, neutral: 0, format: f0 },
  { param: "vibrance", label: "Vibrance", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
  { param: "saturation", label: "Saturation", min: 0, max: 200, step: 1, neutral: 100, format: (v) => `${f0(v)}%` },
  { param: "gamma", label: "Gamma", min: 0.5, max: 2.5, step: 0.01, neutral: 1, format: (v) => v.toFixed(2) },
];

/** Skin / portrait (applied through the skin mask only — natural ranges). */
export const SKIN_PARAMS: ParamDef[] = [
  { param: "skin_warmth", label: "Skin warmth", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
  { param: "skin_saturation", label: "Skin saturation", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
  { param: "skin_brightness", label: "Skin brightness", min: -100, max: 100, step: 1, neutral: 0, format: signed0 },
];

/** Wheel definitions for the Pro tab: puck (hue/sat) + luma slider each. */
export const WHEELS = [
  { key: "lift", label: "Lift" },
  { key: "gamma_wheel", label: "Gamma" },
  { key: "gain", label: "Gain" },
  { key: "offset", label: "Offset" },
] as const;

// ---------------------------------------------------------------------------
// Production (sf-ui-backend production views). Field names are snake_case to
// match the Rust serde exactly. See `apps/desktop` task contract.
// ---------------------------------------------------------------------------

/** Externally-tagged source kind enum for `source_add`. */
export type SourceKind =
  | { test_pattern: Record<string, never> }
  | { color: { r: number; g: number; b: number } }
  | { camera: { device?: string | null; width?: number; height?: number; fps?: number; audio_device?: string | null } }
  | { screen: Record<string, never> }
  | { media: { path: string } }
  | { text: { text: string } }
  | { audio_input: { device?: string | null } };

/** Camera source summary for the Video panel — mirrors the backend. */
export interface CameraSourceInfo {
  id: string;
  name: string;
  device: string | null;
  width: number;
  height: number;
  fps: number;
  negotiated: string | null;
}

/** Video input/output settings (Video menu panel) — mirrors the backend. */
export interface VideoSettings {
  output_width: number;
  output_height: number;
  output_fps: number;
  encoder: string;
  bitrate_kbps: number;
  camera_device: string | null;
  camera_width: number;
  camera_height: number;
  camera_fps: number;
}

export interface SourceView {
  id: string;
  name: string;
  kind: string;
  visible: boolean;
  order: number;
  status: string;
  /** Engine input id for real kinds (`src-<uuid>`); null for pending kinds. */
  input_id: string | null;
}

export interface SceneView {
  id: string;
  name: string;
  sources: SourceView[];
}

export interface TransitionView {
  active: boolean;
  kind: string;
  progress: number; // 0..1
}

export interface MixerChannelView {
  source_id: string;
  name: string;
  gain: number; // 0..2.0 multiplier, shown as 0..200%
  mute: boolean;
}

export interface EnhancerSettings {
  enabled: boolean;
  gate_enabled: boolean;
  gate_threshold_db: number;
  eq_low_db: number;
  eq_mid_db: number;
  eq_high_db: number;
  compressor_enabled: boolean;
  /** Real-time AI noise suppression (RNNoise, on-device). */
  ai_denoise: boolean;
  /** AI suppression strength 0..1. */
  ai_strength: number;
}

/** Live per-voice-source telemetry (mics / camera audio). */
export interface VoiceStats {
  vad: number; // voice-activity probability 0..1
  gain_reduction_db: number;
  gate_closed: boolean;
  latency_ms: number;
  process_us: number;
  buffered_ms: number;
  underruns: number;
  dropped_frames: number;
  device: string; // "starting" | "running" | "error: …" | "stopped"
}

export interface MixerView {
  channels: MixerChannelView[];
  master: { gain: number; mute: boolean };
  meters_live: boolean;
  meters: Record<string, number>; // per-source peak, keyed by source id
  monitor_enabled: boolean;
  enhancer: EnhancerSettings;
  voice: Record<string, VoiceStats>;
}

export interface StreamStateView {
  state: string; // "idle" | "connecting" | "live" | "error" | ...
  message?: string | null; // set when state === "error"
  settings: { url: string; platform: string }; // NO key — never echoed back
}

export interface RecordStateView {
  state: string; // "idle" | "recording" | "paused" | "error" | ...
  elapsed_secs: number;
  message?: string | null; // set when state === "error"
}

/**
 * Macro action as the backend expects it: internally-tagged
 * (`#[serde(tag = "action")]`), e.g. `{ action: "cut" }` or
 * `{ action: "auto_transition", duration_ms: 500 }`.
 */
export type MacroActionInput =
  | { action: "cut" }
  | { action: "auto_transition"; duration_ms: number }
  | { action: "toggle_stream" }
  | { action: "toggle_record" }
  | { action: "apply_look"; look_id: string; intensity: number };

export interface MacroView {
  id: string;
  name: string;
  hotkey: string;
  action: string;
}

/** Full `production_state()` response. */
export interface ProductionStateData {
  scenes: SceneView[];
  preview_scene_id: string | null;
  program_scene_id: string | null;
  transition: TransitionView;
  mixer: MixerView;
  stream: StreamStateView;
  record: RecordStateView;
  macros: MacroView[];
  video_settings: VideoSettings;
}

/** Source kinds whose capture backend is not implemented yet. */
/**
 * Source kinds with no capture backend on ANY platform (the backend reports
 * `status: "pending"` for these). Camera/media are real backends (DirectShow
 * camera on Windows, media files everywhere) — their pending state comes
 * from the backend's per-platform `status` field, not this list.
 */
export const PENDING_CAPTURE_KINDS = ["text"];

export function isPendingCaptureSource(s: SourceView): boolean {
  const kind = s.kind.toLowerCase();
  const status = (s.status ?? "").toLowerCase();
  return (
    PENDING_CAPTURE_KINDS.some((k) => kind.includes(k)) ||
    status.includes("pending")
  );
}
