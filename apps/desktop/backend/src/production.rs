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
        /// Device format to ask for first (`mjpeg`, `h264`, `yuyv422`,
        /// `nv12`…; None = automatic: MJPEG, then raw).
        #[serde(default)]
        format: Option<String>,
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
    /// Still image (PNG / JPEG / WebP / BMP …): logo, watermark, overlay,
    /// background. Decoded once; transparency preserved. Not a video.
    Image {
        /// File path.
        path: String,
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
            | SourceKind::Image { .. }
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
    /// Position / size / crop in the canvas. `None` = full frame.
    #[serde(default)]
    pub transform: Option<SourceTransform>,
    /// Locked: position/size/crop can't be changed (layouts skip it).
    #[serde(default)]
    pub locked: bool,
}

/// How a source fills its box when the aspect ratios differ.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    /// Cover the box, cropping the overflow (centre).
    #[default]
    Fill,
    /// Whole picture inside the box (transparent bars).
    Fit,
    /// Distort to the box.
    Stretch,
}

/// A source's placement in the canvas. Box and crop are fractions (0–1) of
/// the canvas and of the source picture, so they survive a resolution
/// change.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceTransform {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub crop_left: f32,
    #[serde(default)]
    pub crop_top: f32,
    #[serde(default)]
    pub crop_right: f32,
    #[serde(default)]
    pub crop_bottom: f32,
    #[serde(default)]
    pub fit: FitMode,
    #[serde(default = "one")]
    pub opacity: f32,
    /// Clockwise rotation in degrees around the box centre.
    #[serde(default)]
    pub rotation: f32,
}

fn one() -> f32 {
    1.0
}

impl SourceTransform {
    pub const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
        crop_left: 0.0,
        crop_top: 0.0,
        crop_right: 0.0,
        crop_bottom: 0.0,
        fit: FitMode::Fill,
        opacity: 1.0,
        rotation: 0.0,
    };

    pub fn rect(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h, ..Self::FULL }
    }

    /// Identity placement (full canvas, no crop, opaque) — takes the fast
    /// full-frame path.
    pub fn is_full(&self) -> bool {
        let z = |v: f32| v.abs() < 1e-4;
        z(self.x) && z(self.y) && z(self.w - 1.0) && z(self.h - 1.0)
            && z(self.crop_left) && z(self.crop_top) && z(self.crop_right) && z(self.crop_bottom)
            && z(self.opacity - 1.0)
            && z(self.rotation)
    }

    /// Clamped copy (box inside a sane range, crop leaves ≥ 2 % visible).
    pub fn sanitized(mut self) -> Self {
        let f = |v: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { lo };
        self.w = f(self.w, 0.01, 4.0);
        self.h = f(self.h, 0.01, 4.0);
        self.x = f(self.x, -self.w, 1.0);
        self.y = f(self.y, -self.h, 1.0);
        self.crop_left = f(self.crop_left, 0.0, 0.98);
        self.crop_right = f(self.crop_right, 0.0, 0.98 - self.crop_left);
        self.crop_top = f(self.crop_top, 0.0, 0.98);
        self.crop_bottom = f(self.crop_bottom, 0.0, 0.98 - self.crop_top);
        self.opacity = f(self.opacity, 0.0, 1.0);
        self.rotation = if self.rotation.is_finite() {
            let r = self.rotation.rem_euclid(360.0);
            if r > 180.0 { r - 360.0 } else { r }
        } else {
            0.0
        };
        self
    }
}

/// Where a placed source lands: the content rectangle in canvas pixels and
/// the full scaled-source size + offset whose sub-rectangle fills it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placement {
    /// Content rect in canvas pixels (may extend outside the canvas).
    pub cx: i64,
    pub cy: i64,
    pub cw: u32,
    pub ch: u32,
    /// Size to pull/scale the whole source to.
    pub sw: u32,
    pub sh: u32,
    /// Offset of the content inside that scaled source.
    pub ox: i64,
    pub oy: i64,
}

/// Layout math for a placed source of native size `src_w`×`src_h` on a
/// `canvas_w`×`canvas_h` canvas.
pub(crate) fn placement(t: &SourceTransform, canvas_w: u32, canvas_h: u32, src_w: u32, src_h: u32) -> Placement {
    let t = t.sanitized();
    let (cwf, chf) = (canvas_w as f64, canvas_h as f64);
    let (sw, sh) = (src_w.max(1) as f64, src_h.max(1) as f64);
    // Box in canvas px.
    let (bx, by, bw, bh) = (t.x as f64 * cwf, t.y as f64 * chf, (t.w as f64 * cwf).max(1.0), (t.h as f64 * chf).max(1.0));
    // Crop window in source fractions.
    let (mut l, mut tp) = (t.crop_left as f64, t.crop_top as f64);
    let (mut r, mut b) = (1.0 - t.crop_right as f64, 1.0 - t.crop_bottom as f64);
    let crop_aspect = ((r - l) * sw) / ((b - tp) * sh);
    let box_aspect = bw / bh;
    let (mut cx, mut cy, mut cw, mut ch) = (bx, by, bw, bh);
    match t.fit {
        FitMode::Stretch => {}
        FitMode::Fill => {
            if crop_aspect > box_aspect {
                // Too wide: narrow the crop window around its centre.
                let keep = (r - l) * box_aspect / crop_aspect;
                let mid = (l + r) / 2.0;
                l = mid - keep / 2.0;
                r = mid + keep / 2.0;
            } else {
                let keep = (b - tp) * crop_aspect / box_aspect;
                let mid = (tp + b) / 2.0;
                tp = mid - keep / 2.0;
                b = mid + keep / 2.0;
            }
        }
        FitMode::Fit => {
            if crop_aspect > box_aspect {
                ch = bw / crop_aspect;
                cy = by + (bh - ch) / 2.0;
            } else {
                cw = bh * crop_aspect;
                cx = bx + (bw - cw) / 2.0;
            }
        }
    }
    let scale_w = (cw / (r - l)).clamp(1.0, 4096.0);
    let scale_h = (ch / (b - tp)).clamp(1.0, 4096.0);
    Placement {
        cx: cx.round() as i64,
        cy: cy.round() as i64,
        cw: cw.round().max(1.0) as u32,
        ch: ch.round().max(1.0) as u32,
        sw: scale_w.round() as u32,
        sh: scale_h.round() as u32,
        ox: (l * scale_w).round() as i64,
        oy: (tp * scale_h).round() as i64,
    }
}

/// Rotated placement: every canvas pixel inside the rotated content box
/// samples the content bilinearly (straight alpha), then blends "over".
pub(crate) fn blit_rotated(canvas: &mut [u8], w: u32, h: u32, src: &[u8], p: &Placement, opacity: f32, rotation_deg: f32) {
    let (sw, sh) = (p.sw as i64, p.sh as i64);
    if src.len() < (sw * sh * 4) as usize || w == 0 || h == 0 {
        return;
    }
    let (cw, ch) = (p.cw as f32, p.ch as f32);
    let (mx, my) = (p.cx as f32 + cw / 2.0, p.cy as f32 + ch / 2.0);
    let (sin, cos) = rotation_deg.to_radians().sin_cos();
    // Bounding box of the rotated rectangle.
    let ex = (cw / 2.0 * cos).abs() + (ch / 2.0 * sin).abs();
    let ey = (cw / 2.0 * sin).abs() + (ch / 2.0 * cos).abs();
    let x0 = ((mx - ex).floor() as i64).max(0);
    let x1 = ((mx + ex).ceil() as i64).min(w as i64);
    let y0 = ((my - ey).floor() as i64).max(0);
    let y1 = ((my + ey).ceil() as i64).min(h as i64);
    let op = opacity.clamp(0.0, 1.0);
    let sample = |sx: f32, sy: f32| -> Option<[f32; 4]> {
        let fx = sx - 0.5;
        let fy = sy - 0.5;
        let ix = fx.floor() as i64;
        let iy = fy.floor() as i64;
        let (tx, ty) = (fx - ix as f32, fy - iy as f32);
        let mut acc = [0f32; 4];
        let mut any = false;
        for (dy, wy) in [(0, 1.0 - ty), (1, ty)] {
            for (dx, wx) in [(0, 1.0 - tx), (1, tx)] {
                let (xx, yy) = ((ix + dx).clamp(0, sw - 1), (iy + dy).clamp(0, sh - 1));
                let wgt = wx * wy;
                let i = ((yy * sw + xx) * 4) as usize;
                let a = src[i + 3] as f32 / 255.0;
                for c in 0..3 {
                    acc[c] += src[i + c] as f32 * a * wgt;
                }
                acc[3] += a * wgt;
                any = true;
            }
        }
        any.then_some(acc)
    };
    for y in y0..y1 {
        for x in x0..x1 {
            // Inverse-rotate the pixel centre into the content box.
            let (dx, dy) = (x as f32 + 0.5 - mx, y as f32 + 0.5 - my);
            let u = dx * cos + dy * sin + cw / 2.0;
            let v = -dx * sin + dy * cos + ch / 2.0;
            if u < 0.0 || v < 0.0 || u >= cw || v >= ch {
                continue;
            }
            let Some(acc) = sample(p.ox as f32 + u, p.oy as f32 + v) else { continue };
            let a = acc[3] * op;
            if a <= 0.0 {
                continue;
            }
            let i = ((y * w as i64 + x) * 4) as usize;
            let d = &mut canvas[i..i + 4];
            for c in 0..3 {
                let sc = acc[c] / acc[3].max(1e-6);
                d[c] = (sc * a + d[c] as f32 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
            }
            d[3] = d[3].max((a * 255.0).round() as u8);
        }
    }
}

/// Blits the placed content of `src` (`p.sw`×`p.sh` RGBA) into `canvas`
/// with `opacity`; `opaque` sources (cameras, files…) copy whole rows.
pub(crate) fn blit_placed(canvas: &mut [u8], w: u32, h: u32, src: &[u8], p: &Placement, opacity: f32, opaque: bool) {
    let (w, h) = (w as i64, h as i64);
    let (sw, sh) = (p.sw as i64, p.sh as i64);
    if src.len() < (sw * sh * 4) as usize {
        return;
    }
    let x0 = p.cx.max(0);
    let x1 = (p.cx + p.cw as i64).min(w);
    let y0 = p.cy.max(0);
    let y1 = (p.cy + p.ch as i64).min(h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let op = opacity.clamp(0.0, 1.0);
    for y in y0..y1 {
        let syy = p.oy + (y - p.cy);
        if syy < 0 || syy >= sh {
            continue;
        }
        let sx0 = p.ox + (x0 - p.cx);
        let sx1 = (sx0 + (x1 - x0)).min(sw);
        let sx0c = sx0.max(0);
        if sx0c >= sx1 {
            continue;
        }
        let dx0 = x0 + (sx0c - sx0);
        let n = (sx1 - sx0c) as usize;
        let s_off = ((syy * sw + sx0c) * 4) as usize;
        let d_off = ((y * w + dx0) * 4) as usize;
        let srow = &src[s_off..s_off + n * 4];
        let drow = &mut canvas[d_off..d_off + n * 4];
        if opaque && op >= 0.999 {
            drow.copy_from_slice(srow);
            continue;
        }
        for (d, s) in drow.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
            let a = s[3] as f32 / 255.0 * op;
            if a <= 0.0 {
                continue;
            }
            for c in 0..3 {
                d[c] = (s[c] as f32 * a + d[c] as f32 * (1.0 - a)).round() as u8;
            }
            d[3] = d[3].max((a * 255.0).round() as u8);
        }
    }
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

/// Supported transition kinds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    /// Cross-dissolve.
    Mix,
    /// Fade program to black, then black up to preview.
    Fade,
    /// Hard-edge wipe, left to right.
    Wipe,
}

impl TransitionKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mix" => Some(Self::Mix),
            "fade" => Some(Self::Fade),
            "wipe" => Some(Self::Wipe),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Mix => "mix",
            Self::Fade => "fade",
            Self::Wipe => "wipe",
        }
    }
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
    /// Linear gain `0.0..=2.0` (the fader).
    pub gain: f32,
    /// Mute flag.
    pub mute: bool,
    /// Balance `-1.0` (left) … `1.0` (right).
    #[serde(default)]
    pub pan: f32,
    /// Solo: the local monitor (speakers/headphones) plays only soloed
    /// channels. The program mix (stream/record) is never affected.
    #[serde(default)]
    pub solo: bool,
    /// Input trim in dB (`-20..=20`), before the fader.
    #[serde(default)]
    pub trim_db: f32,
    /// Effects chain of this channel (each effect switchable). Runs after
    /// the trim, before balance and the fader.
    #[serde(default)]
    pub fx: sf_audio::FxSettings,
}

impl Default for ChannelStrip {
    fn default() -> Self {
        Self {
            gain: 1.0,
            mute: false,
            pan: 0.0,
            solo: false,
            trim_db: 0.0,
            fx: sf_audio::FxSettings::default(),
        }
    }
}

impl ChannelStrip {
    /// Strip for a new source: voice sources (microphones, cameras with
    /// audio) start with the voice chain, everything else with no effects.
    pub fn for_kind(kind: &SourceKind) -> Self {
        let voice = matches!(kind, SourceKind::AudioInput { .. } | SourceKind::Camera { .. });
        Self {
            fx: if voice { sf_audio::FxSettings::voice() } else { sf_audio::FxSettings::default() },
            ..Self::default()
        }
    }
}

/// Balance (pan) + trim on an interleaved stereo block, in place.
pub(crate) fn apply_trim_pan(block: &mut [f32], trim_db: f32, pan: f32) {
    let trim = 10f32.powf(trim_db.clamp(-20.0, 20.0) / 20.0);
    let pan = pan.clamp(-1.0, 1.0);
    let (l, r) = (trim * (1.0 - pan).min(1.0), trim * (1.0 + pan).min(1.0));
    if (l - 1.0).abs() < 1e-6 && (r - 1.0).abs() < 1e-6 {
        return;
    }
    for fr in block.chunks_exact_mut(2) {
        fr[0] *= l;
        fr[1] *= r;
    }
}

/// Live meters (polled at display rate): peak held since the previous
/// read, so short transients are never missed between polls.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioMetersView {
    pub channels: Vec<ChannelMeterView>,
    pub master_peak: f32,
    pub master_rms: f32,
    /// True while any channel is soloed (monitor = solo bus).
    pub solo_active: bool,
    /// Live readings of the master effects.
    pub master_fx: sf_audio::FxStats,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ChannelMeterView {
    pub id: String,
    /// Max |sample| since the last read, post-fader, linear.
    pub peak: f32,
    /// RMS of the latest block, post-fader, linear.
    pub rms: f32,
    /// The channel has an audio stream (live input or file with audio).
    pub has_signal_path: bool,
    /// Live readings of the channel's effects (None = no effect running).
    pub fx: Option<sf_audio::FxStats>,
}

/// Mixer state: master strip + one strip per source id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MixerState {
    /// Master strip.
    pub master: ChannelStrip,
    /// Per-source strips keyed by source id.
    pub channels: HashMap<String, ChannelStrip>,
}

impl Default for MixerState {
    fn default() -> Self {
        Self {
            master: ChannelStrip {
                fx: sf_audio::FxSettings::master(),
                ..ChannelStrip::default()
            },
            channels: HashMap::new(),
        }
    }
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
    /// Live effects readings per source (gain reduction, gate, latency).
    #[serde(default)]
    pub fx_stats: HashMap<String, sf_audio::FxStats>,
    /// Live effects readings of the master bus.
    #[serde(default)]
    pub master_fx_stats: sf_audio::FxStats,
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
    /// Program output width (see `VIDEO_OUTPUT_SIZES`: 640 … 2560).
    pub output_width: u32,
    /// Program output height (360 … 1440).
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
/// Output (stream + recording) sizes: 360p … 1440p, all 16:9 with even
/// dimensions (H.264 needs even sizes).
pub const VIDEO_OUTPUT_SIZES: [(u32, u32); 6] = [
    (640, 360),
    (854, 480),
    (960, 540),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
];
/// Default camera request sizes (a camera's own mode list is offered per
/// device; see `camera_modes`).
pub const VIDEO_CAMERA_SIZES: [(u32, u32); 8] = [
    (640, 360),
    (640, 480),
    (854, 480),
    (960, 540),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
pub const VIDEO_FPS_OPTIONS: [u32; 2] = [30, 60];
/// Camera input frame rates that can be requested.
pub const CAMERA_FPS_OPTIONS: [u32; 6] = [15, 24, 25, 30, 50, 60];
/// Stream/record bitrate range, kbps.
pub const BITRATE_RANGE_KBPS: (u32, u32) = (300, 60_000);
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
                    kind: t.kind.name().to_string(),
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
    /// Moves a scene to position `to` in the list (operator ordering;
    /// keys 1–9 follow it).
    pub fn scene_move(&mut self, scene_id: &str, to: usize) -> Result<(), ShellError> {
        let from = self
            .scenes
            .iter()
            .position(|s| s.id == scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        let sc = self.scenes.remove(from);
        let to = to.min(self.scenes.len());
        self.scenes.insert(to, sc);
        self.emit_scene_changed();
        Ok(())
    }

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
            // Show the same capture (camera/screen/file) — never reopen it.
            if matches!(s.kind, SourceKind::Camera { .. } | SourceKind::Screen | SourceKind::Media { .. }) {
                let owner = self.frame_owner(&s.id).to_string();
                self.producer_alias.insert(new_id.clone(), owner);
            }
            let input_id = s.kind.is_real().then(|| source_input_id(&new_id));
            if let Some(input_id) = &input_id {
                self.engine.state_mut().add_input(input_id, &s.name);
            }
            let strip = self.mixer.channels.get(&s.id).cloned().unwrap_or_else(|| ChannelStrip::for_kind(&s.kind));
            self.mixer.channels.insert(new_id.clone(), strip);
            dup.sources.push(SourceState {
                id: new_id,
                name: s.name.clone(),
                kind: s.kind.clone(),
                visible: s.visible,
                order: s.order,
                input_id,
                status: s.kind.status().to_string(),
                transform: s.transform,
                locked: s.locked,
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
            format: None,
        }),
        "screen" => Ok(SourceKind::Screen),
        "audio_input" => Ok(SourceKind::AudioInput {
            device: device.map(|d| d.to_string()),
        }),
        "media" => Ok(SourceKind::Media {
            path: path.unwrap_or("").to_string(),
        }),
        "image" => Ok(SourceKind::Image {
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
        // The same camera / screen already captured by another source (in
        // any scene) is shared, not opened twice (a device opens once).
        let shared = self.shared_producer_for(&kind);
        if let Some(owner) = &shared {
            self.producer_alias.insert(id.clone(), owner.clone());
        }
        // Open producers BEFORE the source is pushed, so a bad file or
        // missing camera fails cleanly without leaving a half-added source.
        if let SourceKind::Media { path } = &kind {
            self.open_media_producer(&id, path)?;
        }
        // Images load (and fail) up front; they start at their native
        // pixel size relative to the output, centred.
        let mut transform = None;
        if let SourceKind::Image { path } = &kind {
            let img = self.load_image(path)?;
            let (ow, oh) = (self.video_settings.output_width.max(1) as f32, self.video_settings.output_height.max(1) as f32);
            let (w, h) = (img.width as f32 / ow, img.height as f32 / oh);
            let k = (1.0 / w.max(h)).min(1.0);
            let (w, h) = (w * k, h * k);
            let t = SourceTransform {
                fit: FitMode::Fit,
                ..SourceTransform::rect((1.0 - w) / 2.0, (1.0 - h) / 2.0, w, h)
            };
            transform = (!t.is_full()).then_some(t);
        }
        #[cfg(any(windows, sf_wincheck))]
        if let SourceKind::Camera {
            device,
            width,
            height,
            fps,
            audio_device,
            format,
        } = &kind
        {
            let (w, h) = if *width > 0 && *height > 0 {
                (*width, *height)
            } else {
                (1280, 720)
            };
            let f = camera_fps_or_default(*fps);
            if shared.is_none() {
                self.open_camera_producer(&id, device.as_deref(), audio_device.as_deref(), w, h, f, format.as_deref())?;
            }
        }
        #[cfg(any(windows, sf_wincheck))]
        if let SourceKind::AudioInput { device } = &kind {
            self.open_audio_producer(&id, device.as_deref())?;
        }
        #[cfg(any(windows, sf_wincheck))]
        if matches!(kind, SourceKind::Screen) && shared.is_none() {
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
            transform,
            locked: false,
        };
        // End the scene borrow before touching the engine/mixer.
        self.find_scene_mut(scene_id)
            .expect("scene exists")
            .sources
            .push(source.clone());
        if let Some(input_id) = &input_id {
            self.engine.state_mut().add_input(input_id, name);
        }
        let strip = ChannelStrip::for_kind(&source.kind);
        self.mixer.channels.insert(id, strip);
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
        // Shared captures: an alias just unlinks; an owner still shown by
        // other scenes keeps capturing until its last user is removed.
        let owner = self.producer_alias.remove(source_id);
        let still_used = |me: &Self, o: &str| me.producer_alias.values().any(|v| v == o);
        let mut release: Vec<String> = Vec::new();
        match owner {
            Some(o) => {
                if self.orphan_owners.contains(&o) && !still_used(self, &o) {
                    self.orphan_owners.remove(&o);
                    release.push(o);
                }
            }
            None => {
                if still_used(self, source_id) {
                    self.orphan_owners.insert(source_id.to_string());
                } else {
                    release.push(source_id.to_string());
                }
            }
        }
        if let Some(input_id) = &removed.input_id {
            self.last_frame.remove(input_id);
            self.preview_cache.remove(input_id);
        }
        if let SourceKind::Image { path } = &removed.kind {
            let still_used = self
                .scenes
                .iter()
                .flat_map(|s| s.sources.iter())
                .any(|s| matches!(&s.kind, SourceKind::Image { path: p } if p == path));
            if !still_used {
                self.image_cache.remove(path);
                self.image_sized.retain(|(p, _, _), _| p != path);
            }
        }
        for id in &release {
            self.media_producers.remove(id);
            // Dropping the LiveInput signals its capture thread to stop
            // (never joins: a wedged driver can't hang this command).
            self.audio_inputs.remove(id);
            self.channel_fx.remove(id);
            self.audio_bus.remove(id);
            self.source_frames.remove(id);
            self.resized_frames.retain(|(rid, _, _), _| rid != id);
            self.camera_formats.remove(id);
        }
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

impl Shell {
    /// Loads (once per path) a still image for an Image source.
    pub(crate) fn load_image(&mut self, path: &str) -> Result<std::sync::Arc<sf_capture::still_image::StillImage>, ShellError> {
        if let Some(img) = self.image_cache.get(path) {
            return Ok(img.clone());
        }
        if path.trim().is_empty() {
            return Err(ShellError::Capture("no image selected — pick a PNG, JPG or WebP file".into()));
        }
        let img = sf_capture::still_image::decode_image_rgba(path)
            .map_err(|e| ShellError::Capture(e.to_string()))?;
        let img = std::sync::Arc::new(img);
        self.image_cache.insert(path.to_string(), img.clone());
        Ok(img)
    }

    /// The image at `w`×`h` (premultiplied-alpha resize, cached per size).
    pub(crate) fn image_pixels(&mut self, path: &str, w: u32, h: u32) -> Option<std::sync::Arc<Vec<u8>>> {
        let key = (path.to_string(), w, h);
        if let Some(px) = self.image_sized.get(&key) {
            return Some(px.clone());
        }
        let img = self.load_image(path).ok()?;
        let px = if img.width == w && img.height == h {
            img.data.clone()
        } else {
            sf_capture::still_image::resize_rgba_premultiplied(&img.data, img.width, img.height, w, h)
        };
        // Bounded: ≤ 8 sizes and ≤ 64 MB of resized images in memory.
        let bytes: usize = self.image_sized.values().map(|p| p.len()).sum();
        if self.image_sized.len() >= 8 || bytes + px.len() > 64 << 20 {
            self.image_sized.clear();
        }
        let px = std::sync::Arc::new(px);
        self.image_sized.insert(key, px.clone());
        Some(px)
    }

    /// Locks / unlocks a source's placement.
    pub fn source_set_locked(&mut self, scene_id: &str, source_id: &str, locked: bool) -> Result<SourceState, ShellError> {
        let scene = self
            .find_scene_mut(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        let src = scene
            .sources
            .iter_mut()
            .find(|s| s.id == source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        src.locked = locked;
        let src = src.clone();
        self.events.push_back(UiEvent::SourceChanged { scene_id: scene_id.to_string() });
        Ok(src)
    }

    /// Duplicates a source inside its scene (placed on top, nudged 3 %).
    /// Cameras / screens / files share the original capture; the grade and
    /// filters are copied.
    pub fn source_duplicate(&mut self, scene_id: &str, source_id: &str) -> Result<SourceState, ShellError> {
        let orig = self
            .find_scene(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?
            .sources
            .iter()
            .find(|s| s.id == source_id)
            .cloned()
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        if matches!(orig.kind, SourceKind::AudioInput { .. }) {
            return Err(ShellError::InvalidSetting("a microphone can't be duplicated — add it once and adjust it in the mixer".into()));
        }
        let id = Uuid::new_v4().to_string();
        if matches!(orig.kind, SourceKind::Camera { .. } | SourceKind::Screen | SourceKind::Media { .. }) {
            let owner = self.frame_owner(&orig.id).to_string();
            self.producer_alias.insert(id.clone(), owner);
        }
        let input_id = orig.input_id.as_ref().map(|_| source_input_id(&id));
        let order = self.find_scene(scene_id).map(|s| s.sources.len() as u32).unwrap_or(0);
        let transform = orig.transform.map(|mut t| {
            t.x += 0.03;
            t.y += 0.03;
            t
        });
        let dup = SourceState {
            id: id.clone(),
            name: format!("{} copy", orig.name),
            kind: orig.kind.clone(),
            visible: orig.visible,
            order,
            input_id: input_id.clone(),
            status: orig.status.clone(),
            transform,
            locked: false,
        };
        if let (Some(new_in), Some(old_in)) = (&input_id, &orig.input_id) {
            let st = self.engine.state_mut();
            st.add_input(new_in, &dup.name);
            let copied = st.inputs.iter().find(|i| &i.id == old_in).map(|i| (i.color.clone(), i.filters.clone()));
            if let (Some((color, filters)), Some(n)) = (copied, st.inputs.iter_mut().find(|i| &i.id == new_in)) {
                n.color = color;
                n.filters = filters;
            }
        }
        let strip = ChannelStrip {
            fx: self.mixer.channels.get(&orig.id).map(|c| c.fx.clone()).unwrap_or_else(|| ChannelStrip::for_kind(&orig.kind).fx),
            ..ChannelStrip::default()
        };
        self.mixer.channels.insert(id, strip);
        self.find_scene_mut(scene_id).expect("scene exists").sources.push(dup.clone());
        self.events.push_back(UiEvent::SourceChanged { scene_id: scene_id.to_string() });
        Ok(dup)
    }

    /// Places a source in the canvas (`None` = full frame).
    pub fn source_set_transform(
        &mut self,
        scene_id: &str,
        source_id: &str,
        transform: Option<SourceTransform>,
    ) -> Result<SourceState, ShellError> {
        let scene = self
            .find_scene_mut(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        let src = scene
            .sources
            .iter_mut()
            .find(|s| s.id == source_id)
            .ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        if src.locked {
            return Err(ShellError::InvalidSetting(format!("'{}' is locked — unlock it to move or resize", src.name)));
        }
        src.transform = transform.map(|t| t.sanitized()).filter(|t| !t.is_full());
        let src = src.clone();
        self.events.push_back(UiEvent::SourceChanged {
            scene_id: scene_id.to_string(),
        });
        Ok(src)
    }

    /// Arranges the scene's visible video sources (bottom-first order) in a
    /// preset layout: `full`, `side_by_side`, `side_by_side_fit`, `split_v`,
    /// `pip_tl` / `pip_tr` / `pip_bl` / `pip_br`, `grid`. Returns how many
    /// sources were placed.
    pub fn scene_apply_layout(&mut self, scene_id: &str, layout: &str) -> Result<usize, ShellError> {
        let rects = layout_rects(layout)
            .ok_or_else(|| ShellError::InvalidSetting(format!("unknown layout '{layout}'")))?;
        let scene = self
            .find_scene_mut(scene_id)
            .ok_or_else(|| ShellError::UnknownScene(scene_id.to_string()))?;
        let mut video: Vec<&mut SourceState> = scene
            .sources
            .iter_mut()
            .filter(|s| {
                s.visible
                    && !s.locked
                    && matches!(
                        s.kind,
                        SourceKind::Camera { .. } | SourceKind::Screen | SourceKind::Media { .. } | SourceKind::TestPattern
                    )
            })
            .collect();
        video.sort_by_key(|s| s.order);
        let n = video.len().min(rects.len());
        for (src, t) in video.into_iter().zip(rects.iter()) {
            src.transform = t.filter(|t| !t.is_full());
        }
        self.events.push_back(UiEvent::SourceChanged {
            scene_id: scene_id.to_string(),
        });
        Ok(n)
    }
}

/// Box per video source (bottom-first) for a preset layout.
pub(crate) fn layout_rects(layout: &str) -> Option<Vec<Option<SourceTransform>>> {
    use SourceTransform as T;
    let full = None;
    // PiP: 30 % size (keeps 16:9 on a 16:9 canvas), 3 % margin.
    let (pw, m) = (0.30_f32, 0.03_f32);
    let pip = |x: f32, y: f32| Some(T::rect(x, y, pw, pw));
    Some(match layout {
        "full" => vec![full; 8],
        "side_by_side" => vec![Some(T::rect(0.0, 0.0, 0.5, 1.0)), Some(T::rect(0.5, 0.0, 0.5, 1.0))],
        "side_by_side_fit" => vec![Some(T::rect(0.0, 0.25, 0.5, 0.5)), Some(T::rect(0.5, 0.25, 0.5, 0.5))],
        "split_v" => vec![Some(T::rect(0.0, 0.0, 1.0, 0.5)), Some(T::rect(0.0, 0.5, 1.0, 0.5))],
        "pip_tl" => vec![full, pip(m, m)],
        "pip_tr" => vec![full, pip(1.0 - pw - m, m)],
        "pip_bl" => vec![full, pip(m, 1.0 - pw - m)],
        "pip_br" => vec![full, pip(1.0 - pw - m, 1.0 - pw - m)],
        "grid" => vec![
            Some(T::rect(0.0, 0.0, 0.5, 0.5)),
            Some(T::rect(0.5, 0.0, 0.5, 0.5)),
            Some(T::rect(0.0, 0.5, 0.5, 0.5)),
            Some(T::rect(0.5, 0.5, 0.5, 0.5)),
        ],
        _ => return None,
    })
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
        if self.text_cache.len() >= 4 {
            self.text_cache.clear();
        }
        self.text_cache.insert(key, px.clone());
        px
    }
}

/// Alpha "over" composite of `src` onto `canvas` (both RGBA8 straight
/// alpha, same size). Integer math, with fast paths: fully transparent
/// source pixels are skipped, fully opaque ones copied.
pub(crate) fn composite_over(canvas: &mut [u8], src: &[u8]) {
    for (d, s) in canvas.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let sa = s[3] as u32;
        if sa == 0 {
            continue;
        }
        if sa == 255 {
            d.copy_from_slice(s);
            continue;
        }
        let da = d[3] as u32;
        // out_a = sa + da·(1−sa), all in 0..255 fixed point.
        let inv = 255 - sa;
        let oa = sa * 255 + da * inv; // ×255
        if oa == 0 {
            continue;
        }
        for c in 0..3 {
            let v = (s[c] as u32 * sa * 255 + d[c] as u32 * da * inv + oa / 2) / oa;
            d[c] = v.min(255) as u8;
        }
        d[3] = ((oa + 127) / 255).min(255) as u8;
    }
}

/// Transition frame between `a` (from) and `b` (to) at eased progress `t`.
pub(crate) fn transition_blend(kind: TransitionKind, a: &Frame, b: &Frame, t: f32) -> Frame {
    let t = t.clamp(0.0, 1.0);
    match kind {
        TransitionKind::Mix => crossfade(a, b, t),
        TransitionKind::Fade => {
            // First half: a → black; second half: black → b.
            let (src, k) = if t < 0.5 { (a, 1.0 - t * 2.0) } else { (b, t * 2.0 - 1.0) };
            let pixels = src
                .pixels
                .chunks_exact(4)
                .flat_map(|p| {
                    [
                        (p[0] as f32 * k).round() as u8,
                        (p[1] as f32 * k).round() as u8,
                        (p[2] as f32 * k).round() as u8,
                        p[3],
                    ]
                })
                .collect();
            Frame { width: src.width, height: src.height, pixels }
        }
        TransitionKind::Wipe => {
            let w = a.width as usize;
            let edge = (t * a.width as f32).round() as usize;
            let mut pixels = a.pixels.clone();
            if b.pixels.len() == pixels.len() && w > 0 {
                for (row_a, row_b) in pixels.chunks_exact_mut(w * 4).zip(b.pixels.chunks_exact(w * 4)) {
                    row_a[..edge * 4].copy_from_slice(&row_b[..edge * 4]);
                }
            }
            Frame { width: a.width, height: a.height, pixels }
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
        let mut canvas: Option<Vec<u8>> = None;
        for src in &sources {
            let input_id = src
                .input_id
                .as_ref()
                .ok_or_else(|| ShellError::UnknownSource(src.id.clone()))?;
            let is_capture = matches!(
                src.kind,
                SourceKind::Media { .. } | SourceKind::Camera { .. } | SourceKind::Screen
            );
            // Placed layer (PiP / side-by-side / crop): pull the source
            // already scaled for its box (GPU resize, cached per frame) and
            // blit only that rectangle — no full-frame blend.
            let place = match src.transform.filter(|t| !t.is_full()) {
                Some(t) => {
                    let (nw, nh) = if is_capture {
                        match self.source_native_size(&src.id) {
                            Some(sz) => sz,
                            None => continue,
                        }
                    } else if let SourceKind::Image { path } = &src.kind {
                        match self.image_cache.get(path) {
                            Some(img) => (img.width, img.height),
                            None => continue,
                        }
                    } else {
                        (w, h)
                    };
                    Some((placement(&t, w, h, nw, nh), t.opacity, t.rotation))
                }
                None => None,
            };
            let (pw, ph) = place.map_or((w, h), |(p, _, _)| (p.sw, p.sh));
            let pixels: std::sync::Arc<Vec<u8>> = match &src.kind {
                SourceKind::Media { .. } | SourceKind::Camera { .. } | SourceKind::Screen => {
                    // The tick's cached frame (one pull per source per
                    // tick); skip the layer until the first frame arrives.
                    match self.pull_media_frame(&src.id, pw, ph) {
                        Some(px) => px,
                        None => continue,
                    }
                }
                SourceKind::Text { text } => {
                    std::sync::Arc::new(self.text_pixels(&src.id, text, pw, ph))
                }
                SourceKind::Image { path } => match self.image_pixels(path, pw, ph) {
                    Some(px) => px,
                    None => continue,
                },
                SourceKind::TestPattern => std::sync::Arc::new(self.test_pattern.frame(
                    pw,
                    ph,
                    self.video_settings.output_fps,
                    std::time::SystemTime::now(),
                )),
                _ => std::sync::Arc::new(source_pixels(&src.kind, pw, ph)),
            };
            // Ungraded source with nothing watching it: the grade would be a
            // bit-exact bypass, so skip the GPU round trip (upload, download,
            // sync) entirely and use the shared frame as is.
            let g0 = std::time::Instant::now();
            let graded: std::sync::Arc<Vec<u8>> = if self.engine.grade_is_passthrough(input_id) {
                std::sync::Arc::clone(&pixels)
            } else {
                std::sync::Arc::new(self.engine.grade_input_frame(input_id, pw, ph, &pixels)?.pixels)
            };
            self.perf_gpu_us_accum += g0.elapsed().as_micros() as u64;
            let watch = self.studio_watching(input_id);
            let opaque = is_capture
                || matches!(src.kind, SourceKind::TestPattern | SourceKind::Color { .. })
                || matches!(&src.kind, SourceKind::Image { path } if self.image_cache.get(path).is_some_and(|i| !i.has_alpha));
            if let Some((p, opacity, rotation)) = place {
                let base = canvas.get_or_insert_with(|| vec![0u8; w as usize * h as usize * 4]);
                if rotation.abs() > 0.01 {
                    blit_rotated(base, w, h, &graded, &p, opacity, rotation);
                } else {
                    blit_placed(base, w, h, &graded, &p, opacity, opaque);
                }
            } else if opaque || canvas.is_none() {
                // Full-frame layer: an opaque one replaces everything below
                // (no blend); the first layer needs no blend either.
                let px = if watch.is_none() {
                    std::sync::Arc::try_unwrap(std::sync::Arc::clone(&graded)).unwrap_or_else(|a| (*a).clone())
                } else {
                    (*graded).clone()
                };
                canvas = Some(px);
            } else {
                // Transparent overlay (text, logo, PNG): CPU "over" with
                // fast paths for fully transparent / opaque pixels.
                let base = canvas.as_mut().expect("checked");
                composite_over(base, &graded);
            }
            // Color Studio live tap: the exact graded frame that goes to
            // program/encoder (and the raw one for before/after).
            if let Some(mode) = watch {
                let raw = (mode != crate::color_library::PreviewMode::After).then(|| Frame {
                    width: pw,
                    height: ph,
                    pixels: std::sync::Arc::try_unwrap(pixels).unwrap_or_else(|a| (*a).clone()),
                });
                let input_id = input_id.clone();
                let graded_frame = Frame {
                    width: pw,
                    height: ph,
                    pixels: std::sync::Arc::try_unwrap(graded).unwrap_or_else(|a| (*a).clone()),
                };
                self.studio_tap_store(&input_id, graded_frame, raw);
            }
        }
        Ok(Frame {
            width: w,
            height: h,
            pixels: canvas.unwrap_or_else(|| vec![0u8; w as usize * h as usize * 4]),
        })
    }

    /// Content key of a scene whose picture cannot change by itself: only
    /// colour, image and text layers, all ungraded, no transition, no Color
    /// page watching. `None` = something moves (camera, screen, file, test
    /// signal, grade, transition) — render every frame. The key covers every
    /// layer's full settings (kind, text, colour, placement, order,
    /// visibility) and the render size, so any edit gives a new key.
    pub(crate) fn scene_static_key(&self, scene_id: &str, w: u32, h: u32) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        if self.transition.is_some() || self.studio_watch_active() {
            return None;
        }
        let scene = self.find_scene(scene_id)?;
        let mut hs = std::collections::hash_map::DefaultHasher::new();
        scene_id.hash(&mut hs);
        (w, h).hash(&mut hs);
        for s in &scene.sources {
            if !s.visible || !s.kind.is_real() || matches!(s.kind, SourceKind::AudioInput { .. }) {
                continue;
            }
            if !matches!(s.kind, SourceKind::Color { .. } | SourceKind::Image { .. } | SourceKind::Text { .. }) {
                return None;
            }
            let input = s.input_id.as_ref()?;
            if !self.engine.grade_is_passthrough(input) {
                return None;
            }
            format!("{s:?}").hash(&mut hs);
        }
        Some(hs.finish())
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
                t.kind,
            )
        });
        match active {
            Some((from, to, progress, kind)) => {
                let a = self.render_scene_frame(&from, width, height)?;
                let b = self.render_scene_frame(&to, width, height)?;
                encode_png(&transition_blend(kind, &a, &b, progress))
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
                fx_stats: self.channel_fx.iter().map(|(id, c)| (id.clone(), c.stats())).collect(),
                master_fx_stats: self.master_fx.stats(),
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
        let kind = TransitionKind::parse(kind)
            .ok_or_else(|| ShellError::UnknownTransitionKind(kind.to_string()))?;
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
                kind: active.kind.name().to_string(),
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

    /// Balance `-1..=1`.
    pub fn mixer_set_pan(&mut self, source_id: &str, pan: f32) -> Result<ChannelStrip, ShellError> {
        let strip = self.mixer.channels.get_mut(source_id).ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        strip.pan = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
        let strip = strip.clone();
        self.events.push_back(UiEvent::MixerChanged);
        Ok(strip)
    }

    /// Solo (local monitor only).
    pub fn mixer_set_solo(&mut self, source_id: &str, solo: bool) -> Result<ChannelStrip, ShellError> {
        let strip = self.mixer.channels.get_mut(source_id).ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        strip.solo = solo;
        let strip = strip.clone();
        self.events.push_back(UiEvent::MixerChanged);
        Ok(strip)
    }

    /// Input trim in dB, `-20..=20`.
    pub fn mixer_set_trim(&mut self, source_id: &str, trim_db: f32) -> Result<ChannelStrip, ShellError> {
        let strip = self.mixer.channels.get_mut(source_id).ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        strip.trim_db = if trim_db.is_finite() { trim_db.clamp(-20.0, 20.0) } else { 0.0 };
        let strip = strip.clone();
        self.events.push_back(UiEvent::MixerChanged);
        Ok(strip)
    }

    /// Live meters since the previous call (resets the peak hold).
    pub fn audio_meters(&mut self) -> AudioMetersView {
        let paths: std::collections::HashSet<&String> = self
            .audio_inputs
            .keys()
            .chain(self.media_producers.iter().filter(|(_, p)| sf_capture::FrameProducer::stream_info(&p.0).has_audio).map(|(id, _)| id))
            .collect();
        let mut channels: Vec<ChannelMeterView> = self
            .mixer
            .channels
            .keys()
            .map(|id| {
                let (peak, rms) = self.meter_hold.get(id).copied().unwrap_or((0.0, 0.0));
                ChannelMeterView { id: id.clone(), peak, rms, has_signal_path: paths.contains(id), fx: self.channel_fx.get(id).map(|c| c.stats()) }
            })
            .collect();
        channels.sort_by(|a, b| a.id.cmp(&b.id));
        let (master_peak, master_rms) = self.master_meter_hold;
        self.meter_hold.clear();
        self.master_meter_hold = (0.0, master_rms);
        AudioMetersView {
            channels,
            master_peak,
            master_rms,
            solo_active: self.mixer.channels.values().any(|c| c.solo),
            master_fx: self.master_fx.stats(),
        }
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

    /// Replaces a channel's effects settings (values are clamped). Applies
    /// on the next audio block, without clicks.
    pub fn mixer_set_fx(&mut self, source_id: &str, fx: sf_audio::FxSettings) -> Result<ChannelStrip, ShellError> {
        let strip = self.mixer.channels.get_mut(source_id).ok_or_else(|| ShellError::UnknownSource(source_id.to_string()))?;
        strip.fx = fx.sanitized();
        let strip = strip.clone();
        self.events.push_back(UiEvent::MixerChanged);
        Ok(strip)
    }

    /// Replaces the master bus effects settings (values are clamped).
    pub fn mixer_set_master_fx(&mut self, fx: sf_audio::FxSettings) -> ChannelStrip {
        self.mixer.master.fx = fx.sanitized();
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
    /// Requested device format (None = automatic).
    #[serde(default)]
    pub format: Option<String>,
    /// What the camera actually negotiated (None = not opened / pending).
    pub negotiated: Option<String>,
}

/// Camera fps to request: any of [`CAMERA_FPS_OPTIONS`], else 30.
pub(crate) fn camera_fps_or_default(fps: u32) -> u32 {
    if CAMERA_FPS_OPTIONS.contains(&fps) {
        fps
    } else {
        30
    }
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
                    format,
                    ..
                } => Some(CameraSourceInfo {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    device: device.clone(),
                    width: *width,
                    height: *height,
                    fps: *fps,
                    format: format.clone(),
                    negotiated: self.camera_formats.get(self.frame_owner(&s.id)).cloned(),
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
        if !(BITRATE_RANGE_KBPS.0..=BITRATE_RANGE_KBPS.1).contains(&s.bitrate_kbps) {
            return Err(ShellError::InvalidSetting(format!(
                "bitrate {} kbps out of range ({}-{})",
                s.bitrate_kbps, BITRATE_RANGE_KBPS.0, BITRATE_RANGE_KBPS.1
            )));
        }
        if !VIDEO_CAMERA_SIZES.contains(&(s.camera_width, s.camera_height)) {
            return Err(ShellError::InvalidSetting(format!(
                "unsupported camera size {}x{}",
                s.camera_width, s.camera_height
            )));
        }
        if !CAMERA_FPS_OPTIONS.contains(&s.camera_fps) {
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
        let format = self
            .scenes
            .iter()
            .flat_map(|sc| sc.sources.iter())
            .find(|s| s.id == source_id)
            .and_then(|s| match &s.kind {
                SourceKind::Camera { format, .. } => format.clone(),
                _ => None,
            });
        self.camera_configure(source_id, device, width, height, fps, format, audio_device)
    }

    /// [`Self::camera_reconfigure`] that also picks the device format
    /// (`None` = automatic). The old capture is stopped before the new one
    /// opens, so a device never runs twice.
    #[allow(clippy::too_many_arguments)]
    pub fn camera_configure(
        &mut self,
        source_id: &str,
        device: Option<String>,
        width: u32,
        height: u32,
        fps: u32,
        format: Option<String>,
        audio_device: Option<String>,
    ) -> Result<String, ShellError> {
        let format = format.filter(|f| !f.is_empty() && f != "auto");
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
                format: format.clone(),
            };
        }
        // A shared scene item reconfigures the shared capture (same physical
        // device for every scene); switching it to another device unlinks it.
        let mut target = source_id.to_string();
        if let Some(owner) = self.producer_alias.get(source_id).cloned() {
            let owner_dev = self
                .scenes
                .iter()
                .flat_map(|s| s.sources.iter())
                .find(|s| s.id == owner)
                .and_then(|s| match &s.kind {
                    SourceKind::Camera { device, .. } => Some(device.clone()),
                    _ => None,
                });
            if owner_dev.is_none_or(|d| d == device) {
                target = owner;
            } else {
                self.producer_alias.remove(source_id);
            }
        }
        let source_id = target.as_str();
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
            let f = camera_fps_or_default(fps);
            self.open_camera_producer(
                source_id,
                device.as_deref(),
                existing_audio.as_deref(),
                w,
                h,
                f,
                format.as_deref(),
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

#[cfg(test)]
mod transition_tests {
    use super::*;

    fn solid(v: u8) -> Frame {
        Frame { width: 4, height: 2, pixels: vec![v; 4 * 2 * 4] }
    }

    #[test]
    fn fade_passes_through_black_and_wipe_moves_an_edge() {
        let (a, b) = (solid(200), solid(100));
        assert_eq!(transition_blend(TransitionKind::Fade, &a, &b, 0.5).pixels[0], 0);
        assert_eq!(transition_blend(TransitionKind::Fade, &a, &b, 0.0).pixels[0], 200);
        assert_eq!(transition_blend(TransitionKind::Fade, &a, &b, 1.0).pixels[0], 100);
        let w = transition_blend(TransitionKind::Wipe, &a, &b, 0.5);
        assert_eq!(w.pixels[0], 100, "left of the edge shows the incoming scene");
        assert_eq!(w.pixels[3 * 4], 200, "right of the edge still shows the outgoing scene");
        assert_eq!(TransitionKind::parse("wipe"), Some(TransitionKind::Wipe));
        assert_eq!(TransitionKind::parse("x"), None);
    }

    #[test]
    fn pan_and_trim() {
        let mut b = vec![0.5f32; 8];
        apply_trim_pan(&mut b, 0.0, -1.0);
        assert_eq!((b[0], b[1]), (0.5, 0.0), "hard left silences the right channel");
        let mut b = vec![0.5f32; 8];
        apply_trim_pan(&mut b, 6.0, 0.0);
        assert!((b[0] - 0.5 * 10f32.powf(0.3)).abs() < 1e-4, "+6 dB trim");
    }

    #[test]
    fn placement_fill_crops_centre_and_fit_letterboxes() {
        // 16:9 source into a square box.
        let t = SourceTransform::rect(0.0, 0.0, 0.5, 1.0); // 100x100 on 200x100
        let p = placement(&t, 200, 100, 1920, 1080);
        assert_eq!((p.cx, p.cy, p.cw, p.ch), (0, 0, 100, 100));
        // Scaled so the height fills: 100 high → 178 wide, centred crop.
        assert_eq!((p.sw, p.sh), (178, 100));
        assert_eq!(p.ox, 39);
        let mut f = t;
        f.fit = FitMode::Fit;
        let p = placement(&f, 200, 100, 1920, 1080);
        assert_eq!((p.cw, p.ch), (100, 56));
        assert_eq!(p.cy, 22);
        // Crop 25 % left: the offset starts inside the scaled source.
        let mut c = SourceTransform::rect(0.0, 0.0, 1.0, 1.0);
        c.crop_left = 0.25;
        c.fit = FitMode::Stretch;
        let p = placement(&c, 200, 100, 200, 100);
        assert_eq!((p.sw, p.ox), (267, 67));
    }
}
