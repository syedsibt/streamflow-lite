//! Per-channel (and master) audio effects chain.
//!
//! Every effect has its own on/off switch; a switched-off effect costs
//! nothing (its loop is skipped). With every effect off — the default for
//! music and video files — the chain is not run at all.
//!
//! Fixed order (the usual broadcast order):
//!
//! ```text
//! polarity → mono → low cut → high cut → AI noise removal → noise gate
//!   → 3-band EQ → de-esser → compressor (+ make-up) → limiter → delay
//! ```
//!
//! Contract: 48 kHz stereo interleaved `f32`, any block length, in place.
//! The only stage that adds latency on its own is AI noise removal (a fixed
//! 20 ms). Once it has been switched on for a channel it keeps running (at
//! strength 0 when off, which skips the neural network) so that toggling it
//! never shifts audio against the picture mid-stream. The delay stage adds
//! exactly the delay asked for.

use serde::{Deserialize, Serialize};

use crate::denoise::NoiseSuppressor;
use crate::enhancer::Biquad;

/// Longest delay the delay stage accepts (bounds its memory to 384 KB).
pub const MAX_DELAY_MS: f32 = 1000.0;

/// Settings of one effects chain. Ranges are enforced by [`FxSettings::sanitized`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FxSettings {
    /// Bypass for the whole chain (false = every effect skipped).
    pub enabled: bool,
    /// Flip the waveform (fixes a mic wired out of phase).
    pub invert_polarity: bool,
    /// Sum left and right to mono (a one-sided mic heard on both sides).
    pub mono: bool,

    /// Low cut (high-pass): removes rumble, handling noise, wind.
    pub highpass_enabled: bool,
    /// 20–400 Hz.
    pub highpass_hz: f32,
    /// High cut (low-pass): removes hiss above the voice.
    pub lowpass_enabled: bool,
    /// 2–20 kHz.
    pub lowpass_hz: f32,

    /// AI noise removal (RNNoise): fans, hum, keyboard, room noise.
    pub denoise_enabled: bool,
    /// 0–1.
    pub denoise_strength: f32,

    /// Noise gate: silence between phrases.
    pub gate_enabled: bool,
    /// −80–0 dB.
    pub gate_threshold_db: f32,
    /// How long the gate takes to close, 20–1000 ms.
    pub gate_release_ms: f32,

    /// 3-band EQ: low shelf 200 Hz, mid peak (adjustable), high shelf 6 kHz.
    pub eq_enabled: bool,
    /// −12–+12 dB each.
    pub eq_low_db: f32,
    pub eq_mid_db: f32,
    pub eq_high_db: f32,
    /// Mid band centre, 200–5000 Hz.
    pub eq_mid_hz: f32,

    /// De-esser: tames sharp "s" / "sh" sounds.
    pub deesser_enabled: bool,
    /// 3–10 kHz.
    pub deesser_hz: f32,
    /// Level of the sibilance band above which it is reduced, −60–0 dB.
    pub deesser_threshold_db: f32,
    /// Most reduction applied, 0–18 dB.
    pub deesser_max_db: f32,

    /// Compressor: evens out loud and quiet moments.
    pub compressor_enabled: bool,
    /// −60–0 dB.
    pub comp_threshold_db: f32,
    /// 1–20 (:1).
    pub comp_ratio: f32,
    /// 0.1–200 ms.
    pub comp_attack_ms: f32,
    /// 10–2000 ms.
    pub comp_release_ms: f32,
    /// Make-up gain after compression, 0–24 dB.
    pub comp_makeup_db: f32,

    /// Limiter: hard ceiling, never lets a peak through.
    pub limiter_enabled: bool,
    /// −20–0 dB.
    pub limiter_ceiling_db: f32,
    /// 10–1000 ms.
    pub limiter_release_ms: f32,

    /// Delay (sync offset for this channel only).
    pub delay_enabled: bool,
    /// 0–1000 ms.
    pub delay_ms: f32,
}

impl Default for FxSettings {
    /// Everything off (zero cost): the default for files and other sources.
    fn default() -> Self {
        Self {
            enabled: true,
            invert_polarity: false,
            mono: false,
            highpass_enabled: false,
            highpass_hz: 80.0,
            lowpass_enabled: false,
            lowpass_hz: 16_000.0,
            denoise_enabled: false,
            denoise_strength: 1.0,
            gate_enabled: false,
            gate_threshold_db: -40.0,
            gate_release_ms: 120.0,
            eq_enabled: false,
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            eq_mid_hz: 1000.0,
            deesser_enabled: false,
            deesser_hz: 6000.0,
            deesser_threshold_db: -30.0,
            deesser_max_db: 6.0,
            compressor_enabled: false,
            comp_threshold_db: -18.0,
            comp_ratio: 3.0,
            comp_attack_ms: 10.0,
            comp_release_ms: 100.0,
            comp_makeup_db: 0.0,
            limiter_enabled: false,
            limiter_ceiling_db: -1.0,
            limiter_release_ms: 50.0,
            delay_enabled: false,
            delay_ms: 0.0,
        }
    }
}

impl FxSettings {
    /// Microphones and camera audio: the voice chain StreamFlow has always
    /// applied (AI noise removal, gate, light presence EQ, gentle 3:1
    /// compressor).
    pub fn voice() -> Self {
        Self {
            denoise_enabled: true,
            gate_enabled: true,
            eq_enabled: true,
            eq_mid_db: 2.0,
            eq_high_db: 2.0,
            compressor_enabled: true,
            ..Self::default()
        }
    }

    /// Program (master) bus: a −1 dB limiter so the stream never clips.
    pub fn master() -> Self {
        Self {
            limiter_enabled: true,
            ..Self::default()
        }
    }

    /// True when at least one stage would touch the signal.
    pub fn any_active(&self) -> bool {
        self.enabled
            && (self.invert_polarity
                || self.mono
                || self.highpass_enabled
                || self.lowpass_enabled
                || self.denoise_enabled
                || self.gate_enabled
                || (self.eq_enabled && (self.eq_low_db != 0.0 || self.eq_mid_db != 0.0 || self.eq_high_db != 0.0))
                || self.deesser_enabled
                || self.compressor_enabled
                || self.limiter_enabled
                || (self.delay_enabled && self.delay_ms > 0.0))
    }

    /// Every value clamped to its documented range (NaN → default).
    pub fn sanitized(mut self) -> Self {
        let d = Self::default();
        let c = |v: f32, lo: f32, hi: f32, def: f32| if v.is_finite() { v.clamp(lo, hi) } else { def };
        self.highpass_hz = c(self.highpass_hz, 20.0, 400.0, d.highpass_hz);
        self.lowpass_hz = c(self.lowpass_hz, 2000.0, 20_000.0, d.lowpass_hz);
        self.denoise_strength = c(self.denoise_strength, 0.0, 1.0, d.denoise_strength);
        self.gate_threshold_db = c(self.gate_threshold_db, -80.0, 0.0, d.gate_threshold_db);
        self.gate_release_ms = c(self.gate_release_ms, 20.0, 1000.0, d.gate_release_ms);
        self.eq_low_db = c(self.eq_low_db, -12.0, 12.0, 0.0);
        self.eq_mid_db = c(self.eq_mid_db, -12.0, 12.0, 0.0);
        self.eq_high_db = c(self.eq_high_db, -12.0, 12.0, 0.0);
        self.eq_mid_hz = c(self.eq_mid_hz, 200.0, 5000.0, d.eq_mid_hz);
        self.deesser_hz = c(self.deesser_hz, 3000.0, 10_000.0, d.deesser_hz);
        self.deesser_threshold_db = c(self.deesser_threshold_db, -60.0, 0.0, d.deesser_threshold_db);
        self.deesser_max_db = c(self.deesser_max_db, 0.0, 18.0, d.deesser_max_db);
        self.comp_threshold_db = c(self.comp_threshold_db, -60.0, 0.0, d.comp_threshold_db);
        self.comp_ratio = c(self.comp_ratio, 1.0, 20.0, d.comp_ratio);
        self.comp_attack_ms = c(self.comp_attack_ms, 0.1, 200.0, d.comp_attack_ms);
        self.comp_release_ms = c(self.comp_release_ms, 10.0, 2000.0, d.comp_release_ms);
        self.comp_makeup_db = c(self.comp_makeup_db, 0.0, 24.0, 0.0);
        self.limiter_ceiling_db = c(self.limiter_ceiling_db, -20.0, 0.0, d.limiter_ceiling_db);
        self.limiter_release_ms = c(self.limiter_release_ms, 10.0, 1000.0, d.limiter_release_ms);
        self.delay_ms = c(self.delay_ms, 0.0, MAX_DELAY_MS, 0.0);
        self
    }
}

/// Live readings of one chain (for the mixer and diagnostics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FxStats {
    /// Voice probability from AI noise removal (0 when off).
    pub vad: f32,
    /// Compressor gain reduction now, dB (≥ 0).
    pub comp_reduction_db: f32,
    /// Limiter gain reduction now, dB (≥ 0).
    pub limiter_reduction_db: f32,
    /// De-esser reduction now, dB (≥ 0).
    pub deesser_reduction_db: f32,
    /// The gate is (mostly) closed.
    pub gate_closed: bool,
    /// Latency the chain adds, ms (AI noise removal 20 ms + delay).
    pub latency_ms: f32,
    /// CPU time of the last block, µs.
    pub process_us: u32,
}

const RATE: f32 = 48_000.0;

#[inline]
fn coef(ms: f32) -> f32 {
    (-1.0 / (ms.max(0.01) * 0.001 * RATE)).exp()
}

#[inline]
fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
fn lin_to_db(v: f32) -> f32 {
    20.0 * v.max(1e-9).log10()
}

/// A stateful effects chain for one channel. Keep it alive across blocks
/// (filters and envelopes carry state); rebuild only when a source goes.
pub struct FxChain {
    s: FxSettings,
    hp: [Biquad; 2],
    lp: [Biquad; 2],
    eq: [[Biquad; 3]; 2],
    /// De-esser sidechain (high-pass at the sibilance frequency), per channel.
    ds_hp: [Biquad; 2],
    ds_env: f32,
    ds_gain: f32,
    gate_env: [f32; 2],
    gate_gain: [f32; 2],
    comp_env: f32,
    comp_gain: f32,
    lim_gain: f32,
    denoiser: Option<Box<NoiseSuppressor>>,
    delay: std::collections::VecDeque<f32>,
    stats: FxStats,
}

impl FxChain {
    pub fn new(settings: FxSettings) -> Self {
        let mut c = Self {
            s: FxSettings::default(),
            hp: [Biquad::passthrough(); 2],
            lp: [Biquad::passthrough(); 2],
            eq: [[Biquad::passthrough(); 3]; 2],
            ds_hp: [Biquad::passthrough(); 2],
            ds_env: 0.0,
            ds_gain: 1.0,
            gate_env: [0.0; 2],
            gate_gain: [1.0; 2],
            comp_env: 0.0,
            comp_gain: 1.0,
            lim_gain: 1.0,
            denoiser: None,
            delay: std::collections::VecDeque::new(),
            stats: FxStats::default(),
        };
        c.retune(settings.sanitized());
        c
    }

    pub fn settings(&self) -> &FxSettings {
        &self.s
    }

    pub fn stats(&self) -> FxStats {
        self.stats
    }

    /// True if the chain must run even with every effect off: the AI stage
    /// was used, and dropping its 20 ms delay now would jump the sync.
    pub fn holds_latency(&self) -> bool {
        self.denoiser.is_some()
    }

    /// Apply new settings (cheap; filter memory is kept, so live edits
    /// don't click).
    pub fn set_settings(&mut self, s: FxSettings) {
        let s = s.sanitized();
        if s != self.s {
            self.retune(s);
        }
    }

    fn retune(&mut self, s: FxSettings) {
        for ch in 0..2 {
            self.hp[ch] = Biquad::highpass(s.highpass_hz, RATE).retuned(&self.hp[ch]);
            self.lp[ch] = Biquad::lowpass(s.lowpass_hz, RATE).retuned(&self.lp[ch]);
            let bands = [
                if s.eq_low_db.abs() < 0.05 { Biquad::passthrough() } else { Biquad::lowshelf(200.0, s.eq_low_db, RATE) },
                if s.eq_mid_db.abs() < 0.05 { Biquad::passthrough() } else { Biquad::peaking(s.eq_mid_hz, s.eq_mid_db, 1.0, RATE) },
                if s.eq_high_db.abs() < 0.05 { Biquad::passthrough() } else { Biquad::highshelf(6000.0, s.eq_high_db, RATE) },
            ];
            for (b, nb) in self.eq[ch].iter_mut().zip(bands) {
                *b = nb.retuned(b);
            }
            self.ds_hp[ch] = Biquad::highpass(s.deesser_hz, RATE).retuned(&self.ds_hp[ch]);
        }
        if let Some(d) = self.denoiser.as_mut() {
            d.set_strength(if s.enabled && s.denoise_enabled { s.denoise_strength } else { 0.0 });
        }
        if !(s.enabled && s.delay_enabled) || s.delay_ms <= 0.0 {
            self.delay = std::collections::VecDeque::new();
        }
        self.s = s;
    }

    /// Process one block of interleaved stereo in place.
    pub fn process(&mut self, x: &mut [f32]) {
        let s = &self.s;
        let run = s.any_active() || self.denoiser.is_some();
        if !run || x.len() < 2 {
            self.stats = FxStats::default();
            return;
        }
        let t0 = std::time::Instant::now();
        let on = s.enabled;

        if on && (s.invert_polarity || s.mono) {
            let (inv, mono) = (s.invert_polarity, s.mono);
            for f in x.chunks_exact_mut(2) {
                if mono {
                    let m = 0.5 * (f[0] + f[1]);
                    f[0] = m;
                    f[1] = m;
                }
                if inv {
                    f[0] = -f[0];
                    f[1] = -f[1];
                }
            }
        }
        if on && s.highpass_enabled {
            for f in x.chunks_exact_mut(2) {
                f[0] = self.hp[0].process(f[0]);
                f[1] = self.hp[1].process(f[1]);
            }
        }
        if on && s.lowpass_enabled {
            for f in x.chunks_exact_mut(2) {
                f[0] = self.lp[0].process(f[0]);
                f[1] = self.lp[1].process(f[1]);
            }
        }

        // AI noise removal (created on first use, then always runs).
        if on && s.denoise_enabled && self.denoiser.is_none() {
            let mut d = Box::new(NoiseSuppressor::new());
            d.set_strength(s.denoise_strength);
            self.denoiser = Some(d);
        }
        let mut vad = 0.0;
        if let Some(d) = self.denoiser.as_mut() {
            d.process(x);
            if on && s.denoise_enabled {
                vad = d.vad();
            }
        }

        let mut gate_closed = false;
        if on && s.gate_enabled {
            let thr = db_to_lin(s.gate_threshold_db);
            let att = coef(2.0);
            let rel = coef(s.gate_release_ms);
            let voice = s.denoise_enabled && vad > 0.5;
            for f in x.chunks_exact_mut(2) {
                for ch in 0..2 {
                    let a = f[ch].abs();
                    let env = &mut self.gate_env[ch];
                    *env = if a > *env { att * *env + (1.0 - att) * a } else { rel * *env + (1.0 - rel) * a };
                    let target = if *env > thr || voice { 1.0 } else { 0.0 };
                    let k = if target > self.gate_gain[ch] { 1.0 - att } else { 1.0 - rel };
                    self.gate_gain[ch] += (target - self.gate_gain[ch]) * k;
                    f[ch] *= self.gate_gain[ch];
                }
            }
            gate_closed = self.gate_gain[0].max(self.gate_gain[1]) < 0.5;
        }

        if on && s.eq_enabled && (s.eq_low_db != 0.0 || s.eq_mid_db != 0.0 || s.eq_high_db != 0.0) {
            for f in x.chunks_exact_mut(2) {
                for ch in 0..2 {
                    let mut v = f[ch];
                    for b in self.eq[ch].iter_mut() {
                        v = b.process(v);
                    }
                    f[ch] = v;
                }
            }
        }

        let mut ds_red = 0.0;
        if on && s.deesser_enabled {
            // Wideband de-esser: a high-pass sidechain listens for the
            // sibilance band; while it is above the threshold the channel
            // is turned down by the overshoot (×0.75, capped). No band split,
            // so no phase smearing of the voice.
            let thr_db = s.deesser_threshold_db;
            let max_db = s.deesser_max_db;
            let att = coef(1.0);
            let rel = coef(60.0);
            for f in x.chunks_exact_mut(2) {
                let hl = self.ds_hp[0].process(f[0]);
                let hr = self.ds_hp[1].process(f[1]);
                let a = hl.abs().max(hr.abs());
                self.ds_env = if a > self.ds_env { att * self.ds_env + (1.0 - att) * a } else { rel * self.ds_env + (1.0 - rel) * a };
                let over = lin_to_db(self.ds_env) - thr_db;
                let target = if over > 0.0 { db_to_lin(-(over * 0.75).min(max_db)) } else { 1.0 };
                let k = if target < self.ds_gain { 1.0 - att } else { 1.0 - rel };
                self.ds_gain += (target - self.ds_gain) * k;
                f[0] *= self.ds_gain;
                f[1] *= self.ds_gain;
            }
            ds_red = -lin_to_db(self.ds_gain);
        }

        let mut comp_red = 0.0;
        if on && s.compressor_enabled {
            let thr = s.comp_threshold_db;
            let slope = 1.0 - 1.0 / s.comp_ratio.max(1.0);
            let att = coef(s.comp_attack_ms);
            let rel = coef(s.comp_release_ms);
            let makeup = db_to_lin(s.comp_makeup_db);
            for f in x.chunks_exact_mut(2) {
                let peak = f[0].abs().max(f[1].abs());
                self.comp_env = if peak > self.comp_env { att * self.comp_env + (1.0 - att) * peak } else { rel * self.comp_env + (1.0 - rel) * peak };
                let over = lin_to_db(self.comp_env) - thr;
                let target = if over > 0.0 { db_to_lin(-over * slope) } else { 1.0 };
                let k = if target < self.comp_gain { 1.0 - att } else { 1.0 - rel };
                self.comp_gain += (target - self.comp_gain) * k;
                let g = self.comp_gain * makeup;
                f[0] *= g;
                f[1] *= g;
            }
            comp_red = -lin_to_db(self.comp_gain);
        }

        let mut lim_red = 0.0;
        if on && s.limiter_enabled {
            let ceil = db_to_lin(s.limiter_ceiling_db);
            let rel = coef(s.limiter_release_ms);
            for f in x.chunks_exact_mut(2) {
                let peak = f[0].abs().max(f[1].abs());
                let need = if peak > ceil { ceil / peak } else { 1.0 };
                // Instant attack (no peak gets through), smooth release.
                self.lim_gain = if need < self.lim_gain { need } else { rel * self.lim_gain + (1.0 - rel) * need };
                f[0] = (f[0] * self.lim_gain).clamp(-ceil, ceil);
                f[1] = (f[1] * self.lim_gain).clamp(-ceil, ceil);
            }
            lim_red = -lin_to_db(self.lim_gain);
        }

        let mut delay_ms = 0.0;
        if on && s.delay_enabled && s.delay_ms > 0.0 {
            let want = ((s.delay_ms * 0.001 * RATE) as usize) * 2;
            if self.delay.len() != want {
                // Resize: pad with silence (longer) or skip ahead (shorter).
                while self.delay.len() < want {
                    self.delay.push_back(0.0);
                }
                while self.delay.len() > want {
                    self.delay.pop_front();
                }
            }
            for v in x.iter_mut() {
                self.delay.push_back(*v);
                *v = self.delay.pop_front().unwrap_or(0.0);
            }
            delay_ms = s.delay_ms;
        }

        let latency_ms = if self.denoiser.is_some() { NoiseSuppressor::LATENCY_FRAMES as f32 * 1000.0 / RATE } else { 0.0 } + delay_ms;
        self.stats = FxStats {
            vad,
            comp_reduction_db: comp_red,
            limiter_reduction_db: lim_red,
            deesser_reduction_db: ds_red,
            gate_closed,
            latency_ms,
            process_us: t0.elapsed().as_micros().min(u32::MAX as u128) as u32,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, amp: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE).sin();
                [v, v]
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn only(f: impl FnOnce(&mut FxSettings)) -> FxSettings {
        let mut s = FxSettings::default();
        f(&mut s);
        s
    }

    #[test]
    fn all_off_is_bit_exact_passthrough() {
        let mut c = FxChain::new(FxSettings::default());
        let mut x = tone(440.0, 0.5, 4800);
        let orig = x.clone();
        c.process(&mut x);
        assert_eq!(x, orig);
        assert!(!FxSettings::default().any_active());
    }

    #[test]
    fn bypass_switch_disables_every_effect() {
        let mut s = FxSettings::voice();
        s.denoise_enabled = false;
        s.enabled = false;
        let mut c = FxChain::new(s);
        let mut x = tone(440.0, 0.5, 4800);
        let orig = x.clone();
        c.process(&mut x);
        assert_eq!(x, orig);
    }

    #[test]
    fn polarity_and_mono() {
        let mut c = FxChain::new(only(|s| s.invert_polarity = true));
        let mut x = vec![0.5, 0.25];
        c.process(&mut x);
        assert_eq!(x, vec![-0.5, -0.25]);
        let mut c = FxChain::new(only(|s| s.mono = true));
        let mut x = vec![1.0, 0.0];
        c.process(&mut x);
        assert_eq!(x, vec![0.5, 0.5]);
    }

    #[test]
    fn low_cut_removes_rumble_keeps_voice() {
        let mut c = FxChain::new(only(|s| {
            s.highpass_enabled = true;
            s.highpass_hz = 150.0;
        }));
        let mut low = tone(30.0, 0.5, 48_000);
        c.process(&mut low);
        let mut c = FxChain::new(only(|s| {
            s.highpass_enabled = true;
            s.highpass_hz = 150.0;
        }));
        let mut mid = tone(1000.0, 0.5, 48_000);
        c.process(&mut mid);
        let half = 48_000;
        assert!(rms(&low[half..]) < 0.02, "30 Hz not removed: {}", rms(&low[half..]));
        assert!(rms(&mid[half..]) > 0.33, "1 kHz damaged: {}", rms(&mid[half..]));
    }

    #[test]
    fn high_cut_removes_hiss() {
        let mut c = FxChain::new(only(|s| {
            s.lowpass_enabled = true;
            s.lowpass_hz = 3000.0;
        }));
        let mut hi = tone(15_000.0, 0.5, 48_000);
        c.process(&mut hi);
        assert!(rms(&hi[48_000..]) < 0.03);
    }

    #[test]
    fn gate_closes_on_quiet_opens_on_loud() {
        let s = only(|s| {
            s.gate_enabled = true;
            s.gate_threshold_db = -30.0;
        });
        let mut c = FxChain::new(s.clone());
        let mut q = tone(440.0, 0.001, 48_000);
        c.process(&mut q);
        assert!(rms(&q[48_000..]) < 0.0002);
        assert!(c.stats().gate_closed);
        let mut c = FxChain::new(s);
        let mut l = tone(440.0, 0.5, 48_000);
        c.process(&mut l);
        assert!(rms(&l[48_000..]) > 0.3);
    }

    #[test]
    fn eq_mid_boost_raises_mid() {
        let mut c = FxChain::new(only(|s| {
            s.eq_enabled = true;
            s.eq_mid_db = 12.0;
            s.eq_mid_hz = 1000.0;
        }));
        let mut x = tone(1000.0, 0.1, 48_000);
        c.process(&mut x);
        assert!(rms(&x[48_000..]) > 0.1 * 0.707 * 3.0);
    }

    #[test]
    fn deesser_turns_down_sibilance_not_voice() {
        let s = only(|s| {
            s.deesser_enabled = true;
            s.deesser_hz = 5000.0;
            s.deesser_threshold_db = -30.0;
            s.deesser_max_db = 12.0;
        });
        let mut c = FxChain::new(s.clone());
        let mut ess = tone(8000.0, 0.5, 48_000);
        c.process(&mut ess);
        assert!(rms(&ess[48_000..]) < 0.5 * 0.707 * 0.5, "sibilance not reduced");
        assert!(c.stats().deesser_reduction_db > 3.0);
        let mut c = FxChain::new(s);
        let mut low = tone(300.0, 0.5, 48_000);
        c.process(&mut low);
        assert!(rms(&low[48_000..]) > 0.5 * 0.707 * 0.9, "voice band damaged");
    }

    #[test]
    fn compressor_ratio_and_makeup() {
        let mut c = FxChain::new(only(|s| {
            s.compressor_enabled = true;
            s.comp_threshold_db = -20.0;
            s.comp_ratio = 4.0;
        }));
        let mut x = vec![0.9f32; 96_000];
        c.process(&mut x);
        // 0.9 ≈ −0.9 dB, 19.1 dB over → 14.3 dB reduction at 4:1.
        assert!((c.stats().comp_reduction_db - 14.3).abs() < 1.0, "{}", c.stats().comp_reduction_db);
        let mut c = FxChain::new(only(|s| {
            s.compressor_enabled = true;
            s.comp_threshold_db = 0.0;
            s.comp_makeup_db = 6.0;
        }));
        let mut x = vec![0.1f32; 9600];
        c.process(&mut x);
        assert!((x[9000] - 0.1 * db_to_lin(6.0)).abs() < 1e-3);
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let mut c = FxChain::new(only(|s| {
            s.limiter_enabled = true;
            s.limiter_ceiling_db = -6.0;
        }));
        let mut x = tone(200.0, 1.5, 48_000);
        c.process(&mut x);
        let ceil = db_to_lin(-6.0);
        assert!(x.iter().all(|v| v.abs() <= ceil + 1e-6));
        assert!(c.stats().limiter_reduction_db > 0.0);
    }

    #[test]
    fn delay_shifts_by_exact_samples() {
        let mut c = FxChain::new(only(|s| {
            s.delay_enabled = true;
            s.delay_ms = 10.0;
        }));
        let mut x = vec![0.0f32; 2000 * 2];
        x[0] = 1.0;
        x[1] = 1.0;
        c.process(&mut x);
        let at = x.iter().position(|v| *v == 1.0).unwrap();
        assert_eq!(at, 480 * 2);
        assert!((c.stats().latency_ms - 10.0).abs() < 1e-3);
    }

    #[test]
    fn ai_stage_keeps_latency_after_switch_off() {
        let mut c = FxChain::new(only(|s| s.denoise_enabled = true));
        let mut x = vec![0.0f32; 3200];
        c.process(&mut x);
        assert!((c.stats().latency_ms - 20.0).abs() < 0.01);
        c.set_settings(FxSettings::default());
        c.process(&mut x);
        assert!(c.holds_latency());
        assert!((c.stats().latency_ms - 20.0).abs() < 0.01);
    }

    #[test]
    fn sanitize_clamps_and_rejects_nan() {
        let s = FxSettings {
            comp_ratio: 99.0,
            delay_ms: 5000.0,
            highpass_hz: f32::NAN,
            ..FxSettings::default()
        }
        .sanitized();
        assert_eq!(s.comp_ratio, 20.0);
        assert_eq!(s.delay_ms, MAX_DELAY_MS);
        assert_eq!(s.highpass_hz, 80.0);
    }

    #[test]
    fn voice_preset_matches_previous_enhancer_defaults() {
        let v = FxSettings::voice();
        assert!(v.denoise_enabled && v.gate_enabled && v.eq_enabled && v.compressor_enabled);
        assert_eq!((v.gate_threshold_db, v.eq_mid_db, v.eq_high_db, v.comp_ratio, v.comp_threshold_db), (-40.0, 2.0, 2.0, 3.0, -18.0));
        assert!(FxSettings::master().limiter_enabled);
    }
}
