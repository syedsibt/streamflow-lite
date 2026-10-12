//! RNNoise — recurrent-neural-network noise suppression (Xiph.Org), vendored
//! from the pure-Rust port `nnnoiseless` 0.5.2 (BSD-3-Clause, see `COPYING`).
//!
//! Why vendored instead of a crates.io dependency:
//! * `sf-audio` is intentionally dependency-free (it is the real-time core).
//! * Upstream pulls `easyfft → rustfft/realfft/generic_singleton/...`; the only
//!   thing it needs from them is a 960-point real FFT, which [`fft`] provides.
//!
//! What changed vs. upstream: `easyfft::DynRealDft` is replaced by a plain
//! `[Complex; FREQ_SIZE]` half-spectrum plus [`fft::RealFft`]. The model,
//! weights, feature extraction, pitch filter and RNN are unchanged, and the
//! `reference_output` test (Xiph's reference vectors) still passes.
//!
//! Contract: 48 kHz mono, frames of [`FRAME_SIZE`] (480 = 10 ms) samples, in
//! 16-bit *scale* (±32768) f32. Output lags input by one frame (overlap-add).

#![allow(clippy::needless_range_loop, dead_code, unexpected_cfgs)]

mod fft;
mod pitch;
mod rnn;
mod util;

use std::borrow::Cow;
use std::sync::OnceLock;

use fft::{Complex, RealFft};
pub use rnn::RnnModel;

pub const FRAME_SIZE_SHIFT: usize = 2;
/// Samples per RNNoise frame (10 ms at 48 kHz).
pub const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;
pub(crate) const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
pub(crate) const FREQ_SIZE: usize = FRAME_SIZE + 1;

pub(crate) const PITCH_MIN_PERIOD: usize = 60;
pub(crate) const PITCH_MAX_PERIOD: usize = 768;
pub(crate) const PITCH_FRAME_SIZE: usize = 960;
pub(crate) const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;

pub(crate) const NB_BANDS: usize = 22;
pub(crate) const CEPS_MEM: usize = 8;
const NB_DELTA_CEPS: usize = 6;
pub(crate) const NB_FEATURES: usize = NB_BANDS + 3 * NB_DELTA_CEPS + 2;
const EBAND_5MS: [usize; 22] = [
    // 0  200 400 600 800  1k 1.2 1.4 1.6  2k 2.4 2.8 3.2  4k 4.8 5.6 6.8  8k 9.6 12k 15.6 20k*/
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];

struct Tables {
    window: [f32; WINDOW_SIZE],
    wnorm: f32,
    dct: [f32; NB_BANDS * NB_BANDS],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let pi = std::f64::consts::PI;
        let mut window = [0.0f32; WINDOW_SIZE];
        for i in 0..FRAME_SIZE {
            let sin = (0.5 * pi * (i as f64 + 0.5) / FRAME_SIZE as f64).sin();
            let w = (0.5 * pi * sin * sin).sin() as f32;
            window[i] = w;
            window[WINDOW_SIZE - i - 1] = w;
        }
        let wnorm = 1.0 / window.iter().map(|x| x * x).sum::<f32>();
        let mut dct = [0.0f32; NB_BANDS * NB_BANDS];
        for i in 0..NB_BANDS {
            for j in 0..NB_BANDS {
                dct[i * NB_BANDS + j] =
                    ((i as f64 + 0.5) * j as f64 * pi / NB_BANDS as f64).cos() as f32;
                if j == 0 {
                    dct[i * NB_BANDS + j] *= 0.5f32.sqrt();
                }
            }
        }
        Tables { window, wnorm, dct }
    })
}

fn default_model() -> &'static RnnModel {
    static M: OnceLock<RnnModel> = OnceLock::new();
    M.get_or_init(RnnModel::default)
}

/// Band-aggregated correlation of two half-spectra (`out.len() == NB_BANDS`).
fn compute_band_corr(out: &mut [f32], x: &[Complex], p: &[Complex]) {
    out.iter_mut().for_each(|y| *y = 0.0);
    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            let corr = x[idx].re * p[idx].re + x[idx].im * p[idx].im;
            out[i] += (1.0 - frac) * corr;
            out[i + 1] += frac * corr;
        }
    }
    out[0] *= 2.0;
    out[NB_BANDS - 1] *= 2.0;
}

fn interp_band_gain(out: &mut [f32], band_e: &[f32]) {
    out.iter_mut().for_each(|y| *y = 0.0);
    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            out[idx] = (1.0 - frac) * band_e[i] + frac * band_e[i + 1];
        }
    }
}

/// Brute-force DCT of size NB_BANDS.
fn dct(out: &mut [f32], x: &[f32]) {
    let t = &tables().dct;
    for i in 0..NB_BANDS {
        let mut sum = 0.0;
        for j in 0..NB_BANDS {
            sum += x[j] * t[j * NB_BANDS + i];
        }
        out[i] = (sum as f64 * (2.0 / NB_BANDS as f64).sqrt()) as f32;
    }
}

const fn max(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

/// Feature extraction + synthesis state (upstream `DenoiseFeatures`).
#[derive(Clone)]
struct Features {
    input_mem: [f32; max(FRAME_SIZE, PITCH_BUF_SIZE)],
    cepstral_mem: [[f32; NB_BANDS]; CEPS_MEM],
    mem_id: usize,
    mem_hp_x: [f32; 2],
    synthesis_mem: [f32; FRAME_SIZE],
    window_buf: [f32; WINDOW_SIZE],
    /// Half-spectrum of the current frame (DC at 0, Nyquist at FRAME_SIZE).
    x: [Complex; FREQ_SIZE],
    /// Half-spectrum of the pitch-lagged frame.
    p: [Complex; FREQ_SIZE],
    ex: [f32; NB_BANDS],
    ep: [f32; NB_BANDS],
    exp: [f32; NB_BANDS],
    features: [f32; NB_FEATURES],
    pitch_finder: pitch::PitchFinder,
}

impl Features {
    fn new() -> Self {
        Self {
            input_mem: [0.0; max(FRAME_SIZE, PITCH_BUF_SIZE)],
            cepstral_mem: [[0.0; NB_BANDS]; CEPS_MEM],
            mem_id: 0,
            mem_hp_x: [0.0; 2],
            synthesis_mem: [0.0; FRAME_SIZE],
            window_buf: [0.0; WINDOW_SIZE],
            x: [Complex::default(); FREQ_SIZE],
            p: [Complex::default(); FREQ_SIZE],
            ex: [0.0; NB_BANDS],
            ep: [0.0; NB_BANDS],
            exp: [0.0; NB_BANDS],
            features: [0.0; NB_FEATURES],
            pitch_finder: pitch::PitchFinder::new(),
        }
    }

    fn shift_and_filter_input(&mut self, input: &[f32]) {
        assert!(input.len() == FRAME_SIZE);
        let new_idx = self.input_mem.len() - FRAME_SIZE;
        self.input_mem.copy_within(FRAME_SIZE.., 0);
        util::BIQUAD_HP.filter(&mut self.input_mem[new_idx..], &mut self.mem_hp_x, input);
    }

    fn transform_input(
        fft: &mut RealFft,
        input: &[f32],
        lag: usize,
        window_buf: &mut [f32; WINDOW_SIZE],
        x: &mut [Complex; FREQ_SIZE],
        ex: &mut [f32],
    ) {
        let t = tables();
        let input = &input[input.len() - (WINDOW_SIZE + lag)..];
        for ((o, &i), &w) in window_buf.iter_mut().zip(input).zip(&t.window) {
            *o = i * w;
        }
        fft.forward(&window_buf[..], &mut x[..]);
        // RNNoise normalizes the forward transform; ours is unnormalized.
        for c in x.iter_mut() {
            c.re *= t.wnorm;
            c.im *= t.wnorm;
        }
        compute_band_corr(ex, &x[..], &x[..]);
    }

    /// Returns `true` if the frame was (near) silent.
    fn compute_frame_features(&mut self, fft: &mut RealFft) -> bool {
        let mut ly = [0.0; NB_BANDS];
        let mut tmp = [0.0; NB_BANDS];

        Self::transform_input(
            fft,
            &self.input_mem,
            0,
            &mut self.window_buf,
            &mut self.x,
            &mut self.ex,
        );
        let pitch_idx = {
            let input = &self.input_mem[self.input_mem.len() - PITCH_BUF_SIZE..];
            self.pitch_finder.process(input).0
        };
        Self::transform_input(
            fft,
            &self.input_mem,
            pitch_idx,
            &mut self.window_buf,
            &mut self.p,
            &mut self.ep,
        );
        compute_band_corr(&mut self.exp[..], &self.x[..], &self.p[..]);
        for i in 0..NB_BANDS {
            self.exp[i] /= (0.001 + self.ex[i] * self.ep[i]).sqrt();
        }
        dct(&mut tmp[..], &self.exp[..]);
        for i in 0..NB_DELTA_CEPS {
            self.features[NB_BANDS + 2 * NB_DELTA_CEPS + i] = tmp[i];
        }
        self.features[NB_BANDS + 2 * NB_DELTA_CEPS] -= 1.3;
        self.features[NB_BANDS + 2 * NB_DELTA_CEPS + 1] -= 0.9;
        self.features[NB_BANDS + 3 * NB_DELTA_CEPS] = 0.01 * (pitch_idx as f32 - 300.0);
        let mut log_max = -2.0;
        let mut follow = -2.0;
        let mut e = 0.0;
        for i in 0..NB_BANDS {
            ly[i] = (1e-2 + self.ex[i])
                .log10()
                .max(log_max - 7.0)
                .max(follow - 1.5);
            log_max = f32::max(log_max, ly[i]);
            follow = f32::max(follow - 1.5, ly[i]);
            e += self.ex[i];
        }
        if e < 0.04 {
            // No audio: avoid messing up the state.
            self.features.iter_mut().for_each(|f| *f = 0.0);
            return true;
        }
        dct(&mut self.features, &ly[..]);
        self.features[0] -= 12.0;
        self.features[1] -= 4.0;
        let ceps_0_idx = self.mem_id;
        let ceps_1_idx = if self.mem_id < 1 {
            CEPS_MEM + self.mem_id - 1
        } else {
            self.mem_id - 1
        };
        let ceps_2_idx = if self.mem_id < 2 {
            CEPS_MEM + self.mem_id - 2
        } else {
            self.mem_id - 2
        };
        for i in 0..NB_BANDS {
            self.cepstral_mem[ceps_0_idx][i] = self.features[i];
        }
        self.mem_id += 1;
        let ceps_0 = self.cepstral_mem[ceps_0_idx];
        let ceps_1 = self.cepstral_mem[ceps_1_idx];
        let ceps_2 = self.cepstral_mem[ceps_2_idx];
        for i in 0..NB_DELTA_CEPS {
            self.features[i] = ceps_0[i] + ceps_1[i] + ceps_2[i];
            self.features[NB_BANDS + i] = ceps_0[i] - ceps_2[i];
            self.features[NB_BANDS + NB_DELTA_CEPS + i] = ceps_0[i] - 2.0 * ceps_1[i] + ceps_2[i];
        }
        // Spectral variability features.
        let mut spec_variability = 0.0;
        if self.mem_id == CEPS_MEM {
            self.mem_id = 0;
        }
        for i in 0..CEPS_MEM {
            let mut min_dist = 1e15f32;
            for j in 0..CEPS_MEM {
                let mut dist = 0.0;
                for k in 0..NB_BANDS {
                    let d = self.cepstral_mem[i][k] - self.cepstral_mem[j][k];
                    dist += d * d;
                }
                if j != i {
                    min_dist = min_dist.min(dist);
                }
            }
            spec_variability += min_dist;
        }
        self.features[NB_BANDS + 3 * NB_DELTA_CEPS + 1] = spec_variability / CEPS_MEM as f32 - 2.1;
        false
    }

    /// Attenuates pitches poorly correlated with the pitch-lagged signal.
    fn pitch_filter(&mut self, gain: &[f32; NB_BANDS]) {
        let mut r = [0.0; NB_BANDS];
        let mut rf = [0.0; FREQ_SIZE];
        for i in 0..NB_BANDS {
            r[i] = if self.exp[i] > gain[i] {
                1.0
            } else {
                let exp_sq = self.exp[i] * self.exp[i];
                let g_sq = gain[i] * gain[i];
                exp_sq * (1.0 - g_sq) / (0.001 + g_sq * (1.0 - exp_sq))
            };
            r[i] = r[i].clamp(0.0, 1.0).sqrt();
            r[i] *= (self.ex[i] / (1e-8 + self.ep[i])).sqrt();
        }
        interp_band_gain(&mut rf[..], &r[..]);
        for i in 0..FREQ_SIZE {
            self.x[i].re += self.p[i].re * rf[i];
            self.x[i].im += self.p[i].im * rf[i];
        }
        let mut new_e = [0.0; NB_BANDS];
        compute_band_corr(&mut new_e[..], &self.x, &self.x);
        for i in 0..NB_BANDS {
            r[i] = (self.ex[i] / (1e-8 + new_e[i])).sqrt();
        }
        interp_band_gain(&mut rf[..], &r[..]);
        self.apply_gain(&rf);
    }

    fn apply_gain(&mut self, gain: &[f32; FREQ_SIZE]) {
        for (c, &g) in self.x.iter_mut().zip(gain.iter()) {
            c.re *= g;
            c.im *= g;
        }
    }

    fn frame_synthesis(&mut self, fft: &mut RealFft, out: &mut [f32]) {
        fft.inverse(&self.x, &mut self.window_buf);
        let w = &tables().window;
        for (x, &wi) in self.window_buf.iter_mut().zip(w) {
            // Upstream's /2 compensates the 2/N forward scaling (WNORM).
            *x = *x / 2.0 * wi;
        }
        for i in 0..FRAME_SIZE {
            out[i] = self.window_buf[i] + self.synthesis_mem[i];
            self.synthesis_mem[i] = self.window_buf[FRAME_SIZE + i];
        }
    }
}

/// One mono RNNoise denoiser instance (upstream `DenoiseState`).
///
/// Large (~20 KB) — keep it boxed.
pub struct DenoiseState {
    lastg: [f32; NB_BANDS],
    rnn: rnn::RnnState<'static>,
    feat: Features,
    fft: RealFft,
}

impl DenoiseState {
    /// Samples per call to [`DenoiseState::process_frame`].
    pub const FRAME_SIZE: usize = FRAME_SIZE;

    /// New denoiser using the built-in RNNoise model (shared, loaded once).
    pub fn new() -> Box<Self> {
        Box::new(Self {
            lastg: [0.0; NB_BANDS],
            rnn: rnn::RnnState::new(Cow::Borrowed(default_model())),
            feat: Features::new(),
            fft: RealFft::new(WINDOW_SIZE),
        })
    }

    /// Denoise one frame. `input`/`output` are `FRAME_SIZE` samples of
    /// 48 kHz mono PCM in 16-bit scale. Returns the RNN's voice-activity
    /// probability for this frame (0..1).
    pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32 {
        let mut g = [0.0; NB_BANDS];
        let mut gf = [1.0; FREQ_SIZE];
        let mut vad_prob = [0.0];

        self.feat.shift_and_filter_input(input);
        let silence = self.feat.compute_frame_features(&mut self.fft);
        if !silence {
            self.rnn
                .compute(&mut g[..], &mut vad_prob[..], &self.feat.features[..]);
            self.feat.pitch_filter(&g);
            for i in 0..NB_BANDS {
                g[i] = g[i].max(0.6 * self.lastg[i]);
                self.lastg[i] = g[i];
            }
            interp_band_gain(&mut gf[..], &g[..]);
            self.feat.apply_gain(&gf);
        }
        self.feat.frame_synthesis(&mut self.fft, output);
        vad_prob[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_f32(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(2)
            .map(|x| i16::from_le_bytes([x[0], x[1]]) as f32)
            .collect()
    }

    /// Xiph's reference vectors: our port (with the custom FFT) must match
    /// the C implementation's output to within upstream's tolerance.
    #[test]
    fn matches_rnnoise_reference_output() {
        let input = to_f32(include_bytes!("../../tests/data/testing.raw"));
        let reference: Vec<i16> = include_bytes!("../../tests/data/reference_output.raw")
            .chunks_exact(2)
            .map(|x| i16::from_le_bytes([x[0], x[1]]))
            .collect();
        let mut state = DenoiseState::new();
        let mut out = Vec::new();
        let mut buf = [0.0f32; FRAME_SIZE];
        for (i, chunk) in input.chunks_exact(FRAME_SIZE).enumerate() {
            state.process_frame(&mut buf, chunk);
            if i > 0 {
                out.extend_from_slice(&buf);
            }
        }
        assert_eq!(out.len(), reference.len());
        let out: Vec<i16> = out.iter().map(|&x| x as i16).collect();
        let xx: f64 = out.iter().map(|&x| (x as f64).powi(2)).sum();
        let diff: f64 = reference
            .iter()
            .zip(&out)
            .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
            .sum();
        assert!(diff / xx < 1e-4, "relative error {}", diff / xx);
    }
}
