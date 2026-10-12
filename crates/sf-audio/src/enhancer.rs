//! Real-time voice enhancer: AI noise suppression + noise gate + 3-band EQ +
//! compressor.
//!
//! Everything runs in-process on the audio tick — no cloud round-trip, no
//! keys, works offline. A network API cannot be used here: a live stream
//! needs every 33 ms audio block processed before the next one arrives, and
//! an HTTP round-trip (100–500 ms, plus jitter) would either add that much
//! delay to the whole stream or drop audio. The AI stage is RNNoise, a
//! neural network small enough to run per 10 ms frame on the CPU.
//!
//! Applied **per voice source** (microphones / camera audio), PRE-fader, at
//! the mixer's AI-FX insert point #1 — never to music/media files, which a
//! speech denoiser would damage.
//!
//! Chain order: **AI denoise → gate → EQ → compressor**.
//! * AI denoise (RNNoise): removes fans, hum, keyboard, room noise while
//!   keeping speech. Fixed 20 ms latency; strength 0–100 %.
//! * Noise gate: silences what's left between phrases. When AI denoise is
//!   on, the gate is voice-aware: RNNoise's voice-activity detector holds it
//!   open during soft speech that falls below the level threshold.
//! * 3-band EQ: lowshelf (200 Hz) / peaking (1 kHz) / highshelf (6 kHz),
//!   ±12 dB each — voice clarity lives in the mids/highs.
//! * Compressor: gentle 3:1 levelling so quiet and loud moments sit
//!   together (threshold −18 dB, 10 ms attack, 100 ms release).

/// User-facing enhancer settings.
#[derive(Clone, Debug)]
pub struct EnhancerSettings {
    pub enabled: bool,
    pub gate_enabled: bool,
    /// Gate threshold in dB (−60..0).
    pub gate_threshold_db: f32,
    /// EQ bands in dB (−12..+12).
    pub eq_low_db: f32,
    pub eq_mid_db: f32,
    pub eq_high_db: f32,
    pub compressor_enabled: bool,
    /// AI noise suppression (RNNoise) on/off.
    pub ai_denoise: bool,
    /// AI suppression strength, 0.0 (dry) ..= 1.0 (full).
    pub ai_strength: f32,
}

impl Default for EnhancerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            gate_enabled: true,
            gate_threshold_db: -40.0,
            eq_low_db: 0.0,
            eq_mid_db: 2.0,
            eq_high_db: 2.0,
            compressor_enabled: true,
            ai_denoise: true,
            ai_strength: 1.0,
        }
    }
}

/// Live telemetry from one enhancer (for meters / diagnostics).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EnhancerStats {
    /// Voice-activity probability from the AI stage (0..1; 0 when AI off).
    pub vad: f32,
    /// Current compressor gain reduction in dB (≥ 0).
    pub gain_reduction_db: f32,
    /// True while the gate is (mostly) closed.
    pub gate_closed: bool,
    /// Algorithmic latency added by the chain, in milliseconds.
    pub latency_ms: f32,
    /// Wall time of the last `process` call, microseconds.
    pub process_us: u32,
}

/// RBJ cookbook biquad (Direct Form I).
#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn passthrough() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn lowshelf(freq: f32, gain_db: f32, rate: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / rate;
        // Shelf slope S = 1.
        let alpha = w0.sin() / 2.0;
        let cw = w0.cos();
        let sq = a.sqrt();
        let (b0, b1, b2, a0, a1, a2) = (
            a * ((a + 1.0) - (a - 1.0) * cw + 2.0 * sq * alpha),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
            a * ((a + 1.0) - (a - 1.0) * cw - 2.0 * sq * alpha),
            (a + 1.0) + (a - 1.0) * cw + 2.0 * sq * alpha,
            -2.0 * ((a - 1.0) + (a + 1.0) * cw),
            (a + 1.0) + (a - 1.0) * cw - 2.0 * sq * alpha,
        );
        Self::from_coefs(b0, b1, b2, a0, a1, a2)
    }

    fn highshelf(freq: f32, gain_db: f32, rate: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / rate;
        let alpha = w0.sin() / 2.0;
        let cw = w0.cos();
        let sq = a.sqrt();
        let (b0, b1, b2, a0, a1, a2) = (
            a * ((a + 1.0) + (a - 1.0) * cw + 2.0 * sq * alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
            a * ((a + 1.0) + (a - 1.0) * cw - 2.0 * sq * alpha),
            (a + 1.0) - (a - 1.0) * cw + 2.0 * sq * alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cw),
            (a + 1.0) - (a - 1.0) * cw - 2.0 * sq * alpha,
        );
        Self::from_coefs(b0, b1, b2, a0, a1, a2)
    }

    fn peaking(freq: f32, gain_db: f32, q: f32, rate: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / rate;
        let alpha = w0.sin() / (2.0 * q);
        let cw = w0.cos();
        let (b0, b1, b2, a0, a1, a2) = (
            1.0 + alpha * a,
            -2.0 * cw,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cw,
            1.0 - alpha / a,
        );
        Self::from_coefs(b0, b1, b2, a0, a1, a2)
    }

    fn from_coefs(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// The enhancer: owns filter state across calls (stateful — do not
/// reconstruct per block or you'll hear clicks).
pub struct Enhancer {
    settings: EnhancerSettings,
    rate: f32,
    low: [Biquad; 2],
    mid: [Biquad; 2],
    high: [Biquad; 2],
    /// Gate envelope + open state per channel.
    gate_env: [f32; 2],
    gate_gain: [f32; 2],
    /// Compressor envelope (peak, mono-ized).
    comp_env: f32,
    comp_gain: f32,
    /// AI noise suppressor (created lazily on first use: ~40 KB of state).
    denoiser: Option<crate::denoise::NoiseSuppressor>,
    stats: EnhancerStats,
}

impl Enhancer {
    pub fn new(rate: u32) -> Self {
        let mut e = Self {
            settings: EnhancerSettings::default(),
            rate: rate as f32,
            low: [Biquad::passthrough(); 2],
            mid: [Biquad::passthrough(); 2],
            high: [Biquad::passthrough(); 2],
            gate_env: [0.0; 2],
            gate_gain: [1.0; 2],
            comp_env: 0.0,
            comp_gain: 1.0,
            denoiser: None,
            stats: EnhancerStats::default(),
        };
        e.retune();
        e
    }

    /// Apply new settings. Safe to call on every slider move: EQ state is
    /// preserved and AI strength is smoothed, so live changes don't click.
    pub fn set_settings(&mut self, mut s: EnhancerSettings) {
        s.ai_strength = s.ai_strength.clamp(0.0, 1.0);
        self.settings = s;
        self.retune();
        if let Some(d) = self.denoiser.as_mut() {
            let st = if self.settings.ai_denoise {
                self.settings.ai_strength
            } else {
                0.0
            };
            d.set_strength(st);
        }
    }

    /// Latest telemetry.
    pub fn stats(&self) -> EnhancerStats {
        self.stats
    }

    pub fn settings(&self) -> &EnhancerSettings {
        &self.settings
    }

    /// Rebuild EQ coefficients from settings (cheap; called on change).
    fn retune(&mut self) {
        let r = self.rate;
        let mk = |f: fn(f32, f32, f32) -> Biquad, freq, db: f32| {
            if db.abs() < 0.05 {
                Biquad::passthrough()
            } else {
                f(freq, db, r)
            }
        };
        let lo = mk(Biquad::lowshelf, 200.0, self.settings.eq_low_db);
        let hi = mk(Biquad::highshelf, 6000.0, self.settings.eq_high_db);
        let mid = if self.settings.eq_mid_db.abs() < 0.05 {
            Biquad::passthrough()
        } else {
            Biquad::peaking(1000.0, self.settings.eq_mid_db, 1.0, r)
        };
        // Preserve state where the shape didn't change? Simpler and
        // click-free enough for slider moves: keep existing state by
        // only replacing coefficients is complex; rebuild resets state
        // (a tiny click on slider release is acceptable, no zipper
        // noise during the drag since retune happens on change end).
        for ch in 0..2 {
            let (lx1, lx2, ly1, ly2) = (
                self.low[ch].x1,
                self.low[ch].x2,
                self.low[ch].y1,
                self.low[ch].y2,
            );
            self.low[ch] = lo;
            self.low[ch].x1 = lx1;
            self.low[ch].x2 = lx2;
            self.low[ch].y1 = ly1;
            self.low[ch].y2 = ly2;
            let (mx1, mx2, my1, my2) = (
                self.mid[ch].x1,
                self.mid[ch].x2,
                self.mid[ch].y1,
                self.mid[ch].y2,
            );
            self.mid[ch] = mid;
            self.mid[ch].x1 = mx1;
            self.mid[ch].x2 = mx2;
            self.mid[ch].y1 = my1;
            self.mid[ch].y2 = my2;
            let (hx1, hx2, hy1, hy2) = (
                self.high[ch].x1,
                self.high[ch].x2,
                self.high[ch].y1,
                self.high[ch].y2,
            );
            self.high[ch] = hi;
            self.high[ch].x1 = hx1;
            self.high[ch].x2 = hx2;
            self.high[ch].y1 = hy1;
            self.high[ch].y2 = hy2;
        }
    }

    /// Process interleaved stereo f32 in place (48 kHz expected, but any
    /// rate works — `rate` was fixed at construction).
    pub fn process(&mut self, samples: &mut [f32]) {
        if !self.settings.enabled || samples.is_empty() {
            self.stats = EnhancerStats::default();
            return;
        }
        let t0 = std::time::Instant::now();
        // --- AI noise suppression (RNNoise) ---
        // Once created, the denoiser keeps running even when AI is toggled
        // off (strength 0 skips the network but keeps the 20 ms delay line),
        // so toggling never shifts A/V sync mid-stream.
        if self.settings.ai_denoise && self.denoiser.is_none() {
            let mut d = crate::denoise::NoiseSuppressor::new();
            d.set_strength(self.settings.ai_strength);
            self.denoiser = Some(d);
        }
        let (vad, latency_ms) = match self.denoiser.as_mut() {
            Some(d) => {
                d.process(samples);
                let v = if self.settings.ai_denoise {
                    d.vad()
                } else {
                    0.0
                };
                (
                    v,
                    crate::denoise::NoiseSuppressor::LATENCY_FRAMES as f32 * 1000.0 / self.rate,
                )
            }
            None => (0.0, 0.0),
        };
        // Voice-aware gate: speech detected by the RNN holds the gate open.
        let voice = self.settings.ai_denoise && vad > 0.5;
        let gate_on = self.settings.gate_enabled;
        let comp_on = self.settings.compressor_enabled;
        let gate_thr = 10f32.powf(self.settings.gate_threshold_db / 20.0);
        // Gate timing: 5 ms attack, 120 ms release.
        let g_att = (-1.0 / (0.005 * self.rate)).exp();
        let g_rel = (-1.0 / (0.120 * self.rate)).exp();
        // Compressor: −18 dB threshold, 3:1 ratio, 10 ms attack, 100 ms release.
        let c_thr_db = -18.0;
        let c_att = (-1.0 / (0.010 * self.rate)).exp();
        let c_rel = (-1.0 / (0.100 * self.rate)).exp();

        for frame in samples.chunks_exact_mut(2) {
            // --- noise gate (per channel) ---
            if gate_on {
                for ch in 0..2 {
                    let x = frame[ch].abs();
                    let env = &mut self.gate_env[ch];
                    *env = if x > *env {
                        g_att * *env + (1.0 - g_att) * x
                    } else {
                        g_rel * *env + (1.0 - g_rel) * x
                    };
                    let target = if *env > gate_thr || voice { 1.0 } else { 0.0 };
                    let g = &mut self.gate_gain[ch];
                    // 5 ms smoothing on the gain itself avoids clicks.
                    *g += (target - *g) * (1.0 - g_att);
                    frame[ch] *= *g;
                }
            }
            // --- 3-band EQ ---
            for ch in 0..2 {
                let mut v = frame[ch];
                v = self.low[ch].process(v);
                v = self.mid[ch].process(v);
                v = self.high[ch].process(v);
                frame[ch] = v;
            }
            // --- compressor (peak envelope of the hotter channel) ---
            if comp_on {
                let peak = frame[0].abs().max(frame[1].abs()).max(1e-6);
                self.comp_env = if peak > self.comp_env {
                    c_att * self.comp_env + (1.0 - c_att) * peak
                } else {
                    c_rel * self.comp_env + (1.0 - c_rel) * peak
                };
                let env_db = 20.0 * self.comp_env.log10();
                let target_gain_db = if env_db > c_thr_db {
                    (c_thr_db - env_db) * (1.0 - 1.0 / 3.0)
                } else {
                    0.0
                };
                let target = 10f32.powf(target_gain_db / 20.0);
                self.comp_gain += (target - self.comp_gain) * (1.0 - c_att);
                frame[0] *= self.comp_gain;
                frame[1] *= self.comp_gain;
            }
        }
        self.stats = EnhancerStats {
            vad,
            gain_reduction_db: if comp_on {
                -20.0 * self.comp_gain.max(1e-6).log10()
            } else {
                0.0
            },
            gate_closed: gate_on && self.gate_gain[0].max(self.gate_gain[1]) < 0.5,
            latency_ms,
            process_us: t0.elapsed().as_micros().min(u32::MAX as u128) as u32,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_passthrough() {
        let mut e = Enhancer::new(48_000);
        e.set_settings(EnhancerSettings {
            enabled: false,
            ..EnhancerSettings::default()
        });
        let mut s = vec![0.5f32, -0.5, 0.25, -0.25];
        let orig = s.clone();
        e.process(&mut s);
        assert_eq!(s, orig);
    }

    #[test]
    fn gate_silences_quiet_signal() {
        let mut e = Enhancer::new(48_000);
        let mut st = EnhancerSettings::default();
        st.enabled = true;
        st.ai_denoise = false;
        st.gate_enabled = true;
        st.gate_threshold_db = -20.0;
        st.compressor_enabled = false;
        e.set_settings(st);
        // Very quiet signal, well under the gate → should fade to ~0.
        let mut s = vec![0.001f32; 48_000 * 2];
        e.process(&mut s);
        let half = s.len() / 2;
        let peak = s[half..].iter().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(peak < 0.0005, "gate did not close, peak={peak}");
    }

    #[test]
    fn gate_passes_loud_signal() {
        let mut e = Enhancer::new(48_000);
        let mut st = EnhancerSettings::default();
        st.enabled = true;
        st.ai_denoise = false;
        st.gate_enabled = true;
        st.gate_threshold_db = -40.0;
        st.compressor_enabled = false;
        e.set_settings(st);
        // Loud 440 Hz-ish tone: alternate samples.
        let mut s: Vec<f32> = (0..48_000)
            .flat_map(|i| [0.5 * (i as f32 * 0.1).sin(); 2])
            .collect();
        e.process(&mut s);
        let peak = s.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(peak > 0.2, "gate closed on loud signal, peak={peak}");
    }

    #[test]
    fn eq_boost_changes_spectrum() {
        let mut e = Enhancer::new(48_000);
        let mut st = EnhancerSettings::default();
        st.enabled = true;
        st.ai_denoise = false;
        st.gate_enabled = false;
        st.compressor_enabled = false;
        st.eq_high_db = 12.0;
        e.set_settings(st);
        // High-frequency tone (10 kHz): alternating ±.
        let mut s: Vec<f32> = (0..4800)
            .flat_map(|i| [if i % 2 == 0 { 0.3 } else { -0.3 }; 2])
            .collect();
        let before: f32 = s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32;
        e.process(&mut s);
        let after: f32 = s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32;
        assert!(after > before * 2.0, "high shelf did not boost highs");
    }

    #[test]
    fn compressor_tames_peaks() {
        let mut e = Enhancer::new(48_000);
        let mut st = EnhancerSettings::default();
        st.enabled = true;
        st.ai_denoise = false;
        st.gate_enabled = false;
        st.compressor_enabled = true;
        e.set_settings(st);
        // Loud block: 0.9 peaks should be pulled down.
        let mut s = vec![0.9f32; 48_000 * 2];
        e.process(&mut s);
        let half = s.len() / 2;
        let peak = s[half..].iter().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(peak < 0.85, "compressor did not reduce peaks, peak={peak}");
    }

    #[test]
    fn ai_chain_removes_noise_and_reports_stats() {
        let mut e = Enhancer::new(48_000);
        let mut st = EnhancerSettings::default();
        st.gate_enabled = false;
        st.compressor_enabled = false;
        st.eq_mid_db = 0.0;
        st.eq_high_db = 0.0;
        e.set_settings(st);
        let mut seed = 7u32;
        let mut s: Vec<f32> = (0..48_000 * 2)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                ((seed as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.05
            })
            .flat_map(|v| [v, v])
            .collect();
        let before: f32 = s[s.len() / 2..].iter().map(|v| v * v).sum();
        for c in s.chunks_mut(3200) {
            e.process(c);
        }
        let after: f32 = s[s.len() / 2..].iter().map(|v| v * v).sum();
        assert!(after < before * 0.1, "AI stage did not suppress noise");
        assert!((e.stats().latency_ms - 20.0).abs() < 0.01);
    }

    #[test]
    fn toggling_ai_keeps_latency_constant() {
        let mut e = Enhancer::new(48_000);
        let mut st = EnhancerSettings::default();
        st.gate_enabled = false;
        st.compressor_enabled = false;
        e.set_settings(st.clone());
        let mut b = vec![0.0f32; 3200];
        e.process(&mut b);
        st.ai_denoise = false;
        e.set_settings(st);
        e.process(&mut b);
        assert!((e.stats().latency_ms - 20.0).abs() < 0.01);
    }
}
