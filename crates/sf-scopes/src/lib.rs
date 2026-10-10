//! `sf-scopes` — GPU compute-shader scopes for StreamFlow Lite.
//!
//! Scopes analyze the **graded, display-encoded** frame on the GPU, on a
//! 256x144 downscaled copy ([`SCOPE_W`] x [`SCOPE_H`]), and only while a
//! scope subscription is active (see `sf-engine`'s scope hub — no subscribers
//! means no GPU scope work at all).
//!
//! | Scope | Payload | Notes |
//! |---|---|---|
//! | Waveform | per-column (256) luma + RGB min/max | RGB parade included |
//! | Vectorscope | 128x128 chroma bin grid (Rec.709 Cb/Cr) | skin-tone guide at [`SKIN_TONE_ANGLE_DEG`] deg (UI overlay) |
//! | Histogram | 256-bin R, G, B, luma | atomic histogram |
//! | False color | 256x144 RGB exposure map | stops-from-middle-gray bands |
//! | Zebra | 256x144 RGBA overlay | diagonal stripes on over/under-exposed pixels |
//!
//! [`cpu`] holds f32 reference implementations of every scope — the exact
//! math contract the WGSL shaders in `src/shaders/` follow. GPU-vs-CPU
//! agreement is asserted in `tests/scopes_gpu.rs`.
//!
//! All wgpu usage is backend-agnostic (Vulkan on Linux, DX12 on Windows).

pub mod cpu;
pub mod scope_pass;

pub use scope_pass::{ScopeParams, ScopePass};

/// Scope analysis width in pixels (spec §4.5).
pub const SCOPE_W: u32 = 256;
/// Scope analysis height in pixels (spec §4.5).
pub const SCOPE_H: u32 = 144;
/// Vectorscope chroma grid resolution (bins per axis).
pub const VSCOPE_BINS: u32 = 128;
/// Vectorscope skin-tone guide angle in degrees, per the spec ("~123°").
///
/// Convention: measured in the normalized (Cb, Cr) plane, clockwise from the
/// +Cr (up) axis toward +Cb (right). The guide itself is drawn by the UI as
/// an overlay line; this constant is the shared contract for where it goes.
pub const SKIN_TONE_ANGLE_DEG: f32 = 123.0;
/// Scope frame cadence: 15 Hz (spec §4.5), as a minimum inter-frame interval.
pub const SCOPE_INTERVAL_MICROS: u64 = 66_667;

/// Which scope to compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// Luma + RGB parade waveform (per-column min/max).
    Waveform,
    /// Chroma distribution grid.
    Vectorscope,
    /// Per-channel 256-bin histogram.
    Histogram,
    /// Exposure false-color map.
    FalseColor,
    /// Over/under-exposure zebra overlay.
    Zebra,
}

impl ScopeKind {
    /// Parses the `kind` string from the `scopes.subscribe` IPC command.
    ///
    /// Accepts `"waveform"`, `"vectorscope"`, `"histogram"`,
    /// `"false_color"` and `"zebra"`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "waveform" => Some(Self::Waveform),
            "vectorscope" => Some(Self::Vectorscope),
            "histogram" => Some(Self::Histogram),
            "false_color" => Some(Self::FalseColor),
            "zebra" => Some(Self::Zebra),
            _ => None,
        }
    }

    /// The IPC `kind` string for this scope.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waveform => "waveform",
            Self::Vectorscope => "vectorscope",
            Self::Histogram => "histogram",
            Self::FalseColor => "false_color",
            Self::Zebra => "zebra",
        }
    }

    /// All scope kinds, in IPC documentation order.
    pub fn all() -> [Self; 5] {
        [
            Self::Waveform,
            Self::Vectorscope,
            Self::Histogram,
            Self::FalseColor,
            Self::Zebra,
        ]
    }
}

/// Waveform data: per-column (256) min/max for luma and each RGB channel.
///
/// Column `x` covers the 144 analysis rows at that x position; values are
/// display-referred 0..=1.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformData {
    /// Per-column luma minimum.
    pub luma_min: [f32; 256],
    /// Per-column luma maximum.
    pub luma_max: [f32; 256],
    /// Per-column red minimum / maximum.
    pub r_min: [f32; 256],
    /// Per-column red maximum.
    pub r_max: [f32; 256],
    /// Per-column green minimum.
    pub g_min: [f32; 256],
    /// Per-column green maximum.
    pub g_max: [f32; 256],
    /// Per-column blue minimum.
    pub b_min: [f32; 256],
    /// Per-column blue maximum.
    pub b_max: [f32; 256],
}

impl WaveformData {
    /// Empty waveform (mins at 1.0, maxes at 0.0 — the scan identity).
    pub fn empty() -> Self {
        Self {
            luma_min: [1.0; 256],
            luma_max: [0.0; 256],
            r_min: [1.0; 256],
            r_max: [0.0; 256],
            g_min: [1.0; 256],
            g_max: [0.0; 256],
            b_min: [1.0; 256],
            b_max: [0.0; 256],
        }
    }
}

/// Vectorscope data: 128x128 bins over the normalized (Cb, Cr) chroma plane.
///
/// Bin `(bx, by)` counts pixels with `Cb` in
/// `[-0.5 + bx/128, -0.5 + (bx+1)/128)` and likewise for `Cr` (clamped to
/// the grid edges). Row-major: index `by * 128 + bx`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorscopeData {
    /// 128*128 bin counts, row-major.
    pub bins: Vec<u32>,
}

/// Histogram data: 256 bins per channel over display-referred 0..=1.
///
/// Bin `i` counts values in `[i/256, (i+1)/256)`, with 1.0 landing in bin
/// 255.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistogramData {
    /// Red channel bins.
    pub r: [u32; 256],
    /// Green channel bins.
    pub g: [u32; 256],
    /// Blue channel bins.
    pub b: [u32; 256],
    /// Rec.709 luma bins.
    pub luma: [u32; 256],
}

/// False-color exposure image (256x144 RGB).
///
/// Bands are stops from middle gray (`s = log2(L / 0.18)`), ARRI-inspired:
///
/// | Band | Color | Meaning |
/// |---|---|---|
/// | `s >= 2.42` | red | clipped highlights |
/// | `1.42 <= s < 2.42` | yellow | hot, near clipping |
/// | `0.5 <= s < 1.42` | green | bright mids |
/// | `-0.5 <= s < 0.5` | gray | correct mid exposure |
/// | `-1.5 <= s < -0.5` | cyan | low mids |
/// | `s < -1.5` | blue | shadows / crushed |
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FalseColorImage {
    /// Image width (256).
    pub width: u32,
    /// Image height (144).
    pub height: u32,
    /// Row-major RGB bytes, `width * height * 3`.
    pub rgb: Vec<u8>,
}

/// Zebra overlay image (256x144 RGBA).
///
/// Pixels with luma above `hi` or below `lo` get diagonal white/black
/// stripes (`(x + y) % 16 < 8`) at 0.75 alpha; all other pixels are fully
/// transparent. The UI composites this over the preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZebraOverlay {
    /// Image width (256).
    pub width: u32,
    /// Image height (144).
    pub height: u32,
    /// Row-major RGBA bytes, `width * height * 4`.
    pub rgba: Vec<u8>,
}

/// One computed scope result.
#[derive(Clone, Debug, PartialEq)]
pub enum ScopePayload {
    /// Waveform result.
    Waveform(WaveformData),
    /// Vectorscope result.
    Vectorscope(VectorscopeData),
    /// Histogram result.
    Histogram(HistogramData),
    /// False-color result.
    FalseColor(FalseColorImage),
    /// Zebra result.
    Zebra(ZebraOverlay),
}

impl ScopePayload {
    /// Which scope produced this payload.
    pub fn kind(&self) -> ScopeKind {
        match self {
            Self::Waveform(_) => ScopeKind::Waveform,
            Self::Vectorscope(_) => ScopeKind::Vectorscope,
            Self::Histogram(_) => ScopeKind::Histogram,
            Self::FalseColor(_) => ScopeKind::FalseColor,
            Self::Zebra(_) => ScopeKind::Zebra,
        }
    }
}

/// One scope frame event payload (what the engine emits at 15 Hz per
/// subscription while the scope panel is visible).
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeFrame {
    /// Target input id.
    pub input_id: String,
    /// Which scope this frame is for.
    pub kind: ScopeKind,
    /// Monotonic per-subscription sequence number.
    pub seq: u64,
    /// The computed scope data.
    pub payload: ScopePayload,
}
