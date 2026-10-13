//! Production state machine for the StreamFlow Lite desktop shell:
//! scenes, sources, preview/program, transitions, mixer, streaming,
//! recording, and macros.
//!
//! All logic lives on [`Shell`](super::Shell) (headless-testable, no tauri
//! dependency); `apps/desktop/src-tauri` maps Tauri commands onto these
//! methods 1:1.
//!
//! # What is real vs. honest stub (production scope)
//!
//! - REAL: scene/source CRUD, preview/program assignment, cut + auto
//!   transitions (wall-clock progress, smoothstep easing), CPU
//!   "over"-composite of graded source frames, mixer gain/mute state,
//!   macro dispatch — all against the real engine (`grade_input_frame`).
//! - REAL sources: `TestPattern` (reuses the synthetic test-chart
//!   generator), `Color` (solid frame), and `Media` (file decode via
//!   `sf-capture`'s `MediaFileProducer`, with real audio into the mixer).
//!   Each gets a real engine input and renders through the real grade
//!   pipeline.
//! - REAL outputs: `record_start` records the program composite to MP4/MKV
//!   via `sf-encode` + `sf-output` (driven by the adapter poll loop);
//!   `stream_start` attempts a real RTMP connection via `sf-output`'s
//!   `Streamer` (fails cleanly with a redacted error when unreachable,
//!   reconnect policy live).
//! - REAL meters: `sf-audio`'s mixer runs on decoded media audio;
//!   `meters_live` flips true once audio flows.
//! - HONEST stubs: `Camera`/`Screen`/`Text` sources report
//!   `status: "pending"` and are skipped in the composite — never faked.
//!
//! Scene-level [`FilterStack`]s are modeled and stored on [`ProdScene`]
//! (spec §5.3 attaches stacks at scene level) but are not yet applied in
//! the composite: applying an arbitrary stack to a composited frame needs
//! engine support that does not exist yet. Input-level stacks (incl. the
//! macro `ApplyLook`) do run through the real pipeline.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sf_engine::Frame;
use sf_filters::{FilterSlot, FilterStack};
use sf_ipc::FilterTarget;
use uuid::Uuid;

use super::{encode_png, synthetic_frame, Shell, ShellError, UiEvent};

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// What a production source produces.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SourceKind {
    /// Deterministic test chart (capture stand-in).
    TestPattern,
    /// Solid color frame.
    Color {
        /// Red 0-255.
        r: u8,
        /// Green 0-255.
        g: u8,
        /// Blue 0-255.
        b: u8,
    },
    /// Real camera on Windows (DirectShow); `device` selects which
    /// camera (None = first available).
    Camera {
        /// DirectShow device name, e.g. from `list_cameras`.
        device: Option<String>,
        /// Capture width (0 = default 1280).
        width: u32,
        /// Capture height (0 = default 720).
        height: u32,
        /// Capture fps (0 = default 30).
        fps: u32,
        /// Optional DirectShow audio device captured alongside the video
        /// (`video=X:audio=Y`); its audio flows to the mixer like any
        /// audio input. None = video only.
        audio_device: Option<String>,
    },
    /// Microphone / line-in audio input (Windows DirectShow audio;
    /// `device` selects which, None = first available). Audio-only:
    /// contributes to the mixer, renders no video.
    AudioInput {
        /// DirectShow audio device name, e.g. from `list_audio_devices`.
        device: Option<String>,
    },
    /// Screen share (Windows: real desktop capture via FFmpeg gdigrab;
    /// Linux: x11grab via `sf-capture::screen`, needs an X server).
    Screen,
    /// Media file (real: decoded via `sf-capture`'s `MediaFileProducer`).
    Media {
        /// File path.
        path: String,
    },
    /// Text / lower-third (real: rasterized via `sf-graphics` + fontdue).
    Text {
        /// Text content.
        text: String,
    },
}

impl SourceKind {
    /// True for kinds that produce real frames today.
    ///
    /// `Camera` is real on Windows (DirectShow via `sf-capture::dshow`);
    /// on other platforms it has no backend yet.
    pub fn is_real(&self) -> bool {
        match self {
            SourceKind::TestPattern
            | SourceKind::Color { .. }
            | SourceKind::Media { .. }
            | SourceKind::Text { .. } => true,
            #[cfg(any(windows, sf_wincheck))]
            SourceKind::Camera { .. } => true,
            #[cfg(not(any(windows, sf_wincheck)))]
            SourceKind::Camera { .. } => false,
            #[cfg(any(windows, sf_wincheck))]
            SourceKind::AudioInput { .. } => true,
            #[cfg(not(any(windows, sf_wincheck)))]
            SourceKind::AudioInput { .. } => false,
            #[cfg(any(windows, sf_wincheck))]
            SourceKind::Screen => true,
            #[cfg(not(any(windows, sf_wincheck)))]
            SourceKind::Screen => false,
        }
    }

    /// Why a pending kind has no frames, if pending.
    pub fn pending_reason(&self) -> Option<&'static str> {
        if self.is_real() {
            None
        } else {
            Some("capture backend not implemented for this source kind")
        }
    }

    /// UI badge: `"ready"` or `"pending"`.
    pub fn status(&self) -> &'static str {
        if self.is_real() {
            "ready"
        } else {
            "pending"
        }
    }
}

/// One source inside a scene.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceState {
    /// Stable source id (also keys the mixer channel).
    pub id: String,
    /// Display name.
    pub name: String,
    /// What the source produces.
    pub kind: SourceKind,
    /// Whether it participates in the composite.
    pub visible: bool,
    /// Composite order (0 = bottom).
    pub order: u32,
    /// Engine input id for real kinds; `None` for pending kinds.
    pub input_id: Option<String>,
    /// `"ready"` or `"pending"` (UI badge).
    pub status: String,
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

/// A production scene: ordered sources + a scene-level filter stack
/// (spec §5.3; the stack is stored, not yet applied in the composite).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProdScene {
    /// Stable scene id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Sources, composited bottom-to-top by `order`.
    pub sources: Vec<SourceState>,
    /// Scene-level filter stack.
    pub filters: FilterStack,
}

// ---------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------

/// Supported transition kinds (only mix today).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    Mix,
}

/// An in-flight auto transition (crate-private runtime state).
pub(crate) struct ActiveTransition {
    pub(crate) from_scene: String,
    pub(crate) to_scene: String,
    pub(crate) started: Instant,
    pub(crate) duration_ms: u64,
    pub(crate) kind: TransitionKind,
}

/// UI-facing transition snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitionView {
    /// Whether a transition is currently running.
    pub active: bool,
    /// Eased 0.0..=1.0 progress.
    pub progress: f32,
    /// Transition kind name (`"mix"`), empty when inactive.
    pub kind: String,
    /// Duration in ms, 0 when inactive.
    pub duration_ms: u64,
}

/// Pure easing/progress math for transitions: wall-clock elapsed over
/// duration, eased with smoothstep `t²(3−2t)`, clamped to `0.0..=1.0`.
/// Kept pure (no `Instant`) so tests never sleep.
pub fn transition_progress(elapsed_ms: u64, duration_ms: u64) -> f32 {
    if duration_ms == 0 {
        return 1.0;
    }
    let t = (elapsed_ms as f32 / duration_ms as f32).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

/// One mixer channel strip.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelStrip {
    /// Linear gain `0.0..=2.0`.
    pub gain: f32,
    /// Mute flag.
    pub mute: bool,
}

impl Default for ChannelStrip {
    fn default() -> Self {
        Self {
            gain: 1.0,
            mute: false,
        }
    }
}

/// Mixer state: master strip + one strip per source id.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MixerState {
    /// Master strip.
    pub master: ChannelStrip,
    /// Per-source strips keyed by source id.
    pub channels: HashMap<String, ChannelStrip>,
}

/// UI-facing mixer snapshot. Meters are real once audio flows: `meters_live`
/// flips true on the first mixed audio block and `meters` carries per-source
/// peaks from `sf-audio`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MixerView {
    /// Master strip.
    pub master: ChannelStrip,
    /// Per-source strips keyed by source id.
    pub channels: HashMap<String, ChannelStrip>,
    /// False until `sf-audio` lands — meters never fake movement.
    pub meters_live: bool,
    /// Per-source meter values (all `0.0` while `meters_live` is false).
    pub meters: HashMap<String, f32>,
    /// True while local speaker monitoring is active.
    pub monitor_enabled: bool,
    /// Voice enhancer (AI denoise → gate → EQ → compressor) settings.
    pub enhancer: EnhancerView,
    /// Live per-voice-source enhancer telemetry + bus health, keyed by
    /// source id (only live inputs: mics / camera audio).
    #[serde(default)]
    pub voice: HashMap<String, VoiceStatsView>,
    /// A/V sync settings.
    #[serde(default)]
    pub av_sync: AvSyncView,
}

/// UI-facing voice-enhancer settings (mirror of
/// `sf_audio::EnhancerSettings`). Runs locally in real time — RNNoise AI
/// denoise + DSP; no network API in the audio path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnhancerView {
    pub enabled: bool,
    pub gate_enabled: bool,
    pub gate_threshold_db: f32,
    pub eq_low_db: f32,
    pub eq_mid_db: f32,
    pub eq_high_db: f32,
    pub compressor_enabled: bool,
    /// AI noise suppression on/off (older saved state: on).
    #[serde(default = "default_true")]
    pub ai_denoise: bool,
    /// AI suppression strength 0..1 (older saved state: full).
    #[serde(default = "default_one")]
    pub ai_strength: f32,
}

fn default_true() -> bool {
    true
}

fn default_one() -> f32 {
    1.0
}

/// Live telemetry for one voice source.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VoiceStatsView {
    /// Voice-activity probability from the AI stage (0..1).
    pub vad: f32,
    /// Compressor gain reduction, dB.
    pub gain_reduction_db: f32,
    /// Gate currently closed (silencing).
    pub gate_closed: bool,
    /// Latency added by the enhancer chain, ms (20 with AI denoise).
    pub latency_ms: f32,
    /// CPU time of the last enhancer block, µs.
    pub process_us: u32,
    /// Audio queued in the bus for this source, ms.
    pub buffered_ms: f32,
    /// Underruns (silence inserted) since the source opened.
    pub underruns: u64,
    /// Sample frames dropped to stay live.
    pub dropped_frames: u64,
    /// Device health: "starting" | "running" | "error: …" | "stopped".
    pub device: String,
    /// Age of the audio as played (capture → mix), ms.
    #[serde(default)]
    pub audio_age_ms: Option<f32>,
    /// Age the A/V aligner targets (picture age + offset), ms.
    #[serde(default)]
    pub target_age_ms: Option<f32>,
    /// Alignment corrections applied so far.
    #[serde(default)]
    pub corrections: u64,
}

/// A/V sync settings. `auto` (default): live audio is played at the
/// measured age of the picture it belongs to; `offset_ms` (−100…+100,
/// positive = audio later) fine-tunes on top. With `auto` off,
/// `offset_ms` (≥ 0) is a plain audio delay.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AvSyncView {
    pub auto: bool,
    pub offset_ms: i32,
}

impl Default for AvSyncView {
    fn default() -> Self {
        Self {
            auto: true,
            offset_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Stream destination platform (adapters in `crate::platforms`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamPlatform {
    Youtube,
    Facebook,
    Tiktok,
    CustomRtmp,
}

/// Stream settings. **The key is never serialized and never appears in
/// `Debug` output, logs, or error strings.**
#[derive(Clone, Serialize, Deserialize)]
pub struct StreamSettings {
    /// RTMP(S) ingest URL.
    pub url: String,
    /// Stream key — redacted everywhere (see `Debug` impl).
    #[serde(skip_serializing, default)]
    pub key: String,
    /// Destination platform.
    pub platform: StreamPlatform,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            url: String::new(),
            key: String::new(),
            platform: StreamPlatform::CustomRtmp,
        }
    }
}

impl fmt::Debug for StreamSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamSettings")
            .field("url", &self.url)
            .field("key", &"[redacted]")
            .field("platform", &self.platform)
            .finish()
    }
}

/// Video input/output settings, configured from the Video menu panel.
///
/// Output (program bus) drives the render size, tick rate, and the
/// stream/record encoders. Input holds the camera defaults used when a new
/// camera source is added (and the target of "Apply" for live reconfigure).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoSettings {
    /// Program output width (1280 | 1920).
    pub output_width: u32,
    /// Program output height (720 | 1080).
    pub output_height: u32,
    /// Program output fps (30 | 60). Also sets the media-tick cadence.
    pub output_fps: u32,
    /// Encoder override: `"auto"` (default) or an FFmpeg encoder name
    /// (`"h264_amf"`, `"h264_nvenc"`, `"h264_qsv"`, `"libx264"`).
    pub encoder: String,
    /// Target video bitrate for stream + record, kilobits per second.
    pub bitrate_kbps: u32,
    /// Default camera device (None = first available).
    pub camera_device: Option<String>,
    /// Default camera capture width.
    pub camera_width: u32,
    /// Default camera capture height.
    pub camera_height: u32,
    /// Default camera capture fps (30 | 60).
    pub camera_fps: u32,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            output_width: 1920,
            output_height: 1080,
            output_fps: 30,
            encoder: "auto".to_string(),
            bitrate_kbps: 8000,
            camera_device: None,
            camera_width: 1280,
            camera_height: 720,
            camera_fps: 30,
        }
    }
}

/// Allowed values for the Video panel dropdowns (also the validation set).
pub const VIDEO_OUTPUT_SIZES: [(u32, u32); 2] = [(1280, 720), (1920, 1080)];
pub const VIDEO_CAMERA_SIZES: [(u32, u32); 3] =
    [(854, 480), (1280, 720), (1920, 1080)];
pub const VIDEO_FPS_OPTIONS: [u32; 2] = [30, 60];
pub const VIDEO_ENCODER_OPTIONS: [&str; 5] =
    ["auto", "h264_amf", "h264_nvenc", "h264_qsv", "libx264"];

/// Stream output lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StreamLifecycle {
    Idle,
    Connecting,
    Live,
    Reconnecting,
    Stopping,
    Error {
        /// Human-readable error (never contains the stream key).
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// Recording container format.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordFormat {
    Mp4,
    Mkv,
}

/// Recording settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordSettings {
    /// Output file path.
    pub path: String,
    /// Container format.
    pub format: RecordFormat,
    /// Quality preset name (UI string; encoder-specific later).
    pub quality: String,
}

impl Default for RecordSettings {
    fn default() -> Self {
        Self {
            path: String::new(),
            format: RecordFormat::Mp4,
            quality: "High".to_string(),
        }
    }
}

/// Recording lifecycle. Elapsed time excludes pauses.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecordLifecycle {
    Idle,
    Recording {
        /// Seconds recorded so far (pauses excluded).
        elapsed_secs: f32,
    },
    Paused {
        /// Seconds recorded so far.
        elapsed_secs: f32,
    },
    Error {
        /// Human-readable error.
        message: String,
    },
}

/// Backend-private recording runtime (`Instant` is not serializable, so the
/// UI only ever sees [`RecordLifecycle`]).
pub(crate) struct RecordRuntime {
    pub(crate) started: Instant,
    /// Accumulated elapsed excluding the current running stretch.
    pub(crate) accumulated: Duration,
    pub(crate) paused: bool,
}

impl RecordRuntime {
    fn elapsed(&self) -> Duration {
        if self.paused {
            self.accumulated
        } else {
            self.accumulated + self.started.elapsed()
        }
    }

    fn pause(&mut self) {
        self.accumulated = self.elapsed();
        self.paused = true;
    }

    fn resume(&mut self) {
        self.started = Instant::now();
        self.paused = false;
    }
}

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

/// One macro action.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MacroAction {
    /// Instant program = preview.
    Cut,
    /// Timed mix transition.
    AutoTransition {
        /// Duration in ms.
        duration_ms: u64,
    },
    /// Start stream if stopped, stop if live.
    ToggleStream,
    /// Start record if stopped, stop if recording.
    ToggleRecord,
    /// Live look-switch: apply a look to every visible real source of the
    /// program scene (spec §7 "live look-switch macro").
    ApplyLook {
        /// Look registry id.
        look_id: String,
        /// Look intensity `0.0..=1.0`.
        intensity: f32,
    },
}

/// A named, hotkey-bound macro.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Macro {
    /// Stable macro id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Hotkey string, e.g. `"F1"`, `"ctrl+1"`.
    pub hotkey: String,
    /// What dispatching executes.
    pub action: MacroAction,
}

/// What a macro dispatch did (returned to the caller / shown in the UI).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroDispatchResult {
    /// Dispatched macro id.
    pub macro_id: String,
    /// Dispatched hotkey.
    pub hotkey: String,
    /// Human-readable summary of what happened.
    pub happened: String,
}

// ---------------------------------------------------------------------------
// Production state view
// ---------------------------------------------------------------------------

/// Full production snapshot for the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProductionStateView {
    /// All scenes.
    pub scenes: Vec<ProdScene>,
    /// Preview (standby) scene id.
    pub preview_scene_id: String,
    /// Program (live) scene id.
    pub program_scene_id: String,
    /// Current transition snapshot.
    pub transition: TransitionView,
    /// Mixer snapshot (meters honest stub).
    pub mixer: MixerView,
    /// Stream settings (key redacted by `skip_serializing`).
    pub stream_settings: StreamSettings,
    /// Stream lifecycle.
    pub stream_lifecycle: StreamLifecycle,
    /// Record settings.
    pub record_settings: RecordSettings,
    /// Record lifecycle.
    pub record_lifecycle: RecordLifecycle,
    /// Video input/output settings (Video menu panel).
    pub video_settings: VideoSettings,
    /// Bound macros.
    pub macros: Vec<Macro>,
}

// ---------------------------------------------------------------------------
// Shell state + init
// ---------------------------------------------------------------------------

impl Shell {
    /// Initializes the production state machine: one default scene with a
    /// test-pattern source, preview = program, idle stream/record, empty
    /// mixer macros. Called by both constructors.
    pub(crate) fn init_production(&mut self) {
        let scene = ProdScene {
            id: Uuid::new_v4().to_string(),
            name: "Scene 1".to_string(),
            sources: Vec::new(),
            filters: FilterStack::default(),
        };
        self.preview_scene_id = scene.id.clone();
        self.program_scene_id = scene.id.clone();
        self.scenes.push(scene);
        self.mixer = MixerState::default();
        self.stream_settings = StreamSettings::default();
        self.video_settings = VideoSettings::default();
        self.stream_lifecycle = StreamLifecycle::Idle;
        self.record_settings = RecordSettings::default();
        self.record_runtime = None;
        self.record_error = None;
        self.macros = Vec::new();
        self.transition = None;
        // A default test-pattern source so the program monitor is never blank.
        let preview = self.preview_scene_id.clone();
        let _ = self.source_add(
            &preview,
            "test_pattern",
            "Test Pattern",
            [0, 0, 0],
            None,
            None,
            None,
            None,
            0,
            0,
            0,
        );
    }

    fn find_scene(&self, scene_id: &str) -> Option<&ProdScene> {
        self.scenes.iter().find(|s| s.id == scene_id)
    }

    fn find_scene_mut(&mut self, scene_id: &str) -> Option<&mut ProdScene> {
        self.scenes.iter_mut().find(|s| s.id == scene_id)
    }

    /// Emits `production.scene_changed` with the current preview/program ids.
    fn emit_scene_changed(&mut self) {
        let preview_scene_id = self.preview_scene_id.clone();
        let program_scene_id = self.program_scene_id.clone();
        self.events.push_back(UiEvent::SceneChanged {
            preview_scene_id,
            program_scene_id,
        });
    }

    /// Non-mutating transition snapshot (wall-clock progress, no events).
    fn transition_view(&self) -> TransitionView {
        match &self.transition {
            None => TransitionView {
                active: false,
                progress: 0.0,
                kind: String::new(),
                duration_ms: 0,
            },
            Some(t) => {
                let elapsed = t.started.elapsed().as_millis() as u64;
                TransitionView {
                    active: true,
                    progress: transition_progress(elapsed, t.duration_ms),
                    kind: match t.kind {
                        TransitionKind::Mix => "mix".to_string(),
                    },
                    duration_ms: t.duration_ms,
                }
            }
        }
    }

    fn record_lifecycle(&self) -> RecordLifecycle {
        if let Some(message) = &self.record_error {
            return RecordLifecycle::Error {
                message: message.clone(),
            };
        }
        match &self.record_runtime {
            None => RecordLifecycle::Idle,
            Some(r) => {
                let elapsed_secs = r.elapsed().as_secs_f32();
                if r.paused {
                    RecordLifecycle::Paused { elapsed_secs }
                } else {
                    RecordLifecycle::Recording { elapsed_secs }
                }
            }
        }
    }

    pub(crate) fn emit_stream_state(&mut self) {
        let lifecycle = self.stream_lifecycle.clone();
        self.events.push_back(UiEvent::StreamState { lifecycle });
    }

    pub(crate) fn emit_record_state(&mut self) {
        let lifecycle = self.record_lifecycle();
        self.events.push_back(UiEvent::RecordState { lifecycle });
    }
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

impl Shell {
    /// Creates a scene with the given name.
    pub fn scene_create(&mut self, name: &str) -> ProdScene {
        let scene = ProdScene {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            sources: Vec::new(),
            filters: FilterStack::default(),
        };
        self.scenes.push(scene.clone());
        self.emit_scene_changed();
        scene
    }

    /// Renames a scene.
    pub fn scene_rename(&mut self, scene_id: &str, name: &str) -> Result<ProdScene, ShellError> {
        let scene = self
            .find_scene_mut(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        scene.name = name.to_string();
        let scene = scene.clone();
        self.emit_scene_changed();
        Ok(scene)
    }

    /// Deletes a scene. Errors when it is the last scene. If the deleted
    /// scene was the preview or program scene, both fall back to the first
    /// remaining scene.
    pub fn scene_delete(&mut self, scene_id: &str) -> Result<(), ShellError> {
        if self.scenes.len() <= 1 {
            return Err(ShellError::LastScene);
        }
        let pos = self
            .scenes
            .iter()
            .position(|s| s.id == scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        let removed = self.scenes.remove(pos);
        // Drop mixer strips of the removed scene's sources.
        for src in &removed.sources {
            self.mixer.channels.remove(&src.id);
        }
        // A transition involving the deleted scene cannot continue.
        self.transition = None;
        if self.preview_scene_id == scene_id || self.program_scene_id == scene_id {
            let fallback = self.scenes[0].id.clone();
            if self.preview_scene_id == scene_id {
                self.preview_scene_id = fallback.clone();
            }
            if self.program_scene_id == scene_id {
                self.program_scene_id = fallback;
            }
        }
        self.emit_scene_changed();
        Ok(())
    }

    /// Duplicates a scene (new ids for the scene and its sources, fresh
    /// engine inputs for real sources, fresh mixer strips).
    pub fn scene_duplicate(&mut self, scene_id: &str) -> Result<ProdScene, ShellError> {
        let src = self
            .find_scene(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?
            .clone();
        let mut dup = ProdScene {
            id: Uuid::new_v4().to_string(),
            name: format!("{} copy", src.name),
            sources: Vec::new(),
            filters: remap_slot_ids(&src.filters),
        };
        for s in &src.sources {
            let new_id = Uuid::new_v4().to_string();
            let input_id = s.kind.is_real().then(|| source_input_id(&new_id));
            if let Some(input_id) = &input_id {
                self.engine.state_mut().add_input(input_id, &s.name);
            }
            self.mixer
                .channels
                .insert(new_id.clone(), ChannelStrip::default());
            dup.sources.push(SourceState {
                id: new_id,
                name: s.name.clone(),
                kind: s.kind.clone(),
                visible: s.visible,
                order: s.order,
                input_id,
                status: s.kind.status().to_string(),
            });
        }
        self.scenes.push(dup.clone());
        self.emit_scene_changed();
        Ok(dup)
    }

    /// All scenes.
    pub fn scene_list(&self) -> Vec<ProdScene> {
        self.scenes.clone()
    }
}

/// Clones a filter stack with fresh slot ids (used by scene duplication).
fn remap_slot_ids(stack: &FilterStack) -> FilterStack {
    let mut out = FilterStack::default();
    for slot in stack.slots() {
        let _ = out.push(FilterSlot {
            id: Uuid::new_v4(),
            filter: slot.filter.clone(),
            enabled: slot.enabled,
            intensity: slot.intensity,
            mask: slot.mask.clone(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Engine input id for a real source.
/// Name-based camera ↔ audio device pairing (see `auto_camera_audio`).
pub(crate) fn match_camera_audio(camera: &str, devices: &[String]) -> Option<String> {
    const GENERIC: &[&str] = &[
        "video", "audio", "capture", "device", "camera", "webcam", "usb", "hd", "digital",
        "interface", "input", "line", "microphone", "mic", "the", "and", "with", "high",
        "definition", "integrated", "stream", "source", "in",
    ];
    let words = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= 3 && !GENERIC.contains(w))
            .map(str::to_string)
            .collect()
    };
    let cam = words(camera);
    if cam.is_empty() {
        return None;
    }
    devices
        .iter()
        .map(|d| (cam.iter().filter(|w| words(d).contains(w)).count(), d))
        .filter(|(n, _)| *n > 0)
        .max_by_key(|(n, _)| *n)
        .map(|(_, d)| d.clone())
}

#[cfg(test)]
mod camera_audio_tests {
    use super::match_camera_audio;

    #[test]
    fn pairs_capture_card_audio_by_name_only() {
        let devs = vec![
            "Microphone (Realtek(R) Audio)".to_string(),
            "Elgato HD60 X Audio".to_string(),
            "Digital Audio Interface (USB3.0 Capture)".to_string(),
        ];
        assert_eq!(
            match_camera_audio("Elgato HD60 X", &devs).as_deref(),
            Some("Elgato HD60 X Audio")
        );
        // A plain webcam with no matching audio device gets NO audio (the
        // user's own mic is added separately) — never the first mic.
        assert_eq!(match_camera_audio("Integrated Webcam", &devs), None);
        assert_eq!(
            match_camera_audio("Logitech BRIO", &["Microphone (Logitech BRIO)".to_string()]).as_deref(),
            Some("Microphone (Logitech BRIO)")
        );
    }
}

fn source_input_id(source_id: &str) -> String {
    format!("src-{source_id}")
}

fn parse_source_kind(
    kind: &str,
    color: [u8; 3],
    text: Option<&str>,
    path: Option<&str>,
    device: Option<&str>,
    audio_device: Option<&str>,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<SourceKind, ShellError> {
    match kind {
        "test_pattern" => Ok(SourceKind::TestPattern),
        "color" => Ok(SourceKind::Color {
            r: color[0],
            g: color[1],
            b: color[2],
        }),
        "camera" => Ok(SourceKind::Camera {
            device: device.map(|d| d.to_string()),
            width,
            height,
            fps,
            audio_device: audio_device.map(|d| d.to_string()),
        }),
        "screen" => Ok(SourceKind::Screen),
        "audio_input" => Ok(SourceKind::AudioInput {
            device: device.map(|d| d.to_string()),
        }),
        "media" => Ok(SourceKind::Media {
            path: path.unwrap_or("").to_string(),
        }),
        "text" => Ok(SourceKind::Text {
            text: text.unwrap_or("").to_string(),
        }),
        _ => Err(ShellError::UnknownSourceKind(kind.to_string())),
    }
}

impl Shell {
    /// Adds a source to a scene. `kind`: `test_pattern` | `color` |
    /// `camera` | `screen` | `media` | `text`. Real kinds get an engine
    /// input and render through the real pipeline; pending kinds report
    /// `status: "pending"` and are skipped in the composite.
    /// Adding a camera without an explicit audio device auto-attaches
    /// audio (see `auto_camera_audio`).
    /// Lists available audio input devices (Windows DirectShow; empty elsewhere).
    pub fn list_audio_devices(&self) -> Vec<String> {
        #[cfg(any(windows, sf_wincheck))]
        {
            sf_capture::dshow::list_audio_devices().unwrap_or_default()
        }
        #[cfg(not(any(windows, sf_wincheck)))]
        {
            Vec::new()
        }
    }

    /// Picks the audio device that belongs to a camera / capture card, by
    /// name. Only a device whose name shares a distinctive word with the
    /// camera's (e.g. "Elgato HD60 X" ↔ "Elgato HD60 X Audio") is chosen —
    /// never just "the first microphone", which used to double the audio
    /// when the user also added that microphone as its own source.
    pub fn auto_camera_audio(&self, camera_device: Option<&str>) -> Option<String> {
        match_camera_audio(camera_device?, &self.list_audio_devices())
    }

    /// Lists available camera devices (Windows DirectShow; empty elsewhere).
    pub fn list_cameras(&self) -> Vec<String> {
        #[cfg(any(windows, sf_wincheck))]
        {
            sf_capture::dshow::list_devices().unwrap_or_default()
        }
        #[cfg(not(any(windows, sf_wincheck)))]
        {
            Vec::new()
        }
    }

    pub fn source_add(
        &mut self,
        scene_id: &str,
        kind: &str,
        name: &str,
        color: [u8; 3],
        text: Option<&str>,
        path: Option<&str>,
        device: Option<&str>,
        audio_device: Option<&str>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<SourceState, ShellError> {
        let mut kind = parse_source_kind(kind, color, text, path, device, audio_device, width, height, fps)?;
        // "Video input → audio auto input": a camera added without an
        // explicit audio device automatically captures audio. Prefer an
        // audio device matching the camera/capture-card name, else the
        // first available input. No option to fiddle with — it just works.
        if let SourceKind::Camera {
            device: cam_dev,
            audio_device: cam_audio,
            ..
        } = &mut kind
        {
            if cam_audio.is_none() {
                *cam_audio = self.auto_camera_audio(cam_dev.as_deref());
            }
        }
        let id = Uuid::new_v4().to_string();
        // Open producers BEFORE the source is pushed, so a bad file or
        // missing camera fails cleanly without leaving a half-added source.
        if let SourceKind::Media { path } = &kind {
            self.open_media_producer(&id, path)?;
        }
        #[cfg(any(windows, sf_wincheck))]
        if let SourceKind::Camera {
            device,
            width,
            height,
            fps,
            audio_device,
        } = &kind
        {
            let (w, h) = if *width > 0 && *height > 0 {
                (*width, *height)
            } else {
                (1280, 720)
            };
            let f = if *fps == 30 || *fps == 60 { *fps } else { 30 };
            self.open_camera_producer(&id, device.as_deref(), audio_device.as_deref(), w, h, f)?;
        }
        #[cfg(any(windows, sf_wincheck))]
        if let SourceKind::AudioInput { device } = &kind {
            self.open_audio_producer(&id, device.as_deref())?;
        }
        #[cfg(any(windows, sf_wincheck))]
        if matches!(kind, SourceKind::Screen) {
            self.open_screen_producer(&id)?;
        }
        // Audio-only sources need no video input record.
        let input_id = (kind.is_real() && !matches!(kind, SourceKind::AudioInput { .. }))
            .then(|| source_input_id(&id));
        let order = self
            .find_scene(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?
            .sources
            .len() as u32;
        let source = SourceState {
            id: id.clone(),
            name: name.to_string(),
            status: kind.status().to_string(),
            kind,
            visible: true,
            order,
            input_id: input_id.clone(),
        };
        // End the scene borrow before touching the engine/mixer.
        self.find_scene_mut(scene_id)
            .expect("scene exists")
            .sources
            .push(source.clone());
        if let Some(input_id) = &input_id {
            self.engine.state_mut().add_input(input_id, name);
        }
        self.mixer.channels.insert(id, ChannelStrip::default());
        self.events.push_back(UiEvent::SourceChanged {
            scene_id: scene_id.to_string(),
        });
        Ok(source)
    }

    /// Removes a source from a scene (also drops its engine input record
    /// and mixer strip).
    pub fn source_remove(&mut self, scene_id: &str, source_id: &str) -> Result<(), ShellError> {
        let scene = self
            .find_scene_mut(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        let pos = scene
            .sources
            .iter()
            .position(|s| s.id == source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        let removed = scene.sources.remove(pos);
        // Renumber orders so the composite stays contiguous.
        for (i, s) in scene.sources.iter_mut().enumerate() {
            s.order = i as u32;
        }
        self.mixer.channels.remove(source_id);
        self.media_producers.remove(source_id);
        // Dropping the LiveInput signals its capture thread to stop (never
        // joins: a wedged driver can't hang this command).
        self.audio_inputs.remove(source_id);
        self.voice_enhancers.remove(source_id);
        self.audio_bus.remove(source_id);
        self.source_frames.remove(source_id);
        self.resized_frames.retain(|(id, _, _), _| id != source_id);
        self.camera_formats.remove(source_id);
        // Drop any cached text rasterizations for this source.
        self.text_cache.retain(|(sid, _, _, _), _| sid != source_id);
        self.meter_values.remove(source_id);
        if let Some(input_id) = removed.input_id {
            self.engine.state_mut().inputs.retain(|i| i.id != input_id);
        }
        self.events.push_back(UiEvent::SourceChanged {
            scene_id: scene_id.to_string(),
        });
        Ok(())
    }

    /// Drag-reorders a scene's sources: `order` holds source ids bottom to
    /// top and must contain exactly the scene's source ids.
    pub fn source_reorder(&mut self, scene_id: &str, order: Vec<String>) -> Result<(), ShellError> {
        let scene = self
            .find_scene_mut(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        if order.len() != scene.sources.len()
            || !order
                .iter()
                .all(|id| scene.sources.iter().any(|s| &s.id == id))
        {
            return Err(ShellError::UnknownSource(format!(
                "reorder list does not match scene '{scene_id}' sources"
            )));
        }
        let mut reordered = Vec::with_capacity(order.len());
        for (i, id) in order.iter().enumerate() {
            let mut src = scene
                .sources
                .iter()
                .find(|s| &s.id == id)
                .expect("validated above")
                .clone();
            src.order = i as u32;
            reordered.push(src);
        }
        scene.sources = reordered;
        self.events.push_back(UiEvent::SourceChanged {
            scene_id: scene_id.to_string(),
        });
        Ok(())
    }

    /// Shows or hides a source.
    pub fn source_set_visible(
        &mut self,
        scene_id: &str,
        source_id: &str,
        visible: bool,
    ) -> Result<SourceState, ShellError> {
        let scene = self
            .find_scene_mut(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        let src = scene
            .sources
            .iter_mut()
            .find(|s| s.id == source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        src.visible = visible;
        let src = src.clone();
        self.events.push_back(UiEvent::SourceChanged {
            scene_id: scene_id.to_string(),
        });
        Ok(src)
    }
}

// ---------------------------------------------------------------------------
// Scene render: real pipeline + CPU "over" composite
// ---------------------------------------------------------------------------

/// Raw pixels for a real source kind at the requested size.
fn source_pixels(kind: &SourceKind, width: u32, height: u32) -> Vec<u8> {
    match kind {
        SourceKind::TestPattern => synthetic_frame(width, height),
        SourceKind::Color { r, g, b } => {
            let n = width.max(1) as usize * height.max(1) as usize;
            let mut px = Vec::with_capacity(n * 4);
            for _ in 0..n {
                px.extend_from_slice(&[*r, *g, *b, 255]);
            }
            px
        }
        // Pending kinds never produce pixels — the composite skips them.
        _ => Vec::new(),
    }
}

impl Shell {
    /// Rasterized text overlay for a `Text` source, cached by
    /// (source_id, text, w, h) so the font rasterizer runs once per
    /// distinct text — not on every tick.
    pub(crate) fn text_pixels(&mut self, source_id: &str, text: &str, w: u32, h: u32) -> Vec<u8> {
        let key = (
            source_id.to_string(),
            text.to_string(),
            w.max(1),
            h.max(1),
        );
        if let Some(px) = self.text_cache.get(&key) {
            return px.clone();
        }
        let px = sf_graphics::render_text_rgba(text, w, h);
        // Bound the cache: text sources are few and static; 32 entries is
        // ample and keeps memory trivially small.
        if self.text_cache.len() >= 32 {
            self.text_cache.clear();
        }
        self.text_cache.insert(key, px.clone());
        px
    }
}

/// Alpha "over" composite of `src` onto `canvas` (both RGBA8, same size).
fn composite_over(canvas: &mut [u8], src: &[u8]) {
    for (d, s) in canvas.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let sa = s[3] as f32 / 255.0;
        let da = d[3] as f32 / 255.0;
        let oa = sa + da * (1.0 - sa);
        if oa > 0.0 {
            for c in 0..3 {
                d[c] = ((s[c] as f32 * sa + d[c] as f32 * da * (1.0 - sa)) / oa)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
            d[3] = (oa * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Linear crossfade between two same-size frames (`t` = 0 → `a`).
pub(crate) fn crossfade(a: &Frame, b: &Frame, t: f32) -> Frame {
    let t = t.clamp(0.0, 1.0);
    let pixels = a
        .pixels
        .iter()
        .zip(b.pixels.iter())
        .map(|(&x, &y)| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8)
        .collect();
    Frame {
        width: a.width,
        height: a.height,
        pixels,
    }
}

impl Shell {
    /// Renders a scene: every VISIBLE real source runs through the real
    /// `grade_input_frame` pipeline, then CPU "over"-composites bottom to
    /// top. Pending-kind sources are skipped (documented, never faked).
    pub(crate) fn render_scene_frame(
        &mut self,
        scene_id: &str,
        width: u32,
        height: u32,
    ) -> Result<Frame, ShellError> {
        let (w, h) = (width.max(1), height.max(1));
        // Clone: the engine calls below need `&mut self`.
        let mut sources = self
            .find_scene(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?
            .sources
            .clone();
        sources.retain(|s| {
            s.visible && s.kind.is_real() && !matches!(s.kind, SourceKind::AudioInput { .. })
        });
        sources.sort_by_key(|s| s.order);
        // Fast path: a single opaque video layer needs no composite — the
        // graded frame *is* the program frame (saves a full-frame blend
        // and a copy per tick).
        let single_opaque = sources.len() == 1
            && matches!(
                sources[0].kind,
                SourceKind::Media { .. } | SourceKind::Camera { .. } | SourceKind::Screen
            );
        let mut canvas: Option<Vec<u8>> = None;
        for src in &sources {
            let input_id = src
                .input_id
                .as_ref()
                .ok_or_else(|| ShellError::UnknownSource(src.id.clone()))?;
            let pixels: std::sync::Arc<Vec<u8>> = match &src.kind {
                SourceKind::Media { .. } | SourceKind::Camera { .. } | SourceKind::Screen => {
                    // The tick's cached frame (one pull per source per
                    // tick); skip the layer until the first frame arrives.
                    match self.pull_media_frame(&src.id, w, h) {
                        Some(px) => px,
                        None => continue,
                    }
                }
                SourceKind::Text { text } => {
                    std::sync::Arc::new(self.text_pixels(&src.id, text, w, h))
                }
                SourceKind::TestPattern => std::sync::Arc::new(self.test_pattern.frame(
                    w,
                    h,
                    self.video_settings.output_fps,
                    std::time::SystemTime::now(),
                )),
                _ => std::sync::Arc::new(source_pixels(&src.kind, w, h)),
            };
            let g0 = std::time::Instant::now();
            let frame = self.engine.grade_input_frame(input_id, w, h, &pixels)?;
            self.perf_gpu_us_accum += g0.elapsed().as_micros() as u64;
            // Color Studio live tap: hand over the exact graded frame that
            // goes to program/encoder (and the raw one for before/after).
            let watch = self.studio_watching(input_id);
            let out_pixels = if single_opaque {
                if watch.is_some() {
                    frame.pixels.clone()
                } else {
                    // Moved, no copy.
                    canvas = Some(frame.pixels);
                    continue;
                }
            } else {
                let base = canvas.take().unwrap_or_else(|| vec![0u8; w as usize * h as usize * 4]);
                let g1 = std::time::Instant::now();
                let blended = match self.engine.gpu_composite_over(&base, &frame.pixels, w, h) {
                    Ok(px) => px,
                    Err(_) => {
                        let mut b = base;
                        composite_over(&mut b, &frame.pixels);
                        b
                    }
                };
                self.perf_gpu_us_accum += g1.elapsed().as_micros() as u64;
                canvas = Some(blended);
                Vec::new()
            };
            if single_opaque {
                canvas = Some(out_pixels);
            }
            if let Some(mode) = watch {
                let raw = (mode != crate::color_library::PreviewMode::After).then(|| Frame {
                    width: w,
                    height: h,
                    pixels: std::sync::Arc::try_unwrap(pixels).unwrap_or_else(|a| (*a).clone()),
                });
                let input_id = input_id.clone();
                self.studio_tap_store(&input_id, frame, raw);
            }
        }
        Ok(Frame {
            width: w,
            height: h,
            pixels: canvas.unwrap_or_else(|| vec![0u8; w as usize * h as usize * 4]),
        })
    }

    /// PNG bytes for a scene's composite (the adapter wraps this in a PNG
    /// data URL for the `scene_preview_png` command).
    pub fn scene_render_png(
        &mut self,
        scene_id: &str,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, ShellError> {
        let frame = self.render_scene_frame(scene_id, width, height)?;
        encode_png(&frame)
    }

    /// JPEG bytes for a scene monitor. The program scene is served from the
    /// media tick's live monitor frame — no re-render, no camera re-pull,
    /// no mutex stall of the tick (the old path re-rendered at 2 Hz and
    /// jittered the pipeline on every poll). Other scenes render on demand
    /// at monitor size.
    pub fn scene_monitor_jpg(&mut self, scene_id: &str) -> Result<Vec<u8>, ShellError> {
        self.monitor_snapshot(scene_id)?.to_jpeg()
    }

    /// Program-monitor frame: during an auto transition this is the live
    /// crossfade of program→preview at the eased wall-clock progress;
    /// otherwise it is the program scene's composite.
    pub fn program_frame_png(&mut self, width: u32, height: u32) -> Result<Vec<u8>, ShellError> {
        let active = self.transition.as_ref().map(|t| {
            let elapsed = t.started.elapsed().as_millis() as u64;
            (
                t.from_scene.clone(),
                t.to_scene.clone(),
                transition_progress(elapsed, t.duration_ms),
            )
        });
        match active {
            Some((from, to, progress)) => {
                let a = self.render_scene_frame(&from, width, height)?;
                let b = self.render_scene_frame(&to, width, height)?;
                encode_png(&crossfade(&a, &b, progress))
            }
            None => {
                let program = self.program_scene_id.clone();
                self.scene_render_png(&program, width, height)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Preview / program + transitions
// ---------------------------------------------------------------------------

impl Shell {
    /// Full production snapshot for the UI.
    pub fn production_state(&self) -> ProductionStateView {
        ProductionStateView {
            scenes: self.scenes.clone(),
            preview_scene_id: self.preview_scene_id.clone(),
            program_scene_id: self.program_scene_id.clone(),
            transition: self.transition_view(),
            mixer: MixerView {
                master: self.mixer.master.clone(),
                channels: self.mixer.channels.clone(),
                meters_live: self.meters_live,
                meters: self
                    .mixer
                    .channels
                    .keys()
                    .map(|id| {
                        (
                            id.clone(),
                            self.meter_values.get(id).copied().unwrap_or(0.0),
                        )
                    })
                    .collect(),
                monitor_enabled: self.audio_monitor.is_some(),
                enhancer: self.enhancer_settings(),
                voice: self.voice_stats(),
                av_sync: self.av_sync.clone(),
            },
            stream_settings: self.stream_settings.clone(),
            stream_lifecycle: self.stream_lifecycle.clone(),
            record_settings: self.record_settings.clone(),
            record_lifecycle: self.record_lifecycle(),
            video_settings: self.video_settings.clone(),
            macros: self.macros.clone(),
        }
    }

    /// Sets the preview (standby) scene. Retargets an in-flight transition.
    pub fn production_set_preview(&mut self, scene_id: &str) -> Result<(), ShellError> {
        if self.find_scene(scene_id).is_none() {
            return Err(ShellError::UnknownScene(scene_id.to_string()));
        }
        self.preview_scene_id = scene_id.to_string();
        if let Some(t) = self.transition.as_mut() {
            t.to_scene = scene_id.to_string();
        }
        self.emit_scene_changed();
        Ok(())
    }

    /// Sets the program scene directly (cancels any in-flight transition).
    pub fn production_set_program(&mut self, scene_id: &str) -> Result<(), ShellError> {
        if self.find_scene(scene_id).is_none() {
            return Err(ShellError::UnknownScene(scene_id.to_string()));
        }
        self.transition = None;
        self.program_scene_id = scene_id.to_string();
        self.emit_scene_changed();
        Ok(())
    }

    /// Instant cut: program = preview. Emits `transition_done` + scene change.
    pub fn transition_cut(&mut self) -> Result<(), ShellError> {
        self.transition = None;
        self.program_scene_id = self.preview_scene_id.clone();
        let program_scene_id = self.program_scene_id.clone();
        self.events
            .push_back(UiEvent::TransitionDone { program_scene_id });
        self.emit_scene_changed();
        Ok(())
    }

    /// Starts an auto transition (`kind`: `"mix"` only today). The Tauri
    /// adapter's 66 ms poll loop calls [`Shell::transition_tick`] to advance
    /// it; completion commits program = preview.
    pub fn transition_auto(
        &mut self,
        duration_ms: u64,
        kind: &str,
    ) -> Result<TransitionView, ShellError> {
        let kind = match kind {
            "mix" => TransitionKind::Mix,
            _ => return Err(ShellError::UnknownTransitionKind(kind.to_string())),
        };
        if self.transition.is_some() {
            return Err(ShellError::Transition(
                "a transition is already in progress".to_string(),
            ));
        }
        self.transition = Some(ActiveTransition {
            from_scene: self.program_scene_id.clone(),
            to_scene: self.preview_scene_id.clone(),
            started: Instant::now(),
            duration_ms: duration_ms.max(1),
            kind,
        });
        Ok(self.transition_view())
    }

    /// Advances the in-flight transition by wall-clock elapsed (smoothstep
    /// easing). Emits `transition_progress` per tick while active; on
    /// completion commits program = preview and emits `transition_done`.
    /// Called by the Tauri adapter's poll loop before `drain_events()`.
    pub fn transition_tick(&mut self) -> TransitionView {
        let active = match self.transition.take() {
            None => {
                return TransitionView {
                    active: false,
                    progress: 0.0,
                    kind: String::new(),
                    duration_ms: 0,
                }
            }
            Some(t) => t,
        };
        let elapsed = active.started.elapsed().as_millis() as u64;
        let progress = transition_progress(elapsed, active.duration_ms);
        if progress >= 1.0 {
            self.program_scene_id = active.to_scene.clone();
            let program_scene_id = self.program_scene_id.clone();
            self.events
                .push_back(UiEvent::TransitionDone { program_scene_id });
            self.emit_scene_changed();
            TransitionView {
                active: false,
                progress: 1.0,
                kind: String::new(),
                duration_ms: 0,
            }
        } else {
            self.events
                .push_back(UiEvent::TransitionProgress { progress });
            let view = TransitionView {
                active: true,
                progress,
                kind: match active.kind {
                    TransitionKind::Mix => "mix".to_string(),
                },
                duration_ms: active.duration_ms,
            };
            // The original start time is kept: progress is wall-clock.
            self.transition = Some(active);
            view
        }
    }
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

impl Shell {
    /// Sets a source channel gain, clamped to `0.0..=2.0`.
    pub fn mixer_set_gain(
        &mut self,
        source_id: &str,
        gain: f32,
    ) -> Result<ChannelStrip, ShellError> {
        let strip = self
            .mixer
            .channels
            .get_mut(source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        strip.gain = gain.clamp(0.0, 2.0);
        let strip = strip.clone();
        self.events.push_back(UiEvent::MixerChanged);
        Ok(strip)
    }

    /// Mutes/unmutes a source channel.
    pub fn mixer_set_mute(
        &mut self,
        source_id: &str,
        mute: bool,
    ) -> Result<ChannelStrip, ShellError> {
        let strip = self
            .mixer
            .channels
            .get_mut(source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        strip.mute = mute;
        let strip = strip.clone();
        self.events.push_back(UiEvent::MixerChanged);
        Ok(strip)
    }

    /// Sets the master gain, clamped to `0.0..=2.0`.
    pub fn mixer_set_master_gain(&mut self, gain: f32) -> ChannelStrip {
        self.mixer.master.gain = gain.clamp(0.0, 2.0);
        self.events.push_back(UiEvent::MixerChanged);
        self.mixer.master.clone()
    }

    /// Mutes/unmutes the master.
    pub fn mixer_set_master_mute(&mut self, mute: bool) -> ChannelStrip {
        self.mixer.master.mute = mute;
        self.events.push_back(UiEvent::MixerChanged);
        self.mixer.master.clone()
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

fn parse_stream_platform(platform: &str) -> Result<StreamPlatform, ShellError> {
    match platform {
        "youtube" => Ok(StreamPlatform::Youtube),
        "tiktok" => Ok(StreamPlatform::Tiktok),
        "facebook" => Ok(StreamPlatform::Facebook),
        "custom_rtmp" => Ok(StreamPlatform::CustomRtmp),
        _ => Err(ShellError::UnknownStreamPlatform(platform.to_string())),
    }
}

impl Shell {
    /// Stores stream settings. The key is kept in memory only — never
    /// logged, never serialized, never in error strings.
    pub fn stream_set_settings(
        &mut self,
        url: &str,
        key: &str,
        platform: &str,
    ) -> Result<(), ShellError> {
        let platform = parse_stream_platform(platform)?;
        self.stream_settings = StreamSettings {
            url: url.to_string(),
            key: key.to_string(),
            platform,
        };
        self.emit_stream_state();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Video input/output settings (Video menu panel)
// ---------------------------------------------------------------------------

/// Camera source summary for the Video panel's input section.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CameraSourceInfo {
    /// Source id (for `camera_reconfigure`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Device name (None = first available).
    pub device: Option<String>,
    /// Configured capture width.
    pub width: u32,
    /// Configured capture height.
    pub height: u32,
    /// Configured capture fps.
    pub fps: u32,
    /// What the camera actually negotiated (None = not opened / pending).
    pub negotiated: Option<String>,
}

impl Shell {
    /// Current video settings for the Video panel.
    pub fn video_settings_get(&self) -> VideoSettings {
        self.video_settings.clone()
    }

    /// All camera sources across scenes, with their live config and
    /// negotiated format. Powers the Video panel input section.
    pub fn camera_sources(&self) -> Vec<CameraSourceInfo> {
        self.scenes
            .iter()
            .flat_map(|sc| sc.sources.iter())
            .filter_map(|s| match &s.kind {
                SourceKind::Camera {
                    device,
                    width,
                    height,
                    fps,
                    ..
                } => Some(CameraSourceInfo {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    device: device.clone(),
                    width: *width,
                    height: *height,
                    fps: *fps,
                    negotiated: self.camera_formats.get(&s.id).cloned(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Validates and stores video settings. Output size/fps/encoder apply
    /// the next time streaming/recording starts (encoders are opened at
    /// start); the media-tick cadence follows `output_fps` live.
    pub fn video_settings_set(&mut self, s: VideoSettings) -> Result<(), ShellError> {
        if !VIDEO_OUTPUT_SIZES.contains(&(s.output_width, s.output_height)) {
            return Err(ShellError::InvalidSetting(format!(
                "unsupported output size {}x{}",
                s.output_width, s.output_height
            )));
        }
        if !VIDEO_FPS_OPTIONS.contains(&s.output_fps) {
            return Err(ShellError::InvalidSetting(format!(
                "unsupported output fps {}",
                s.output_fps
            )));
        }
        if !VIDEO_ENCODER_OPTIONS.contains(&s.encoder.as_str()) {
            return Err(ShellError::InvalidSetting(format!(
                "unknown encoder '{}'",
                s.encoder
            )));
        }
        if !(500..=50_000).contains(&s.bitrate_kbps) {
            return Err(ShellError::InvalidSetting(format!(
                "bitrate {} kbps out of range (500-50000)",
                s.bitrate_kbps
            )));
        }
        if !VIDEO_CAMERA_SIZES.contains(&(s.camera_width, s.camera_height)) {
            return Err(ShellError::InvalidSetting(format!(
                "unsupported camera size {}x{}",
                s.camera_width, s.camera_height
            )));
        }
        if !VIDEO_FPS_OPTIONS.contains(&s.camera_fps) {
            return Err(ShellError::InvalidSetting(format!(
                "unsupported camera fps {}",
                s.camera_fps
            )));
        }
        let fps_changed = s.output_fps != self.video_settings.output_fps;
        self.video_settings = s;
        // FPS applies live — even mid stream/record. The media-tick cadence
        // follows `output_fps` on the next tick; the encoders are rebuilt
        // here so the encoded output matches immediately. PTS stays
        // continuous: the new encoder anchors its first packet to the
        // running `video_pts_us` clock (see `VideoEncoder::encode_rgba`).
        // Other settings (size/encoder/bitrate) apply on next start.
        if fps_changed {
            self.reinit_video_encoders()?;
        }
        Ok(())
    }

    /// Media-tick frame interval in microseconds, derived from the output
    /// fps setting (30 → 33_333, 60 → 16_667).
    pub fn frame_interval_us(&self) -> u64 {
        1_000_000 / self.video_settings.output_fps.max(1) as u64
    }

    /// Reconfigures a camera source live: closes its producer, updates the
    /// stored kind, and reopens with the new device/resolution/fps.
    /// Returns what the camera actually negotiated (for the Video panel).
    /// On non-Windows there is no capture backend; the kind is still
    /// updated so the settings travel with the source.
    pub fn camera_reconfigure(
        &mut self,
        source_id: &str,
        device: Option<String>,
        width: u32,
        height: u32,
        fps: u32,
        audio_device: Option<String>,
    ) -> Result<String, ShellError> {
        let scene_idx = self
            .scenes
            .iter()
            .position(|sc| sc.sources.iter().any(|s| s.id == source_id))
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        // The caller passes the desired audio device (the Video panel's
        // dropdown is the source of truth; "No audio" = None).
        let existing_audio = audio_device;
        {
            let src = self.scenes[scene_idx]
                .sources
                .iter()
                .find(|s| s.id == source_id)
                .expect("source exists");
            if !matches!(&src.kind, SourceKind::Camera { .. }) {
                return Err(ShellError::InvalidSetting(format!(
                    "source '{}' is not a camera source",
                    src.name,
                )));
            }
        };
        {
            let src = self.scenes[scene_idx]
                .sources
                .iter_mut()
                .find(|s| s.id == source_id)
                .expect("source exists");
            src.kind = SourceKind::Camera {
                device: device.clone(),
                width,
                height,
                fps,
                audio_device: existing_audio.clone(),
            };
        }
        // Drop the old producer + negotiated-format entry before reopening.
        self.media_producers.remove(source_id);
        self.camera_formats.remove(source_id);
        #[cfg(any(windows, sf_wincheck))]
        {
            let (w, h) = if width > 0 && height > 0 {
                (width, height)
            } else {
                (1280, 720)
            };
            let f = if fps == 30 || fps == 60 { fps } else { 30 };
            self.open_camera_producer(
                source_id,
                device.as_deref(),
                existing_audio.as_deref(),
                w,
                h,
                f,
            )?;
        }
        Ok(self
            .camera_formats
            .get(source_id)
            .cloned()
            .unwrap_or_else(|| "camera backend not available on this platform".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

fn parse_record_format(format: &str) -> Result<RecordFormat, ShellError> {
    match format {
        "mp4" => Ok(RecordFormat::Mp4),
        "mkv" => Ok(RecordFormat::Mkv),
        _ => Err(ShellError::UnknownRecordFormat(format.to_string())),
    }
}

impl Shell {
    /// Stores record settings.
    pub fn record_set_settings(
        &mut self,
        path: &str,
        format: &str,
        quality: &str,
    ) -> Result<(), ShellError> {
        let format = parse_record_format(format)?;
        self.record_settings = RecordSettings {
            path: path.to_string(),
            format,
            quality: quality.to_string(),
        };
        self.emit_record_state();
        Ok(())
    }

    /// Pauses an active recording (keeps elapsed time).
    pub fn record_pause(&mut self) -> Result<(), ShellError> {
        match &mut self.record_runtime {
            Some(r) if !r.paused => {
                r.pause();
                self.emit_record_state();
                Ok(())
            }
            _ => Err(ShellError::Record(
                "no active recording to pause".to_string(),
            )),
        }
    }

    /// Resumes a paused recording.
    pub fn record_resume(&mut self) -> Result<(), ShellError> {
        match &mut self.record_runtime {
            Some(r) if r.paused => {
                r.resume();
                self.emit_record_state();
                Ok(())
            }
            _ => Err(ShellError::Record(
                "no paused recording to resume".to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

impl Shell {
    /// Binds a macro to a hotkey (hotkeys must be unique).
    pub fn macro_add(
        &mut self,
        name: &str,
        hotkey: &str,
        action: MacroAction,
    ) -> Result<Macro, ShellError> {
        if self.macros.iter().any(|m| m.hotkey == hotkey) {
            return Err(ShellError::Macro(format!(
                "hotkey '{hotkey}' is already bound"
            )));
        }
        let m = Macro {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            hotkey: hotkey.to_string(),
            action,
        };
        self.macros.push(m.clone());
        Ok(m)
    }

    /// Removes a macro by id.
    pub fn macro_remove(&mut self, macro_id: &str) -> Result<(), ShellError> {
        let pos = self
            .macros
            .iter()
            .position(|m| m.id == macro_id)
            .ok_or_else(|| ShellError::Macro(format!("unknown macro '{macro_id}'")))?;
        self.macros.remove(pos);
        Ok(())
    }

    /// All bound macros.
    pub fn macro_list(&self) -> Vec<Macro> {
        self.macros.clone()
    }

    /// Executes the macro bound to `hotkey` against the state machine and
    /// reports what happened.
    pub fn macro_dispatch(&mut self, hotkey: &str) -> Result<MacroDispatchResult, ShellError> {
        let m = self
            .macros
            .iter()
            .find(|m| m.hotkey == hotkey)
            .cloned()
            .ok_or_else(|| ShellError::UnknownHotkey(hotkey.to_string()))?;
        let happened = match &m.action {
            MacroAction::Cut => {
                self.transition_cut()?;
                format!("cut: program is now '{}'", self.program_scene_id)
            }
            MacroAction::AutoTransition { duration_ms } => {
                self.transition_auto(*duration_ms, "mix")?;
                format!("auto transition started ({duration_ms} ms mix)")
            }
            MacroAction::ToggleStream => match self.stream_lifecycle {
                StreamLifecycle::Live
                | StreamLifecycle::Connecting
                | StreamLifecycle::Reconnecting => {
                    self.stream_stop()?;
                    "stream stopped".to_string()
                }
                _ => match self.stream_start() {
                    Ok(()) => "stream started".to_string(),
                    Err(e) => format!("stream start failed: {e}"),
                },
            },
            MacroAction::ToggleRecord => {
                let running = matches!(
                    self.record_lifecycle(),
                    RecordLifecycle::Recording { .. } | RecordLifecycle::Paused { .. }
                );
                if running {
                    self.record_stop()?;
                    "recording stopped".to_string()
                } else {
                    match self.record_start() {
                        Ok(()) => "recording started".to_string(),
                        Err(e) => format!("record start failed: {e}"),
                    }
                }
            }
            MacroAction::ApplyLook { look_id, intensity } => {
                // Live look-switch: apply to every visible real source of
                // the program scene, through the real look pipeline.
                let program = self.program_scene_id.clone();
                let sources = self
                    .find_scene(&program)
                    .map(|s| s.sources.clone())
                    .unwrap_or_default();
                let mut applied = 0;
                for src in sources.iter().filter(|s| s.visible && s.kind.is_real()) {
                    if let Some(input_id) = &src.input_id {
                        self.look_apply(
                            FilterTarget::Input(input_id.clone()),
                            look_id,
                            *intensity,
                        )?;
                        applied += 1;
                    }
                }
                format!("look '{look_id}' applied to {applied} program source(s)")
            }
        };
        Ok(MacroDispatchResult {
            macro_id: m.id,
            hotkey: m.hotkey,
            happened,
        })
    }
}
