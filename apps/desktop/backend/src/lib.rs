//! `sf-ui-backend` — headless-testable engine bindings for the StreamFlow Lite
//! Tauri + React shell (`apps/desktop`).
//!
//! # Architecture decision (UI-shell brief)
//!
//! ALL engine-binding logic lives in this plain Rust crate, which has **no**
//! `tauri` dependency. `apps/desktop/src-tauri` is a thin adapter that maps
//! Tauri commands to [`Shell`] methods and forwards [`UiEvent`]s to the
//! frontend. This keeps the entire binding layer unit-testable headless —
//! the app window cannot run on a headless dev/CI machine — while the
//! adapter stays small enough to verify by inspection.
//!
//! The UI process only ever sends parameters; the engine applies everything
//! on the GPU (spec §2 process model). The command/event names mirror the
//! spec §9 IPC contract; [`UiEvent`] is the JSON shape the frontend
//! subscribes to.
//!
//! # What is real vs. stand-in (UI shell scope)
//!
//! - REAL: every command handler below drives the real engine (real wgpu
//!   grade/filter/scope passes on lavapipe headless, DX12 on Windows).
//! - STAND-IN: [`Shell::synthetic_frame`] generates the input pixels until
//!   `sf-capture` lands — the preview path is otherwise the real
//!   `grade_input_frame` pipeline, PNG-encoded.
//! - The ONNX model path is still the `StubSession` (C6 decision); AI color
//!   features use the tested hue-based fallbacks.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sf_ai_color::{auto::auto_color, camera_match::camera_match, stats::compute_stats};
use sf_color::ColorGrade;
use sf_engine::{Engine, EngineError, Frame};
use sf_filters::{EffectKind, Filter, FilterSlot, FilterStack, GpuCost, LookRegistry, StackError};
use sf_ipc::FilterTarget;
use sf_scopes::{ScopeFrame, ScopeKind, ScopePayload};
use thiserror::Error;
use uuid::Uuid;

/// Re-exported for UI/test convenience.
pub use sf_color::{LutPosition, LutRef};

/// Real capture/encode/output/audio backend wiring (record, stream, meters,
/// media sources). Keeps `production.rs` focused on the state machines.
mod av_backends;
mod production;
/// Production state machine (scenes, sources, transitions, mixer, stream,
/// record, macros) — all headless-testable, no tauri dependency.
pub use production::{
    transition_progress, ChannelStrip, Macro, MacroAction, MacroDispatchResult, MixerView,
    ProdScene, ProductionStateView, RecordFormat, RecordLifecycle, RecordSettings, SourceKind,
    SourceState, StreamLifecycle, StreamPlatform, StreamSettings, TransitionKind, TransitionView,
};

/// How often `preview_png` may re-render per input (spec has no preview rule;
/// 10 Hz keeps the shell light — production will use a faster zero-copy path).
const PREVIEW_THROTTLE: Duration = Duration::from_millis(100);

/// Stack length above which the UI gets a "GPU cost high" warning (spec §5.3).
const STACK_WARN_LEN: usize = 6;

/// Errors from the shell binding layer.
#[derive(Debug, Error)]
pub enum ShellError {
    /// Unknown input id.
    #[error("unknown input: {0}")]
    UnknownInput(String),
    /// Unknown grade parameter name.
    #[error("unknown param: {0}")]
    UnknownParam(String),
    /// Unknown curve name.
    #[error("unknown curve: {0}")]
    UnknownCurve(String),
    /// Unknown scope kind name.
    #[error("unknown scope kind: {0}")]
    UnknownScopeKind(String),
    /// Unknown look preset id.
    #[error("unknown look: {0}")]
    UnknownLook(String),
    /// Unknown filter target.
    #[error("unknown target: {0}")]
    UnknownTarget(String),
    /// No frame ingested for this input yet.
    #[error("no frame captured for input '{0}' yet")]
    NoFrame(String),
    /// Filter-stack rule violation.
    #[error("stack error: {0}")]
    Stack(#[from] StackError),
    /// Engine failure.
    #[error("engine error: {0}")]
    Engine(#[from] EngineError),
    /// PNG encoding failure.
    #[error("png encode error: {0}")]
    Png(String),
    /// Unknown scene id.
    #[error("unknown scene: {0}")]
    UnknownScene(String),
    /// Unknown source id.
    #[error("unknown source: {0}")]
    UnknownSource(String),
    /// Refusing to delete the last remaining scene.
    #[error("cannot delete the last scene")]
    LastScene,
    /// Unknown source kind name.
    #[error("unknown source kind: {0}")]
    UnknownSourceKind(String),
    /// Unknown stream platform name.
    #[error("unknown stream platform: {0}")]
    UnknownStreamPlatform(String),
    /// Unknown record format name.
    #[error("unknown record format: {0}")]
    UnknownRecordFormat(String),
    /// Unknown transition kind name.
    #[error("unknown transition kind: {0}")]
    UnknownTransitionKind(String),
    /// Transition state error.
    #[error("transition error: {0}")]
    Transition(String),
    /// Streaming error.
    #[error("stream error: {0}")]
    Stream(String),
    /// Recording error.
    #[error("record error: {0}")]
    Record(String),
    /// Macro error.
    #[error("macro error: {0}")]
    Macro(String),
    /// No macro bound to this hotkey.
    #[error("unknown hotkey: {0}")]
    UnknownHotkey(String),
    /// Capture backend error (media file open/decode).
    #[error("capture error: {0}")]
    Capture(String),
}

impl ShellError {
    /// String form for the Tauri adapter (`Result<T, String>`).
    pub fn to_user_string(&self) -> String {
        self.to_string()
    }
}

/// UI-facing event, JSON-serializable for Tauri event emission.
/// Tag names match the spec §9 event names.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum UiEvent {
    /// An input's grade changed (echo after a color command).
    #[serde(rename = "color.changed")]
    ColorChanged {
        /// Target input id.
        input_id: String,
    },
    /// A filter stack changed (add/remove/reorder/intensity/toggle/look).
    #[serde(rename = "filter.stack_changed")]
    FilterStackChanged {
        /// Which stack changed.
        target: FilterTarget,
    },
    /// One scope frame payload (15 Hz while subscribed).
    #[serde(rename = "scopes.frame")]
    ScopesFrame {
        /// Target input id.
        input_id: String,
        /// Monotonic per-subscription sequence number.
        seq: u64,
        /// Scope kind name.
        kind: String,
        /// The computed scope data.
        data: ScopeDataView,
    },
    /// Camera-match finished.
    #[serde(rename = "color.match_done")]
    ColorMatchDone {
        /// Target input id.
        input_id: String,
    },
    /// GPU cost warning (spec §5.3 "GPU cost high").
    #[serde(rename = "gpu.cost_warning")]
    GpuCostWarning {
        /// Human-readable warning for the UI toast.
        message: String,
    },
    /// Drift guard suggestion — the UI offers a re-match, never auto-applies.
    #[serde(rename = "color.drift_suggest")]
    ColorDriftSuggest {
        /// Target input id.
        input_id: String,
        /// Human-readable suggestion for the UI toast.
        message: String,
    },
    /// Production scenes changed (create/rename/delete/duplicate,
    /// preview/program assignment, transition commit).
    #[serde(rename = "production.scene_changed")]
    SceneChanged {
        /// Preview (standby) scene id.
        preview_scene_id: String,
        /// Program (live) scene id.
        program_scene_id: String,
    },
    /// A scene's source list changed (add/remove/reorder/visibility).
    #[serde(rename = "production.source_changed")]
    SourceChanged {
        /// Scene whose sources changed.
        scene_id: String,
    },
    /// Auto-transition progress (one per poll tick while active).
    #[serde(rename = "production.transition_progress")]
    TransitionProgress {
        /// Eased 0.0..=1.0 progress.
        progress: f32,
    },
    /// A transition finished (cut or auto commit).
    #[serde(rename = "production.transition_done")]
    TransitionDone {
        /// Program scene id after the commit.
        program_scene_id: String,
    },
    /// Stream lifecycle changed.
    #[serde(rename = "production.stream_state")]
    StreamState {
        /// New lifecycle.
        lifecycle: StreamLifecycle,
    },
    /// Record lifecycle changed.
    #[serde(rename = "production.record_state")]
    RecordState {
        /// New lifecycle.
        lifecycle: RecordLifecycle,
    },
    /// Mixer gain/mute changed.
    #[serde(rename = "production.mixer_changed")]
    MixerChanged,
}

/// JSON-friendly scope payload for the frontend (the `sf-scopes` types do not
/// derive `Serialize`; this is the UI-facing projection).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScopeDataView {
    /// Waveform: per-column min/max, display-referred 0..=1.
    Waveform {
        luma_min: Vec<f32>,
        luma_max: Vec<f32>,
        r_min: Vec<f32>,
        r_max: Vec<f32>,
        g_min: Vec<f32>,
        g_max: Vec<f32>,
        b_min: Vec<f32>,
        b_max: Vec<f32>,
    },
    /// Vectorscope: 128×128 bin counts, row-major.
    Vectorscope { bins: Vec<u32> },
    /// Histogram: 256 bins per channel.
    Histogram {
        r: Vec<u32>,
        g: Vec<u32>,
        b: Vec<u32>,
        luma: Vec<u32>,
    },
    /// False-color exposure map (RGB).
    FalseColor {
        width: u32,
        height: u32,
        rgb: Vec<u8>,
    },
    /// Zebra overlay (RGBA).
    Zebra {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

impl ScopeDataView {
    fn from_payload(payload: &ScopePayload) -> Self {
        match payload {
            ScopePayload::Waveform(w) => ScopeDataView::Waveform {
                luma_min: w.luma_min.to_vec(),
                luma_max: w.luma_max.to_vec(),
                r_min: w.r_min.to_vec(),
                r_max: w.r_max.to_vec(),
                g_min: w.g_min.to_vec(),
                g_max: w.g_max.to_vec(),
                b_min: w.b_min.to_vec(),
                b_max: w.b_max.to_vec(),
            },
            ScopePayload::Vectorscope(v) => ScopeDataView::Vectorscope {
                bins: v.bins.clone(),
            },
            ScopePayload::Histogram(h) => ScopeDataView::Histogram {
                r: h.r.to_vec(),
                g: h.g.to_vec(),
                b: h.b.to_vec(),
                luma: h.luma.to_vec(),
            },
            ScopePayload::FalseColor(f) => ScopeDataView::FalseColor {
                width: f.width,
                height: f.height,
                rgb: f.rgb.clone(),
            },
            ScopePayload::Zebra(z) => ScopeDataView::Zebra {
                width: z.width,
                height: z.height,
                rgba: z.rgba.clone(),
            },
        }
    }

    fn kind_name(frame: &ScopeFrame) -> &'static str {
        match frame.kind {
            ScopeKind::Waveform => "waveform",
            ScopeKind::Vectorscope => "vectorscope",
            ScopeKind::Histogram => "histogram",
            ScopeKind::FalseColor => "false_color",
            ScopeKind::Zebra => "zebra",
        }
    }
}

/// Serializable look preset summary for the looks strip.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LookInfo {
    /// Registry id (`.sflook` file stem).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Category name (spec §5.1 categories).
    pub category: String,
}

/// Serializable filter-slot summary for the Filters tab stack list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotView {
    /// Stable slot id (used by reorder/intensity/toggle).
    pub id: Uuid,
    /// Display label, e.g. `"Look: Podcast Warm"` or `"Vignette"`.
    pub label: String,
    /// `"look"` or the effect kind name.
    pub kind: String,
    /// Whether the slot is currently applied.
    pub enabled: bool,
    /// Slot strength `0.0..=1.0`.
    pub intensity: f32,
    /// Estimated GPU cost badge: `"low"`, `"medium"`, `"high"`.
    pub cost: String,
    /// True for AI kinds (may grey out without a model).
    pub is_ai: bool,
}

/// The shell binding: owns the engine, look registry, per-input frames,
/// preview cache, and the UI event queue.
///
/// Spec rules implemented here (see also the components that mirror them):
/// - slider drags update the engine immediately; the *frontend* throttles
///   `set_param` invokes to 60 Hz (spec §8);
/// - double-click a slider resets that single parameter
///   ([`Shell::reset_param`], spec §8).
pub struct Shell {
    engine: Engine,
    /// UI event queue, drained by the Tauri adapter's poll loop.
    events: VecDeque<UiEvent>,
    /// Latest raw input frame per input id (stand-in for capture).
    last_frame: HashMap<String, Frame>,
    /// Preview PNG cache per input: (rendered_at, png bytes).
    preview_cache: HashMap<String, (Instant, Vec<u8>)>,
    // --- production state machine (see `production` module) ---
    /// Production scenes.
    scenes: Vec<production::ProdScene>,
    /// Preview (standby) scene id.
    preview_scene_id: String,
    /// Program (live) scene id.
    program_scene_id: String,
    /// In-flight auto transition, if any.
    transition: Option<production::ActiveTransition>,
    /// Audio mixer state (meters honest stub).
    mixer: production::MixerState,
    /// Stream settings (key redacted everywhere).
    stream_settings: production::StreamSettings,
    /// Stream output lifecycle.
    stream_lifecycle: production::StreamLifecycle,
    /// Record settings.
    record_settings: production::RecordSettings,
    /// Active/paused recording runtime (`None` when idle).
    record_runtime: Option<production::RecordRuntime>,
    /// Recording error message (`Some` when lifecycle is `Error`).
    record_error: Option<String>,
    /// Bound macros.
    macros: Vec<production::Macro>,
    // --- real AV backends (see `av_backends` module) ---
    /// Open media-file producers keyed by source id. The `Send` wrapper is
    /// sound: every producer is only touched through the shell's `Mutex`.
    media_producers: HashMap<String, av_backends::SendProducer>,
    /// Last decoded video frame per media source (freeze on end-of-stream).
    media_last_frame: HashMap<String, (u32, u32, Vec<u8>)>,
    /// Real audio mixer (sf-audio); fed by media-source audio chunks.
    audio_mixer: sf_audio::Mixer,
    /// Per-source meter peaks from the last audio tick.
    meter_values: HashMap<String, f32>,
    /// True once real audio has flowed through the mixer.
    meters_live: bool,
    /// Active recording session (`None` when idle).
    active_recording: Option<av_backends::ActiveRecording>,
    /// Active streaming session (`None` when idle).
    active_stream: Option<av_backends::ActiveStream>,
    /// Performance monitoring: frame timestamps for FPS calc.
    perf_frame_times: VecDeque<Instant>,
    /// Last av_tick duration (render time).
    perf_last_tick_us: u64,
    /// Total av_tick time in current second (for CPU% estimate).
    perf_tick_accum_us: u64,
    /// Window start for CPU% calculation.
    perf_window_start: Instant,
}

impl Shell {
    /// Creates the shell: headless engine, a default `cam1` input, and the
    /// built-in looks loaded from `assets/looks/`.
    ///
    /// # Errors
    ///
    /// Returns [`ShellError::Engine`] when no GPU adapter/device opens.
    pub fn new() -> Result<Self, ShellError> {
        let mut shell = Self {
            engine: Engine::new_headless()?,
            events: VecDeque::new(),
            last_frame: HashMap::new(),
            preview_cache: HashMap::new(),
            scenes: Vec::new(),
            preview_scene_id: String::new(),
            program_scene_id: String::new(),
            transition: None,
            mixer: production::MixerState::default(),
            stream_settings: production::StreamSettings::default(),
            stream_lifecycle: production::StreamLifecycle::Idle,
            record_settings: production::RecordSettings::default(),
            record_runtime: None,
            record_error: None,
            macros: Vec::new(),
            media_producers: HashMap::new(),
            media_last_frame: HashMap::new(),
            audio_mixer: sf_audio::Mixer::new(),
            meter_values: HashMap::new(),
            meters_live: false,
            active_recording: None,
            active_stream: None,
            perf_frame_times: VecDeque::new(),
            perf_last_tick_us: 0,
            perf_tick_accum_us: 0,
            perf_window_start: Instant::now(),
        };
        shell.init_production();
        // A default input so the UI is usable immediately (the engine
        // itself starts empty; capture will add real inputs later).
        shell.add_input("cam1", "Camera 1");
        if let Some(dir) = find_looks_dir() {
            if let Ok(reg) = LookRegistry::load_dir(&dir) {
                *shell.engine.look_registry_mut() = reg;
            }
        }
        Ok(shell)
    }

    /// Test constructor with an explicit looks directory.
    pub fn new_with_looks_dir(looks_dir: &std::path::Path) -> Result<Self, ShellError> {
        let mut shell = Self {
            engine: Engine::new_headless()?,
            events: VecDeque::new(),
            last_frame: HashMap::new(),
            preview_cache: HashMap::new(),
            scenes: Vec::new(),
            preview_scene_id: String::new(),
            program_scene_id: String::new(),
            transition: None,
            mixer: production::MixerState::default(),
            stream_settings: production::StreamSettings::default(),
            stream_lifecycle: production::StreamLifecycle::Idle,
            record_settings: production::RecordSettings::default(),
            record_runtime: None,
            record_error: None,
            macros: Vec::new(),
            media_producers: HashMap::new(),
            media_last_frame: HashMap::new(),
            audio_mixer: sf_audio::Mixer::new(),
            meter_values: HashMap::new(),
            meters_live: false,
            active_recording: None,
            active_stream: None,
            perf_frame_times: VecDeque::new(),
            perf_last_tick_us: 0,
            perf_tick_accum_us: 0,
            perf_window_start: Instant::now(),
        };
        shell.init_production();
        if let Ok(reg) = LookRegistry::load_dir(looks_dir) {
            *shell.engine.look_registry_mut() = reg;
        }
        Ok(shell)
    }

    /// Direct (test) access to the engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Direct (test) mutable access to the engine.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Registers an input (mirrors a connected camera).
    pub fn add_input(&mut self, id: &str, name: &str) {
        self.engine.state_mut().add_input(id, name);
    }

    /// Current grade snapshot for the UI (slider positions).
    pub fn grade_snapshot(&self, input_id: &str) -> Result<ColorGrade, ShellError> {
        self.engine
            .state()
            .input_color(input_id)
            .map(|c| c.grade.clone())
            .ok_or_else(|| ShellError::UnknownInput(input_id.to_string()))
    }

    /// Mutable grade for a known input.
    fn grade_mut(&mut self, input_id: &str) -> Result<&mut ColorGrade, ShellError> {
        self.engine
            .state_mut()
            .input_color_mut(input_id)
            .map(|c| &mut c.grade)
            .ok_or_else(|| ShellError::UnknownInput(input_id.to_string()))
    }

    /// `color.set_param` — sets one scalar grade parameter (UI units),
    /// clamped to the spec §4.1 ranges. Wheel channels use suffixed names:
    /// `lift_r`, `gain_g`, `offset_master`, …; the frontend throttles drags
    /// to 60 Hz (spec §8).
    pub fn set_param(
        &mut self,
        input_id: &str,
        param: &str,
        value: f32,
    ) -> Result<ColorGrade, ShellError> {
        let grade = self.grade_mut(input_id)?;
        apply_param(grade, param, value)?;
        let snapshot = grade.clone();
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        Ok(snapshot)
    }

    /// Resets a single parameter to its neutral default (spec §8:
    /// double-click a slider = reset).
    pub fn reset_param(&mut self, input_id: &str, param: &str) -> Result<ColorGrade, ShellError> {
        let grade = self.grade_mut(input_id)?;
        reset_single_param(grade, param)?;
        let snapshot = grade.clone();
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        Ok(snapshot)
    }

    /// `color.set_curve` — replaces one curve's control points (`0..=1`).
    /// Curve names: `master`, `r`, `g`, `b`, `luma`, `hue_vs_sat`.
    pub fn set_curve(
        &mut self,
        input_id: &str,
        curve: &str,
        points: Vec<[f32; 2]>,
    ) -> Result<(), ShellError> {
        let grade = self.grade_mut(input_id)?;
        let target = match curve {
            "master" => &mut grade.curves.master,
            "r" => &mut grade.curves.r,
            "g" => &mut grade.curves.g,
            "b" => &mut grade.curves.b,
            "luma" => &mut grade.curves.luma,
            "hue_vs_sat" => &mut grade.curves.hue_vs_sat,
            _ => return Err(ShellError::UnknownCurve(curve.to_string())),
        };
        target
            .set_points(points)
            .map_err(|e| ShellError::UnknownCurve(format!("{curve}: {e}")))?;
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        Ok(())
    }

    /// `color.load_lut` — attaches a `.cube` LUT to an input.
    /// `position` is `"before"` or `"after"` (spec §4.4); `intensity`
    /// `0.0..=1.0`. Bad files surface at grade time via the engine's
    /// `LutLoad` error (spec: show parse error, keep old LUT).
    pub fn load_lut(
        &mut self,
        input_id: &str,
        path: &str,
        position: &str,
        intensity: f32,
    ) -> Result<(), ShellError> {
        let pos = match position {
            "before" => LutPosition::Before,
            "after" => LutPosition::After,
            _ => LutPosition::After,
        };
        let grade = self.grade_mut(input_id)?;
        grade.lut = Some(LutRef {
            file: path.to_string(),
            intensity: intensity.clamp(0.0, 1.0),
            position: pos,
        });
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        Ok(())
    }

    /// `color.reset` — resets grade sections. `section`: `"basic"`,
    /// `"wheels"`, `"curves"`, `"lut"`, or `None` for everything.
    pub fn reset(
        &mut self,
        input_id: &str,
        section: Option<&str>,
    ) -> Result<ColorGrade, ShellError> {
        let grade = self.grade_mut(input_id)?;
        match section {
            Some("basic") => reset_basic(grade),
            Some("wheels") => reset_wheels(grade),
            Some("curves") => grade.curves = Default::default(),
            Some("lut") => grade.lut = None,
            _ => *grade = ColorGrade::default(),
        }
        let snapshot = grade.clone();
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        Ok(snapshot)
    }
}

/// Applies one named scalar parameter with spec-range clamping.
fn apply_param(grade: &mut ColorGrade, param: &str, value: f32) -> Result<(), ShellError> {
    let cl = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi);
    match param {
        "exposure" => grade.exposure = cl(value, -3.0, 3.0),
        "brightness" => grade.brightness = cl(value, -100.0, 100.0),
        "contrast" => grade.contrast = cl(value, -100.0, 100.0),
        "saturation" => grade.saturation = cl(value, 0.0, 200.0),
        "vibrance" => grade.vibrance = cl(value, -100.0, 100.0),
        "temperature" => grade.temperature = cl(value, 2500.0, 10000.0),
        "tint" => grade.tint = cl(value, -100.0, 100.0),
        "highlights" => grade.highlights = cl(value, -100.0, 100.0),
        "shadows" => grade.shadows = cl(value, -100.0, 100.0),
        "whites" => grade.whites = cl(value, -100.0, 100.0),
        "blacks" => grade.blacks = cl(value, -100.0, 100.0),
        "gamma" => grade.gamma = cl(value, 0.5, 2.5),
        "sharpness" => grade.sharpness = cl(value, 0.0, 100.0),
        _ => {
            // Wheel channels: `<wheel>_<r|g|b|master>`.
            let (wheel, ch) = param
                .rsplit_once('_')
                .ok_or_else(|| ShellError::UnknownParam(param.to_string()))?;
            let idx = match ch {
                "r" => 0,
                "g" => 1,
                "b" => 2,
                "master" => 3,
                _ => return Err(ShellError::UnknownParam(param.to_string())),
            };
            match wheel {
                "lift" => grade.lift[idx] = cl(value, -1.0, 1.0),
                "gamma_wheel" => grade.gamma_wheel[idx] = cl(value, 0.1, 3.0),
                "gain" => grade.gain[idx] = cl(value, 0.0, 4.0),
                "offset" => grade.offset[idx] = cl(value, -1.0, 1.0),
                _ => return Err(ShellError::UnknownParam(param.to_string())),
            }
        }
    }
    Ok(())
}

/// Neutral default for one parameter (mirrors `apply_param`'s names).
fn reset_single_param(grade: &mut ColorGrade, param: &str) -> Result<(), ShellError> {
    let neutral = match param {
        "exposure" | "brightness" | "contrast" | "vibrance" | "tint" | "highlights" | "shadows"
        | "whites" | "blacks" | "sharpness" => 0.0,
        "saturation" => 100.0,
        "temperature" => 6500.0,
        "gamma" => 1.0,
        _ => {
            let (wheel, ch) = param
                .rsplit_once('_')
                .ok_or_else(|| ShellError::UnknownParam(param.to_string()))?;
            if !matches!(ch, "r" | "g" | "b" | "master") {
                return Err(ShellError::UnknownParam(param.to_string()));
            }
            match wheel {
                "lift" | "offset" => 0.0,
                "gamma_wheel" | "gain" => 1.0,
                _ => return Err(ShellError::UnknownParam(param.to_string())),
            }
        }
    };
    apply_param(grade, param, neutral)
}

fn reset_basic(grade: &mut ColorGrade) {
    grade.exposure = 0.0;
    grade.brightness = 0.0;
    grade.contrast = 0.0;
    grade.saturation = 100.0;
    grade.vibrance = 0.0;
    grade.temperature = 6500.0;
    grade.tint = 0.0;
    grade.highlights = 0.0;
    grade.shadows = 0.0;
    grade.whites = 0.0;
    grade.blacks = 0.0;
    grade.gamma = 1.0;
    grade.sharpness = 0.0;
}

fn reset_wheels(grade: &mut ColorGrade) {
    grade.lift = [0.0; 4];
    grade.gamma_wheel = [1.0; 4];
    grade.gain = [1.0; 4];
    grade.offset = [0.0; 4];
}

/// Locates the built-in looks directory: explicit env var first, then the
/// repo layout relative to this crate, then a local `./assets/looks`.
fn find_looks_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("STREAMFLOW_LOOKS_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    // apps/desktop/backend/../../../assets/looks
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("assets")
        .join("looks");
    if repo.is_dir() {
        return Some(repo);
    }
    let local = PathBuf::from("assets").join("looks");
    local.is_dir().then_some(local)
}

impl Shell {
    /// Stores the latest raw frame for an input (stand-in for capture;
    /// feeds [`Shell::auto_color`], [`Shell::match_cameras`], and preview).
    pub fn ingest_frame(&mut self, input_id: &str, width: u32, height: u32, pixels: Vec<u8>) {
        self.last_frame.insert(
            input_id.to_string(),
            Frame {
                width,
                height,
                pixels,
            },
        );
    }

    /// `color.auto` — one-click AI auto color. Analyzes the input's latest
    /// frame and lands the result as **editable parameters** (spec §6.1,
    /// never black-box). The returned explanation strings are shown in the UI.
    pub fn auto_color(
        &mut self,
        input_id: &str,
    ) -> Result<sf_ai_color::auto::AutoColorResult, ShellError> {
        let frame = self
            .last_frame
            .get(input_id)
            .ok_or_else(|| ShellError::NoFrame(input_id.to_string()))?;
        let stats = compute_stats(&frame.pixels, frame.width, frame.height);
        let result = auto_color(&stats);
        let grade = self.grade_mut(input_id)?;
        *grade = result.grade.clone();
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        Ok(result)
    }

    /// `color.match` — AI camera match: adjusts `target_id` toward
    /// `reference_id` (spec §6.2). The shell applies the final grade
    /// immediately; the 0.5 s smooth apply is a UI animation concern.
    pub fn match_cameras(
        &mut self,
        reference_id: &str,
        target_id: &str,
    ) -> Result<sf_ai_color::camera_match::MatchResult, ShellError> {
        let reference = self
            .last_frame
            .get(reference_id)
            .ok_or_else(|| ShellError::NoFrame(reference_id.to_string()))?;
        let target = self
            .last_frame
            .get(target_id)
            .ok_or_else(|| ShellError::NoFrame(target_id.to_string()))?;
        let ref_stats = compute_stats(&reference.pixels, reference.width, reference.height);
        let tgt_stats = compute_stats(&target.pixels, target.width, target.height);
        let result = camera_match(&ref_stats, &tgt_stats);
        let grade = self.grade_mut(target_id)?;
        *grade = result.grade.clone();
        self.events.push_back(UiEvent::ColorChanged {
            input_id: target_id.to_string(),
        });
        self.events.push_back(UiEvent::ColorMatchDone {
            input_id: target_id.to_string(),
        });
        Ok(result)
    }

    /// Resolves a filter-stack target to its mutable stack.
    fn stack_mut(&mut self, target: &FilterTarget) -> Result<&mut FilterStack, ShellError> {
        let state = self.engine.state_mut();
        match target {
            FilterTarget::Input(id) => state
                .inputs
                .iter_mut()
                .find(|i| i.id == *id)
                .map(|i| &mut i.filters)
                .ok_or_else(|| ShellError::UnknownInput(id.clone())),
            FilterTarget::Scene(id) => state
                .scenes
                .iter_mut()
                .find(|s| s.id == *id)
                .map(|s| &mut s.filters)
                .ok_or_else(|| ShellError::UnknownTarget(format!("scene {id}"))),
            FilterTarget::Program => Ok(&mut state.program_filters),
        }
    }

    /// Resolves a filter-stack target to its shared stack.
    fn stack(&self, target: &FilterTarget) -> Result<&FilterStack, ShellError> {
        let state = self.engine.state();
        match target {
            FilterTarget::Input(id) => state
                .inputs
                .iter()
                .find(|i| i.id == *id)
                .map(|i| &i.filters)
                .ok_or_else(|| ShellError::UnknownInput(id.clone())),
            FilterTarget::Scene(id) => state
                .scenes
                .iter()
                .find(|s| s.id == *id)
                .map(|s| &s.filters)
                .ok_or_else(|| ShellError::UnknownTarget(format!("scene {id}"))),
            FilterTarget::Program => Ok(&state.program_filters),
        }
    }

    /// `filter.add` — appends a slot; the engine assigns the slot id.
    /// Emits a `gpu.cost_warning` when the stack grows past [`STACK_WARN_LEN`]
    /// (spec §5.3 "GPU cost high").
    pub fn filter_add(&mut self, target: FilterTarget, filter: Filter) -> Result<Uuid, ShellError> {
        let id = Uuid::new_v4();
        let slot = FilterSlot {
            id,
            filter,
            enabled: true,
            intensity: 1.0,
            mask: None,
        };
        let stack = self.stack_mut(&target)?;
        stack.push(slot)?;
        let stack_len = stack.slots().len();
        if stack_len > STACK_WARN_LEN {
            self.events.push_back(UiEvent::GpuCostWarning {
                message: format!(
                    "Filter cost high: {stack_len} filters on this stack — consider disabling heavy filters"
                ),
            });
        }
        self.events
            .push_back(UiEvent::FilterStackChanged { target });
        Ok(id)
    }

    /// `filter.reorder` — drag-reorders a stack (spec §5.3).
    pub fn filter_reorder(
        &mut self,
        target: FilterTarget,
        order: Vec<Uuid>,
    ) -> Result<(), ShellError> {
        self.stack_mut(&target)?.reorder(&order)?;
        self.events
            .push_back(UiEvent::FilterStackChanged { target });
        Ok(())
    }

    /// `filter.set_intensity` — slot strength `0.0..=1.0` (spec: every filter
    /// has an intensity slider).
    pub fn filter_set_intensity(
        &mut self,
        target: FilterTarget,
        slot_id: Uuid,
        intensity: f32,
    ) -> Result<(), ShellError> {
        self.stack_mut(&target)?
            .set_intensity(&slot_id, intensity.clamp(0.0, 1.0))?;
        self.events
            .push_back(UiEvent::FilterStackChanged { target });
        Ok(())
    }

    /// `filter.toggle` — one-click bypass (spec §1).
    pub fn filter_toggle(
        &mut self,
        target: FilterTarget,
        slot_id: Uuid,
        enabled: bool,
    ) -> Result<(), ShellError> {
        self.stack_mut(&target)?.toggle(&slot_id, enabled)?;
        self.events
            .push_back(UiEvent::FilterStackChanged { target });
        Ok(())
    }

    /// Removes a slot (UI affordance; the IPC spec has no remove command).
    pub fn filter_remove(&mut self, target: FilterTarget, slot_id: Uuid) -> Result<(), ShellError> {
        if !self.stack_mut(&target)?.remove(&slot_id) {
            return Err(ShellError::UnknownTarget(format!("slot {slot_id}")));
        }
        self.events
            .push_back(UiEvent::FilterStackChanged { target });
        Ok(())
    }

    /// `look.apply` — one click applies a full look at `intensity`
    /// (spec §5.1). Returns the new slot id.
    ///
    /// The requested intensity lives on the *slot* (so the uniform
    /// `filter.set_intensity` slider drives it); the filter's own intensity
    /// stays 1.0 — `resolve_grade` multiplies both, so the effective
    /// strength is exactly the requested value.
    pub fn look_apply(
        &mut self,
        target: FilterTarget,
        look_id: &str,
        intensity: f32,
    ) -> Result<Uuid, ShellError> {
        if self.engine.look_registry_mut().get(look_id).is_none() {
            return Err(ShellError::UnknownLook(look_id.to_string()));
        }
        let id = Uuid::new_v4();
        let slot = FilterSlot {
            id,
            filter: Filter::Look {
                preset_id: look_id.to_string(),
                intensity: 1.0,
            },
            enabled: true,
            intensity: intensity.clamp(0.0, 1.0),
            mask: None,
        };
        let stack = self.stack_mut(&target)?;
        stack.push(slot)?;
        let stack_len = stack.slots().len();
        if stack_len > STACK_WARN_LEN {
            self.events.push_back(UiEvent::GpuCostWarning {
                message: format!(
                    "Filter cost high: {stack_len} filters on this stack — consider disabling heavy filters"
                ),
            });
        }
        self.events
            .push_back(UiEvent::FilterStackChanged { target });
        Ok(id)
    }

    /// Serializable snapshot of a stack for the Filters tab.
    pub fn stack_snapshot(&mut self, target: &FilterTarget) -> Result<Vec<SlotView>, ShellError> {
        // Clone the slots first: `slot_view` needs `&mut self` for the
        // look registry, which conflicts with the shared stack borrow.
        let slots: Vec<FilterSlot> = self.stack(target)?.slots().to_vec();
        Ok(slots.iter().map(|slot| self.slot_view(slot)).collect())
    }

    fn slot_view(&mut self, slot: &FilterSlot) -> SlotView {
        match &slot.filter {
            Filter::Look {
                preset_id,
                intensity: _,
            } => {
                let name = self
                    .engine
                    .look_registry_mut()
                    .get(preset_id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| preset_id.clone());
                SlotView {
                    id: slot.id,
                    label: format!("Look: {name}"),
                    kind: "look".to_string(),
                    enabled: slot.enabled,
                    intensity: slot.intensity,
                    cost: "low".to_string(),
                    is_ai: false,
                }
            }
            Filter::Effect { kind, params: _ } => SlotView {
                id: slot.id,
                label: effect_label(kind),
                kind: format!("{kind:?}").to_lowercase(),
                enabled: slot.enabled,
                intensity: slot.intensity,
                cost: cost_name(kind.cost()),
                is_ai: kind.is_ai(),
            },
        }
    }

    /// The 30 built-in looks for the looks strip (spec §8).
    pub fn list_looks(&mut self) -> Vec<LookInfo> {
        let mut ids = self.engine.look_registry_mut().ids();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| {
                self.engine.look_registry_mut().get(&id).map(|p| LookInfo {
                    id,
                    name: p.name.clone(),
                    category: format!("{:?}", p.category),
                })
            })
            .collect()
    }

    /// `scopes.subscribe` — `kind`: `waveform`, `vectorscope`, `histogram`,
    /// `false_color`, `zebra`.
    pub fn scopes_subscribe(&mut self, input_id: &str, kind: &str) -> Result<(), ShellError> {
        let kind = parse_scope_kind(kind)?;
        self.engine.scopes_subscribe(input_id, kind)?;
        Ok(())
    }

    /// `scopes.unsubscribe`.
    pub fn scopes_unsubscribe(&mut self, input_id: &str, kind: &str) -> Result<(), ShellError> {
        let kind = parse_scope_kind(kind)?;
        self.engine.scopes_unsubscribe(input_id, kind);
        Ok(())
    }

    /// Drains engine scope frames (as [`UiEvent::ScopesFrame`] with payload)
    /// followed by queued command-echo events. The Tauri adapter polls this.
    pub fn drain_events(&mut self) -> Vec<UiEvent> {
        let mut out = Vec::new();
        for frame in self.engine.drain_scope_frames() {
            out.push(UiEvent::ScopesFrame {
                input_id: frame.input_id.clone(),
                seq: frame.seq,
                kind: ScopeDataView::kind_name(&frame).to_string(),
                data: ScopeDataView::from_payload(&frame.payload),
            });
        }
        out.extend(self.events.drain(..));
        out
    }
}

fn parse_scope_kind(kind: &str) -> Result<ScopeKind, ShellError> {
    match kind {
        "waveform" => Ok(ScopeKind::Waveform),
        "vectorscope" => Ok(ScopeKind::Vectorscope),
        "histogram" => Ok(ScopeKind::Histogram),
        "false_color" => Ok(ScopeKind::FalseColor),
        "zebra" => Ok(ScopeKind::Zebra),
        _ => Err(ShellError::UnknownScopeKind(kind.to_string())),
    }
}

fn cost_name(cost: GpuCost) -> String {
    match cost {
        GpuCost::Low => "low",
        GpuCost::Medium => "medium",
        GpuCost::High => "high",
    }
    .to_string()
}

/// Human label for an effect kind (the Filters tab + stack list).
fn effect_label(kind: &EffectKind) -> String {
    match kind {
        EffectKind::Sharpen => "Sharpen",
        EffectKind::GaussianBlur => "Gaussian Blur",
        EffectKind::BackgroundBlur => "Background Blur (AI)",
        EffectKind::BackgroundReplace => "Background Replace (AI)",
        EffectKind::ChromaKey => "Chroma Key",
        EffectKind::Vignette => "Vignette",
        EffectKind::FilmGrain => "Film Grain",
        EffectKind::Bloom => "Bloom",
        EffectKind::NoiseReduction => "Noise Reduction",
        EffectKind::SkinSmooth => "Skin Smooth (AI)",
        EffectKind::MirrorFlip => "Mirror / Flip",
        EffectKind::CropZoomPan => "Crop / Zoom / Pan",
        EffectKind::LensCorrection => "Lens Correction",
        EffectKind::Letterbox => "Letterbox",
        EffectKind::ChromaticAberration => "Chromatic Aberration",
        EffectKind::Pixelate => "Pixelate",
    }
    .to_string()
}

impl Shell {
    /// `preview.get_frame` — grades the input's latest synthetic frame and
    /// returns PNG bytes, throttled to ~10 Hz per input (spec has no preview
    /// rule; production will use a faster zero-copy path — documented in the
    /// Tauri adapter).
    pub fn preview_png(
        &mut self,
        input_id: &str,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, ShellError> {
        if self.engine.state().input_color(input_id).is_none() {
            return Err(ShellError::UnknownInput(input_id.to_string()));
        }
        if let Some((at, png)) = self.preview_cache.get(input_id) {
            if at.elapsed() < PREVIEW_THROTTLE {
                return Ok(png.clone());
            }
        }
        // Prefer a real camera/media frame when a production source exists;
        // fall back to the synthetic test chart otherwise.
        let pixels = self
            .real_preview_frame(width, height)
            .unwrap_or_else(|| synthetic_frame(width, height));
        self.ingest_frame(input_id, width, height, pixels.clone());
        let frame = self
            .engine
            .grade_input_frame(input_id, width, height, &pixels)?;
        let png = encode_png(&frame)?;
        self.preview_cache
            .insert(input_id.to_string(), (Instant::now(), png.clone()));
        Ok(png)
    }

    /// Records a completed frame for FPS calculation. Call once per
    /// rendered frame (from the poll loop).
    pub fn perf_mark_frame(&mut self) {
        let now = Instant::now();
        self.perf_frame_times.push_back(now);
        // Keep only the last 120 frame times (2 seconds at 60fps).
        while self.perf_frame_times.len() > 120 {
            self.perf_frame_times.pop_front();
        }
    }

    /// Records av_tick duration for render-time tracking.
    pub fn perf_mark_tick(&mut self, duration_us: u64) {
        self.perf_last_tick_us = duration_us;
        self.perf_tick_accum_us += duration_us;
    }

    /// Returns (fps, render_ms, cpu_percent_estimate).
    /// CPU% is estimated as (tick time / wall time) over the last second.
    pub fn perf_stats(&mut self) -> (f32, f32, f32) {
        let now = Instant::now();
        // FPS from frame timestamps in the last second.
        let cutoff = now - std::time::Duration::from_secs(1);
        while self.perf_frame_times.front().map(|t| *t < cutoff).unwrap_or(false) {
            self.perf_frame_times.pop_front();
        }
        let fps = self.perf_frame_times.len() as f32;
        let render_ms = self.perf_last_tick_us as f32 / 1000.0;
        // CPU% estimate: reset window every second.
        let elapsed = now.duration_since(self.perf_window_start).as_micros() as f32;
        let cpu_pct = if elapsed > 0.0 {
            (self.perf_tick_accum_us as f32 / elapsed * 100.0).min(100.0)
        } else {
            0.0
        };
        if elapsed >= 1_000_000.0 {
            self.perf_window_start = now;
            self.perf_tick_accum_us = 0;
        }
        (fps, render_ms, cpu_pct)
    }

    /// Pulls a real video frame from the first available camera/media
    /// production source, resized to `width`x`height`. Returns `None` when
    /// no real source exists or no frame is ready.
    fn real_preview_frame(&mut self, width: u32, height: u32) -> Option<Vec<u8>> {
        // Find the first camera/media source with a live producer.
        let source_id = self
            .scenes
            .iter()
            .flat_map(|s| s.sources.iter())
            .find(|src| {
                matches!(
                    src.kind,
                    production::SourceKind::Camera { .. } | production::SourceKind::Media { .. }
                ) && self.media_producers.contains_key(&src.id)
            })
            .map(|src| src.id.clone())?;
        self.pull_media_frame(&source_id, width, height)
    }
}

/// Deterministic test-chart frame (gradient + color bars + skin-tone patch)
/// used as the capture stand-in for preview and shell demos.
fn synthetic_frame(width: u32, height: u32) -> Vec<u8> {
    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    let mut px = Vec::with_capacity(w * h * 4);
    // 6 color bars across the top third, luma ramp in the middle third,
    // a skin-tone patch + dark background in the bottom third.
    let bars: [[u8; 3]; 6] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
        [255, 0, 255],
    ];
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let (r, g, b) = if fy < 0.34 {
                let b = bars[(fx * 6.0) as usize % 6];
                (b[0], b[1], b[2])
            } else if fy < 0.67 {
                let v = (fx * 255.0) as u8;
                (v, v, v)
            } else if fx > 0.35 && fx < 0.65 && fy > 0.72 && fy < 0.95 {
                (200, 150, 120) // skin-tone patch (matches C4/C6 test color)
            } else {
                let v = (30.0 + fx * 40.0) as u8;
                (v, v, (v as f32 * 1.2) as u8)
            };
            px.extend_from_slice(&[r, g, b, 255]);
        }
    }
    px
}

/// Encodes an RGBA8 frame as PNG bytes.
fn encode_png(frame: &Frame) -> Result<Vec<u8>, ShellError> {
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
        .write_image(
            &frame.pixels,
            frame.width,
            frame.height,
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| ShellError::Png(e.to_string()))?;
    Ok(buf)
}

/// UI-facing AI result: the suggested grade plus one explanation string per
/// decision (spec §6: results land as editable parameters, never black-box).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiResultView {
    /// Suggested grade in UI units, ready to land on the sliders.
    pub grade: ColorGrade,
    /// Human-readable explanation per change.
    pub explanation: Vec<String>,
}

/// UI-facing effect-kind catalog entry (drives the Filters tab "add" menu).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectKindInfo {
    /// Machine name, e.g. `"vignette"`.
    pub kind: String,
    /// Human label, e.g. `"Vignette"`.
    pub label: String,
    /// Estimated GPU cost badge: `"low"`, `"medium"`, `"high"`.
    pub cost: String,
    /// True for AI kinds (may grey out without a model).
    pub is_ai: bool,
    /// Per-kind default parameters.
    pub default_params: serde_json::Value,
}

impl Shell {
    /// All effect kinds with labels, cost badges, and defaults.
    pub fn list_effect_kinds(&self) -> Vec<EffectKindInfo> {
        const KINDS: [EffectKind; 16] = [
            EffectKind::Sharpen,
            EffectKind::GaussianBlur,
            EffectKind::BackgroundBlur,
            EffectKind::BackgroundReplace,
            EffectKind::ChromaKey,
            EffectKind::Vignette,
            EffectKind::FilmGrain,
            EffectKind::Bloom,
            EffectKind::NoiseReduction,
            EffectKind::SkinSmooth,
            EffectKind::MirrorFlip,
            EffectKind::CropZoomPan,
            EffectKind::LensCorrection,
            EffectKind::Letterbox,
            EffectKind::ChromaticAberration,
            EffectKind::Pixelate,
        ];
        KINDS
            .iter()
            .map(|kind| EffectKindInfo {
                kind: format!("{kind:?}").to_lowercase(),
                label: effect_label(kind),
                cost: cost_name(kind.cost()),
                is_ai: kind.is_ai(),
                default_params: serde_json::to_value(kind.default_params())
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect()
    }

    /// UI-friendly `color.auto` returning a serializable view.
    pub fn color_auto(&mut self, input_id: &str) -> Result<AiResultView, ShellError> {
        let r = self.auto_color(input_id)?;
        Ok(AiResultView {
            grade: r.grade,
            explanation: r.explanation,
        })
    }

    /// UI-friendly `color.match` returning a serializable view.
    pub fn color_match(
        &mut self,
        reference_id: &str,
        target_id: &str,
    ) -> Result<AiResultView, ShellError> {
        let r = self.match_cameras(reference_id, target_id)?;
        Ok(AiResultView {
            grade: r.grade,
            explanation: r.explanation,
        })
    }
}

impl Shell {
    /// Applies a full grade snapshot (UI affordance for the spec §8 A/B
    /// grade slots and the before/after compare toggle).
    pub fn apply_grade(
        &mut self,
        input_id: &str,
        grade: ColorGrade,
    ) -> Result<ColorGrade, ShellError> {
        let g = self.grade_mut(input_id)?;
        *g = grade;
        let snapshot = g.clone();
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        Ok(snapshot)
    }
}
