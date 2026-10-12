//! `sf-audio` — real-time software audio mixer for StreamFlow Lite.
//!
//! Works purely on `f32` PCM buffers; no audio hardware, no FFmpeg. The
//! capture side (`sf-capture`) normalizes decoded audio to the contract
//! format — **48000 Hz, stereo, interleaved `f32`** — before handing frames
//! to this crate. This crate does not depend on `sf-capture`; anything that
//! violates the contract is skipped defensively (see [`Mixer::dropped_blocks`]).
//!
//! # Signal chain
//!
//! ```text
//! AudioFrame (contract: 48 kHz stereo f32 interleaved)
//!   → [AI-FX INSERT POINT #1: per-source PRE-gain]
//!   → per-source gain (clamped 0.0–2.0)
//!   → per-source mute
//!   → SourceMeter (peak/RMS, measured POST-gain, PRE-sum — the meter
//!                   follows the fader, not the raw input)
//!   → sum into the mix bus (a registered source that sent no frame this
//!     block contributes silence)
//!   → [AI-FX INSERT POINT #2: program bus, POST-sum, PRE-master]
//!   → master gain / master mute
//!   → soft-clip: hard clamp at ±1.0 (0 dBFS)
//!   → peak/RMS of the mixed output (measured POST-clip, so the meter
//!     matches what is actually sent to the encoder / speakers)
//! ```
//!
//! ## Soft-clip choice: hard clamp at ±1.0
//!
//! A hard clamp was chosen over `tanh` deliberately:
//!
//! - This is a live-production mixer, not a mastering chain. Below the clip
//!   point the path is perfectly linear — zero harmonic coloration from the
//!   mixer itself. `tanh` would compress *everything*, even quiet sources,
//!   adding audible distortion where none is needed.
//! - At the clip point a hard clamp is the broadcast-safe behavior: a
//!   predictable 0 dBFS ceiling, no wrap-around, no NaN, no infinite tails
//!   into the encoder.
//! - Per-source gains are already clamped to 2.0, so the clip point is only
//!   reached when sources genuinely overload the bus; that overload is
//!   reported honestly via `peak == 1.0` on the mixed output.
//!
//! ## Complexity
//!
//! `mix` is **O(S·N)** where S = registered sources and N = samples per block:
//! one pass over each contributing frame, accumulating directly into the
//! single output buffer. Per-source meters are measured in the same pass
//! (POST-gain, PRE-sum), so there are no per-source temporary buffers — the
//! only allocation per `mix` call is the output `Vec<f32>` of N samples. The
//! mixer is fully deterministic: identical inputs produce bit-identical
//! outputs (the only mutable state is the `dropped_blocks` counter).
//!
//! ## AI insert points (future work — documented, not implemented)
//!
//! - **#1 Per-source PRE-gain** (see the marker in `mix`): per-source AI
//!   denoise / voice-clarity / de-esser processors slot in here. They receive
//!   the raw contract-normalized frame and return a processed frame of
//!   identical length. Inserting *before* the gain multiply means the fader
//!   still controls the processed output, and the [`SourceMeter`] still
//!   reflects the fader position.
//! - **#2 Program-bus PRE-master** (see the marker in `mix`): a program-level
//!   AI processor (e.g. loudness leveling / auto-ducking) slots in here,
//!   operating on the summed bus before master gain and clipping.

use std::collections::HashMap;

pub mod enhancer;
pub use enhancer::{Enhancer, EnhancerSettings};

/// Sample rate (Hz) every input must carry. Anything else is skipped for the
/// block and counted in [`Mixer::dropped_blocks`].
pub const TARGET_SAMPLE_RATE: u32 = 48_000;
/// Channel count every input must carry. Anything else is skipped.
pub const TARGET_CHANNELS: u32 = 2;
/// Maximum fader gain. `set_gain`/`add_source` clamp to `0.0..=MAX_GAIN`.
pub const MAX_GAIN: f32 = 2.0;
/// Clip ceiling of the mix bus (0 dBFS).
pub const CLIP_CEILING: f32 = 1.0;

/// One block of audio: interleaved `f32` PCM samples.
///
/// The contract is `sample_rate == 48000` and `channels == 2` (set by
/// [`TARGET_SAMPLE_RATE`] / [`TARGET_CHANNELS`]); `sf-capture` normalizes to
/// this. Frames that violate the contract are *skipped* for that block by
/// [`Mixer::mix`] (counted, not an error).
///
/// `samples.len()` is normally `channels * frames_per_block`; any length is
/// accepted, and mixing processes `min(frame_len, block_len)`.
#[derive(Clone, Copy, Debug)]
pub struct AudioFrame<'a> {
    /// Interleaved samples (e.g. `[L0, R0, L1, R1, …]` for stereo).
    pub samples: &'a [f32],
    /// Channel count (contract: 2).
    pub channels: u32,
    /// Sample rate in Hz (contract: 48000).
    pub sample_rate: u32,
}

impl<'a> AudioFrame<'a> {
    /// Builds a frame from a sample slice.
    pub fn new(samples: &'a [f32], channels: u32, sample_rate: u32) -> Self {
        Self {
            samples,
            channels,
            sample_rate,
        }
    }

    /// Whether this frame meets the mix contract and can be mixed directly.
    pub fn conforms(&self) -> bool {
        self.sample_rate == TARGET_SAMPLE_RATE && self.channels == TARGET_CHANNELS
    }
}

/// Per-source meter reading: measured POST-gain, PRE-sum, so it reflects the
/// fader position. A muted source reports `peak == 0.0`, `rms == 0.0`.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceMeter {
    /// Source id as registered in the mixer.
    pub id: String,
    /// Maximum absolute sample value of the post-gain signal.
    pub peak: f32,
    /// Root-mean-square of the post-gain signal (over interleaved samples).
    pub rms: f32,
}

/// The mixed bus plus meters, produced by [`Mixer::mix`].
#[derive(Clone, Debug)]
pub struct MixedOutput {
    /// Interleaved stereo 48 kHz `f32` samples, post master gain and clip.
    pub samples: Vec<f32>,
    /// Peak (max |sample|) of the mixed output, measured POST-clip.
    pub peak: f32,
    /// RMS of the mixed output, measured POST-clip.
    pub rms: f32,
    /// One meter per registered source, in registration order.
    pub per_source: Vec<SourceMeter>,
}

/// Public view of one mixer strip (for UI consumption).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripState {
    /// Fader gain, always within `0.0..=MAX_GAIN`.
    pub gain: f32,
    /// Mute flag.
    pub mute: bool,
}

#[derive(Debug)]
struct Strip {
    gain: f32,
    mute: bool,
}

/// Real-time software mixer: per-source faders → mute → sum → master → clip.
#[derive(Debug, Default)]
pub struct Mixer {
    /// Registration order (keeps meter order deterministic).
    order: Vec<String>,
    strips: HashMap<String, Strip>,
    master_gain: f32,
    master_mute: bool,
    /// Frames skipped because they violated the mix contract.
    dropped_blocks: u64,
}

impl Mixer {
    /// Creates a mixer with `master_gain = 1.0` and no sources.
    pub fn new() -> Self {
        Self {
            order: Vec::new(),
            strips: HashMap::new(),
            master_gain: 1.0,
            master_mute: false,
            dropped_blocks: 0,
        }
    }

    fn clamp_gain(gain: f32) -> f32 {
        gain.clamp(0.0, MAX_GAIN)
    }

    /// Registers a source with `gain` (clamped to `0.0..=MAX_GAIN`), unmuted.
    /// If the id is already registered, its gain is updated and the mute
    /// state is preserved (idempotent add).
    pub fn add_source(&mut self, id: &str, gain: f32) {
        let gain = Self::clamp_gain(gain);
        match self.strips.get_mut(id) {
            Some(strip) => strip.gain = gain,
            None => {
                self.order.push(id.to_string());
                self.strips
                    .insert(id.to_string(), Strip { gain, mute: false });
            }
        }
    }

    /// Removes a source. Returns `true` if it was registered.
    pub fn remove_source(&mut self, id: &str) -> bool {
        if self.strips.remove(id).is_some() {
            self.order.retain(|o| o != id);
            true
        } else {
            false
        }
    }

    /// Sets a source's fader gain (clamped to `0.0..=MAX_GAIN`).
    /// Returns `false` if the id is not registered.
    pub fn set_gain(&mut self, id: &str, gain: f32) -> bool {
        match self.strips.get_mut(id) {
            Some(strip) => {
                strip.gain = Self::clamp_gain(gain);
                true
            }
            None => false,
        }
    }

    /// Mutes/unmutes a source. Returns `false` if the id is not registered.
    pub fn set_mute(&mut self, id: &str, mute: bool) -> bool {
        match self.strips.get_mut(id) {
            Some(strip) => {
                strip.mute = mute;
                true
            }
            None => false,
        }
    }

    /// Sets the master fader gain (clamped to `0.0..=MAX_GAIN`).
    pub fn set_master_gain(&mut self, gain: f32) {
        self.master_gain = Self::clamp_gain(gain);
    }

    /// Mutes/unmutes the master bus.
    pub fn set_master_mute(&mut self, mute: bool) {
        self.master_mute = mute;
    }

    /// Current master fader gain.
    pub fn master_gain(&self) -> f32 {
        self.master_gain
    }

    /// Whether the master bus is muted.
    pub fn master_muted(&self) -> bool {
        self.master_mute
    }

    /// Number of frames skipped so far because they violated the mix
    /// contract (wrong sample rate or channel count).
    pub fn dropped_blocks(&self) -> u64 {
        self.dropped_blocks
    }

    /// Resets the [`Mixer::dropped_blocks`] counter.
    pub fn reset_dropped(&mut self) {
        self.dropped_blocks = 0;
    }

    /// Ids of all registered sources, in registration order.
    pub fn source_ids(&self) -> Vec<String> {
        self.order.clone()
    }

    /// Current strip state for `id`, or `None` if not registered.
    pub fn strip(&self, id: &str) -> Option<StripState> {
        self.strips.get(id).map(|s| StripState {
            gain: s.gain,
            mute: s.mute,
        })
    }

    /// Mixes one block.
    ///
    /// `inputs` maps source ids to their frames for this block. The block
    /// length is the length of the first contract-conforming frame (0 if
    /// none — the output is then empty silence). Frames are processed in
    /// registration order; a registered source with no frame contributes
    /// silence; non-conforming frames are skipped and counted in
    /// `dropped_blocks`.
    pub fn mix(&mut self, inputs: &[(&str, AudioFrame)]) -> MixedOutput {
        let block_len = inputs
            .iter()
            .filter(|(_, f)| f.conforms())
            .map(|(_, f)| f.samples.len())
            .next()
            .unwrap_or(0);

        let mut out = vec![0.0f32; block_len];
        let mut per_source = Vec::with_capacity(self.order.len());

        // Borrow the strips immutably for the source pass; the dropped
        // counter is updated afterwards from the local count.
        let mut dropped = 0u64;
        {
            for id in &self.order {
                // `order` and `strips` are kept in sync by add/remove.
                let strip = &self.strips[id.as_str()];
                let frame = inputs
                    .iter()
                    .find(|(i, _)| *i == id.as_str())
                    .map(|(_, f)| *f);

                let (mut peak, mut sum_sq) = (0.0f32, 0.0f64);
                match frame {
                    Some(f) if !f.conforms() => {
                        // Safety net: capture is expected to normalize, but a
                        // wrong-rate/-channel frame must never corrupt the
                        // bus. Skip it for this block and count it.
                        dropped += 1;
                    }
                    Some(f) if !strip.mute => {
                        // ---- AI-FX INSERT POINT #1 (per-source, PRE-gain) ----
                        // Per-source AI denoise / voice-clarity / de-esser
                        // processors slot in here: they receive the raw
                        // contract-normalized frame `f` and return a processed
                        // frame of identical length. Inserting before the
                        // gain multiply keeps the fader in control of the
                        // processed output, and the meter below (POST-gain,
                        // PRE-sum) still follows the fader.
                        let g = strip.gain;
                        let n = block_len.min(f.samples.len());
                        for i in 0..n {
                            let s = f.samples[i] * g;
                            out[i] += s;
                            let a = s.abs();
                            if a > peak {
                                peak = a;
                            }
                            sum_sq += f64::from(s) * f64::from(s);
                        }
                    }
                    // `None` (registered but silent this block) or muted:
                    // contribute silence, meters stay at 0.
                    _ => {}
                }
                let rms = if block_len > 0 {
                    (sum_sq / block_len as f64).sqrt() as f32
                } else {
                    0.0
                };
                per_source.push(SourceMeter {
                    id: id.clone(),
                    peak,
                    rms,
                });
            }
        }
        self.dropped_blocks += dropped;

        // ---- AI-FX INSERT POINT #2 (program bus, POST-sum, PRE-master) ----
        // A program-level AI processor (e.g. loudness leveling /
        // auto-ducking) slots in here, operating on the summed `out`
        // buffer before master gain and clipping.

        let mg = if self.master_mute {
            0.0
        } else {
            self.master_gain
        };
        let (mut mix_peak, mut mix_sq) = (0.0f32, 0.0f64);
        for s in out.iter_mut() {
            // Soft-clip: hard clamp at ±1.0 (0 dBFS). Linear below the
            // ceiling, predictable at it; never wraps, never NaNs.
            let c = (*s * mg).clamp(-CLIP_CEILING, CLIP_CEILING);
            *s = c;
            let a = c.abs();
            if a > mix_peak {
                mix_peak = a;
            }
            mix_sq += f64::from(c) * f64::from(c);
        }
        let rms = if block_len > 0 {
            (mix_sq / block_len as f64).sqrt() as f32
        } else {
            0.0
        };

        MixedOutput {
            samples: out,
            peak: mix_peak,
            rms,
            per_source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Stereo-interleaved `amp * sin(2π·freq·t)` (same on both channels).
    fn sine_stereo(freq: f32, amp: f32, frames: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
            v.push(s);
            v.push(s);
        }
        v
    }

    /// One full period of 100 Hz at 48 kHz = exactly 480 frames.
    const FRAMES_100HZ: usize = 480;

    fn frame_of(samples: &[f32]) -> AudioFrame<'_> {
        AudioFrame::new(samples, TARGET_CHANNELS, TARGET_SAMPLE_RATE)
    }

    fn approx(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn single_sine_peak_and_rms() {
        // Hand-computed: one full period of a 1.0-amplitude sine has peak
        // exactly 1.0 and RMS exactly 1/√2 ≈ 0.70710678.
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        let data = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("a", frame_of(&data))]);
        assert_eq!(out.samples.len(), FRAMES_100HZ * 2);
        assert!(approx(out.peak, 1.0, 1e-5), "peak {} not ≈ 1.0", out.peak);
        assert!(
            approx(out.rms, 1.0 / 2.0f32.sqrt(), 1e-5),
            "rms {} not ≈ 0.7071",
            out.rms
        );
        // Per-source meter matches the mixed output for a single source.
        assert_eq!(out.per_source.len(), 1);
        assert!(approx(out.per_source[0].peak, 1.0, 1e-5));
        assert!(approx(out.per_source[0].rms, 1.0 / 2.0f32.sqrt(), 1e-5));
    }

    #[test]
    fn two_sines_superposition() {
        // Hand-computed: 0.5·sin(ωt) + 0.5·cos(ωt) = (√2/2)·sin(ωt + π/4).
        // peak = 0.5√2 ≈ 0.70710678, RMS = 0.5 (energies add, cross term
        // integrates to 0 over a full period).
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.add_source("b", 1.0);
        let a = sine_stereo(100.0, 0.5, FRAMES_100HZ);
        // cos = sin with +π/2 phase.
        let b: Vec<f32> = (0..FRAMES_100HZ)
            .flat_map(|i| {
                let s = 0.5
                    * (2.0 * std::f32::consts::PI * 100.0 * i as f32 / SR
                        + std::f32::consts::FRAC_PI_2)
                        .sin();
                [s, s]
            })
            .collect();
        let out = m.mix(&[("a", frame_of(&a)), ("b", frame_of(&b))]);
        assert!(
            approx(out.peak, 0.5 * 2.0f32.sqrt(), 1e-4),
            "peak {} not ≈ 0.7071068",
            out.peak
        );
        assert!(approx(out.rms, 0.5, 1e-5), "rms {} not ≈ 0.5", out.rms);
        // Per-source meters: each source alone is a 0.5-amplitude sine.
        assert!(approx(out.per_source[0].peak, 0.5, 1e-5));
        assert!(approx(out.per_source[1].peak, 0.5, 1e-5));
    }

    #[test]
    fn mute_yields_zeros() {
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.set_mute("a", true);
        let data = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("a", frame_of(&data))]);
        assert!(out.samples.iter().all(|&s| s == 0.0));
        assert_eq!(out.peak, 0.0);
        assert_eq!(out.rms, 0.0);
        assert_eq!(out.per_source[0].peak, 0.0);
        assert_eq!(out.per_source[0].rms, 0.0);
    }

    #[test]
    fn gain_halves_peak() {
        let mut m = Mixer::new();
        m.add_source("a", 0.5);
        let data = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("a", frame_of(&data))]);
        assert!(approx(out.peak, 0.5, 1e-5), "peak {} not ≈ 0.5", out.peak);
        assert!(
            approx(out.per_source[0].peak, 0.5, 1e-5),
            "meter peak not ≈ 0.5"
        );
    }

    #[test]
    fn master_mute_yields_zeros() {
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.set_master_mute(true);
        let data = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("a", frame_of(&data))]);
        assert!(out.samples.iter().all(|&s| s == 0.0));
        assert_eq!(out.peak, 0.0);
        assert_eq!(out.rms, 0.0);
        // Source meter still reflects the fader (pre-master).
        assert!(approx(out.per_source[0].peak, 1.0, 1e-5));
    }

    #[test]
    fn master_gain_scales() {
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.set_master_gain(0.5);
        let data = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("a", frame_of(&data))]);
        assert!(approx(out.peak, 0.5, 1e-5));
    }

    #[test]
    fn full_scale_clip_no_wrap_or_nan() {
        // Two full-scale DC sources: bus sums to ±2.0 → clamped to ±1.0.
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.add_source("b", 1.0);
        let dc_pos = vec![1.0f32; FRAMES_100HZ * 2];
        let out = m.mix(&[("a", frame_of(&dc_pos)), ("b", frame_of(&dc_pos))]);
        assert_eq!(out.peak, 1.0);
        assert!(!out.peak.is_nan());
        assert!(out.samples.iter().all(|&s| s == 1.0));

        let dc_neg = vec![-1.0f32; FRAMES_100HZ * 2];
        let out = m.mix(&[("a", frame_of(&dc_neg)), ("b", frame_of(&dc_neg))]);
        assert_eq!(out.peak, 1.0);
        assert!(!out.peak.is_nan());
        assert!(out.samples.iter().all(|&s| s == -1.0));
    }

    #[test]
    fn per_source_meters_live_vs_muted() {
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.add_source("b", 1.0);
        m.set_mute("b", true);
        let a = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let b = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("a", frame_of(&a)), ("b", frame_of(&b))]);
        // Meters are in registration order.
        assert_eq!(out.per_source[0].id, "a");
        assert_eq!(out.per_source[1].id, "b");
        assert!(approx(out.per_source[0].peak, 1.0, 1e-5));
        assert!(approx(out.per_source[0].rms, 1.0 / 2.0f32.sqrt(), 1e-5));
        assert_eq!(out.per_source[1].peak, 0.0);
        assert_eq!(out.per_source[1].rms, 0.0);
        // Mixed output equals A alone (B contributed silence).
        assert!(approx(out.peak, 1.0, 1e-5));
    }

    #[test]
    fn wrong_rate_input_skipped_and_counted() {
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.add_source("b", 1.0);
        let a = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let b = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[
            ("a", frame_of(&a)),
            (
                "b",
                AudioFrame::new(&b, TARGET_CHANNELS, 44_100), // wrong rate
            ),
        ]);
        assert_eq!(m.dropped_blocks(), 1);
        // Mix continues with only the conforming source.
        assert!(approx(out.peak, 1.0, 1e-5));
        assert!(approx(out.rms, 1.0 / 2.0f32.sqrt(), 1e-5));
        // Skipped source's meter reads zero.
        assert_eq!(out.per_source[1].id, "b");
        assert_eq!(out.per_source[1].peak, 0.0);
        assert_eq!(out.per_source[1].rms, 0.0);

        // Wrong channel count is also skipped.
        m.reset_dropped();
        let mono = AudioFrame::new(&b, 1, TARGET_SAMPLE_RATE);
        let out = m.mix(&[("a", frame_of(&a)), ("b", mono)]);
        assert_eq!(m.dropped_blocks(), 1);
        assert!(approx(out.peak, 1.0, 1e-5));
    }

    #[test]
    fn silence_in_yields_exact_zeros() {
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        let data = vec![0.0f32; FRAMES_100HZ * 2];
        let out = m.mix(&[("a", frame_of(&data))]);
        assert_eq!(out.peak, 0.0);
        assert_eq!(out.rms, 0.0);
        assert_eq!(out.per_source[0].peak, 0.0);
        assert_eq!(out.per_source[0].rms, 0.0);
    }

    #[test]
    fn missing_source_contributes_silence() {
        // "b" registered but sends no frame this block.
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.add_source("b", 1.0);
        let a = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("a", frame_of(&a))]);
        assert_eq!(out.samples.len(), FRAMES_100HZ * 2);
        assert!(approx(out.peak, 1.0, 1e-5));
        assert_eq!(out.per_source.len(), 2);
        assert_eq!(out.per_source[1].id, "b");
        assert_eq!(out.per_source[1].peak, 0.0);
        assert_eq!(out.per_source[1].rms, 0.0);
        assert_eq!(m.dropped_blocks(), 0);
    }

    #[test]
    fn gain_clamped_to_0_2() {
        let mut m = Mixer::new();
        m.add_source("a", 9.0);
        assert_eq!(m.strip("a").unwrap().gain, 2.0);
        assert!(m.set_gain("a", -3.0));
        assert_eq!(m.strip("a").unwrap().gain, 0.0);
        m.set_master_gain(99.0);
        assert_eq!(m.master_gain(), 2.0);
        // Unknown ids: no-op, report false.
        assert!(!m.set_gain("nope", 1.0));
        assert!(!m.set_mute("nope", true));
        assert!(m.strip("nope").is_none());
    }

    #[test]
    fn add_remove_source_lifecycle() {
        let mut m = Mixer::new();
        m.add_source("a", 1.0);
        m.add_source("b", 0.5);
        assert_eq!(m.source_ids(), vec!["a".to_string(), "b".to_string()]);
        assert!(m.remove_source("a"));
        assert!(!m.remove_source("a"));
        let data = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let out = m.mix(&[("b", frame_of(&data))]);
        assert_eq!(out.per_source.len(), 1);
        assert_eq!(out.per_source[0].id, "b");
        // b at gain 0.5.
        assert!(approx(out.peak, 0.5, 1e-5));
    }

    #[test]
    fn no_inputs_yields_empty_silence() {
        let mut m = Mixer::new();
        let out = m.mix(&[]);
        assert!(out.samples.is_empty());
        assert_eq!(out.peak, 0.0);
        assert_eq!(out.rms, 0.0);
        assert!(out.per_source.is_empty());
    }

    #[test]
    fn mix_is_deterministic() {
        // Bit-identical output for identical input (documented property).
        let mut m = Mixer::new();
        m.add_source("a", 0.7);
        m.add_source("b", 1.3);
        let a = sine_stereo(100.0, 1.0, FRAMES_100HZ);
        let b = sine_stereo(100.0, 0.5, FRAMES_100HZ);
        let o1 = m.mix(&[("a", frame_of(&a)), ("b", frame_of(&b))]);
        let o2 = m.mix(&[("a", frame_of(&a)), ("b", frame_of(&b))]);
        assert_eq!(o1.samples, o2.samples);
        assert_eq!(o1.peak, o2.peak);
        assert_eq!(o1.rms, o2.rms);
    }
}
