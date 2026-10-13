//! Real-time AI noise suppression for live voice (RNNoise).
//!
//! Wraps the mono [`crate::rnnoise::DenoiseState`] for the engine's audio
//! contract (48 kHz stereo interleaved f32, arbitrary block sizes):
//!
//! * **Constant latency.** RNNoise works on 480-sample (10 ms) frames and its
//!   overlap-add adds one more frame, so the processor is primed with 10 ms
//!   of silence: every call returns exactly as many samples as it received,
//!   always delayed by exactly [`NoiseSuppressor::LATENCY_FRAMES`] (20 ms).
//!   A constant delay keeps A/V sync stable — nothing ever accumulates.
//! * **Strength.** `0.0..=1.0` blends the denoised signal with the
//!   latency-aligned dry signal (so 50 % really is "half the noise
//!   removal", with no comb filtering). Changes are smoothed per sample, so
//!   dragging the slider live never clicks. At strength 0 the RNN is skipped
//!   entirely (no CPU), but the delay line keeps running so latency does not
//!   jump.
//! * **Mono fast path.** Most mics are mono duplicated to both channels: while
//!   L == R only one RNN runs (half the CPU). The first truly stereo frame
//!   switches to two RNNs permanently (sticky, to avoid state ping-pong).
//! * **Voice activity.** RNNoise's own VAD probability is exposed (for the UI
//!   meter and for the enhancer's voice-aware gate).
//!
//! No allocation happens on the processing path after construction.

use std::collections::VecDeque;

use crate::rnnoise::{DenoiseState, FRAME_SIZE};

/// RNNoise expects 16-bit-scale floats.
const PCM_SCALE: f32 = 32768.0;

/// Stereo real-time RNNoise processor.
pub struct NoiseSuppressor {
    states: [Box<DenoiseState>; 2],
    /// Per-channel input accumulators (always < FRAME_SIZE between calls).
    acc: [Vec<f32>; 2],
    /// Denoised interleaved output waiting to be handed out.
    wet: VecDeque<f32>,
    /// Dry interleaved signal delayed by exactly the wet path's latency.
    dry: VecDeque<f32>,
    out_l: [f32; FRAME_SIZE],
    out_r: [f32; FRAME_SIZE],
    stereo: bool,
    target_strength: f32,
    strength: f32,
    vad: f32,
}

impl NoiseSuppressor {
    /// Total algorithmic latency in sample frames (20 ms at 48 kHz).
    pub const LATENCY_FRAMES: usize = 2 * FRAME_SIZE;

    pub fn new() -> Self {
        let mut wet = VecDeque::with_capacity(FRAME_SIZE * 2 * 16);
        // Prime one frame: guarantees output availability for any block
        // size (see module docs), giving a fixed 10 ms framing delay.
        wet.extend(std::iter::repeat(0.0).take(FRAME_SIZE * 2));
        let mut dry = VecDeque::with_capacity(Self::LATENCY_FRAMES * 2 + FRAME_SIZE * 2 * 16);
        dry.extend(std::iter::repeat(0.0).take(Self::LATENCY_FRAMES * 2));
        Self {
            states: [DenoiseState::new(), DenoiseState::new()],
            acc: [
                Vec::with_capacity(FRAME_SIZE),
                Vec::with_capacity(FRAME_SIZE),
            ],
            wet,
            dry,
            out_l: [0.0; FRAME_SIZE],
            out_r: [0.0; FRAME_SIZE],
            stereo: false,
            target_strength: 1.0,
            strength: 1.0,
            vad: 0.0,
        }
    }

    /// Set suppression strength (`0.0` = off/dry, `1.0` = full). Applied
    /// with per-sample smoothing.
    pub fn set_strength(&mut self, s: f32) {
        self.target_strength = s.clamp(0.0, 1.0);
    }

    /// Latest voice-activity probability (0..1), lightly smoothed.
    pub fn vad(&self) -> f32 {
        self.vad
    }

    /// True once a genuinely stereo signal has been seen.
    pub fn is_stereo(&self) -> bool {
        self.stereo
    }

    /// Process interleaved stereo in place. Output is the input delayed by
    /// [`Self::LATENCY_FRAMES`], denoised according to the strength.
    pub fn process(&mut self, samples: &mut [f32]) {
        if samples.len() < 2 {
            return;
        }
        let run_rnn = self.target_strength > 0.0 || self.strength > 1e-4;
        for frame in samples.chunks_exact(2) {
            self.dry.push_back(frame[0]);
            self.dry.push_back(frame[1]);
            self.acc[0].push(frame[0] * PCM_SCALE);
            self.acc[1].push(frame[1] * PCM_SCALE);
            if self.acc[0].len() == FRAME_SIZE {
                self.process_frame(run_rnn);
                self.acc[0].clear();
                self.acc[1].clear();
            }
        }
        // Smoothing coefficient: ~10 ms time constant at 48 kHz.
        const K: f32 = 1.0 / 480.0;
        for frame in samples.chunks_exact_mut(2) {
            // Both queues are long enough by construction (priming).
            let wl = self.wet.pop_front().unwrap_or(0.0);
            let wr = self.wet.pop_front().unwrap_or(0.0);
            let dl = self.dry.pop_front().unwrap_or(0.0);
            let dr = self.dry.pop_front().unwrap_or(0.0);
            self.strength += (self.target_strength - self.strength) * K;
            let s = self.strength;
            frame[0] = wl * s + dl * (1.0 - s);
            frame[1] = wr * s + dr * (1.0 - s);
        }
    }

    fn process_frame(&mut self, run_rnn: bool) {
        if !run_rnn {
            // Strength 0: keep timing, skip the network.
            self.wet.extend(std::iter::repeat(0.0).take(FRAME_SIZE * 2));
            self.vad *= 0.9;
            return;
        }
        if !self.stereo {
            let diff = self.acc[0]
                .iter()
                .zip(&self.acc[1])
                .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
            // > ~-90 dBFS difference ⇒ real stereo content.
            if diff > 1.0 {
                self.stereo = true;
            }
        }
        let vad_l = self.states[0].process_frame(&mut self.out_l, &self.acc[0]);
        let vad = if self.stereo {
            let vad_r = self.states[1].process_frame(&mut self.out_r, &self.acc[1]);
            vad_l.max(vad_r)
        } else {
            self.out_r = self.out_l;
            vad_l
        };
        // Fast attack, slower release so the meter/gate don't flicker.
        self.vad = if vad > self.vad {
            vad
        } else {
            self.vad * 0.8 + vad * 0.2
        };
        for i in 0..FRAME_SIZE {
            self.wet
                .push_back((self.out_l[i] / PCM_SCALE).clamp(-1.0, 1.0));
            self.wet
                .push_back((self.out_r[i] / PCM_SCALE).clamp(-1.0, 1.0));
        }
    }
}

impl Default for NoiseSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white-ish noise in -1..1 (xorshift).
    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn output_length_always_equals_input_length() {
        let mut ns = NoiseSuppressor::new();
        for &frames in &[1usize, 7, 480, 1600, 333, 2048, 800] {
            let mut block = vec![0.1f32; frames * 2];
            ns.process(&mut block);
            assert_eq!(block.len(), frames * 2);
        }
    }

    #[test]
    fn latency_is_exactly_twenty_ms_at_zero_strength() {
        let mut ns = NoiseSuppressor::new();
        ns.set_strength(0.0);
        ns.strength = 0.0;
        // Impulse at frame 0, processed in odd block sizes.
        let total = 3000;
        let mut input = vec![0.0f32; total * 2];
        input[0] = 1.0;
        input[1] = 1.0;
        let mut out = Vec::new();
        for chunk in input.chunks(1601 * 2) {
            let mut c = chunk.to_vec();
            ns.process(&mut c);
            out.extend(c);
        }
        let pos = out.iter().position(|&v| v > 0.5).unwrap() / 2;
        assert_eq!(pos, NoiseSuppressor::LATENCY_FRAMES);
    }

    #[test]
    fn strongly_attenuates_stationary_noise() {
        let mut ns = NoiseSuppressor::new();
        let n = 48_000 * 3; // 3 s
        let mono: Vec<f32> = noise(n, 1234).iter().map(|v| v * 0.05).collect();
        let mut block: Vec<f32> = mono.iter().flat_map(|&v| [v, v]).collect();
        let before = rms(&block[block.len() / 2..]);
        // Feed in 33 ms ticks like the media tick does.
        for chunk in block.chunks_mut(1600 * 2) {
            ns.process(chunk);
        }
        let after = rms(&block[block.len() / 2..]);
        assert!(
            after < before * 0.3,
            "noise not suppressed enough: before {before}, after {after}"
        );
        assert!(
            !ns.is_stereo(),
            "dual-mono input must use the mono fast path"
        );
    }

    #[test]
    fn half_strength_sits_between_dry_and_full() {
        let n = 48_000 * 2;
        let mono: Vec<f32> = noise(n, 99).iter().map(|v| v * 0.05).collect();
        let run = |s: f32| {
            let mut ns = NoiseSuppressor::new();
            ns.set_strength(s);
            ns.strength = s;
            let mut b: Vec<f32> = mono.iter().flat_map(|&v| [v, v]).collect();
            for c in b.chunks_mut(1600 * 2) {
                ns.process(c);
            }
            rms(&b[b.len() / 2..])
        };
        let (dry, half, full) = (run(0.0), run(0.5), run(1.0));
        assert!(
            full < half && half < dry,
            "dry {dry} half {half} full {full}"
        );
    }

    #[test]
    fn detects_true_stereo() {
        let mut ns = NoiseSuppressor::new();
        let l = noise(4800, 1);
        let r = noise(4800, 2);
        let mut b: Vec<f32> = l
            .iter()
            .zip(&r)
            .flat_map(|(&a, &b)| [a * 0.1, b * 0.1])
            .collect();
        ns.process(&mut b);
        assert!(ns.is_stereo());
    }
}
