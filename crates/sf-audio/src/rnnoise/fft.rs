//! Small dependency-free mixed-radix FFT, sized for RNNoise's 960-point
//! analysis window (960 = 2^6 · 3 · 5).
//!
//! Replaces the `easyfft`/`rustfft` dependency of upstream `nnnoiseless` so
//! `sf-audio` stays dependency-free. Unnormalized in both directions, like
//! `rustfft` — the callers apply RNNoise's own scaling.
//!
//! All buffers are allocated once in [`RealFft::new`]; `forward`/`inverse`
//! never allocate, so this is safe to run on the real-time audio path.

/// Minimal complex number (f32).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    #[inline]
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    #[inline]
    fn mul(self, o: Complex) -> Complex {
        Complex::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
    #[inline]
    fn add(self, o: Complex) -> Complex {
        Complex::new(self.re + o.re, self.im + o.im)
    }
    #[inline]
    fn conj(self) -> Complex {
        Complex::new(self.re, -self.im)
    }
}

/// Complex FFT plan for a fixed length whose prime factors are 2, 3, 5.
struct ComplexFft {
    n: usize,
    factors: Vec<usize>,
    /// `twiddles[k] = exp(-2πi·k/n)`, computed in f64 for accuracy.
    twiddles: Vec<Complex>,
    scratch: Vec<Complex>,
}

impl ComplexFft {
    fn new(n: usize) -> Self {
        let mut factors = Vec::new();
        let mut m = n;
        for p in [4usize, 2, 3, 5] {
            while m % p == 0 {
                factors.push(p);
                m /= p;
            }
        }
        assert!(m == 1, "FFT length {n} must factor into 2, 3, 5");
        let twiddles = (0..n)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
                Complex::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Self {
            n,
            factors,
            twiddles,
            scratch: vec![Complex::default(); n],
        }
    }

    /// In-place forward transform (sign −1, unnormalized).
    fn forward(&mut self, data: &mut [Complex]) {
        assert_eq!(data.len(), self.n);
        self.scratch.copy_from_slice(data);
        Self::rec(
            data,
            &self.scratch,
            1,
            &self.factors,
            &self.twiddles,
            self.n,
        );
    }

    /// Recursive decimation-in-time Cooley–Tukey with generic radix-p
    /// butterflies (p ≤ 5, so the inner DFT is tiny).
    fn rec(
        out: &mut [Complex],
        inp: &[Complex],
        stride: usize,
        factors: &[usize],
        tw: &[Complex],
        n_total: usize,
    ) {
        let n = out.len();
        if n == 1 {
            out[0] = inp[0];
            return;
        }
        let p = factors[0];
        let m = n / p;
        for q in 0..p {
            Self::rec(
                &mut out[q * m..(q + 1) * m],
                &inp[q * stride..],
                stride * p,
                &factors[1..],
                tw,
                n_total,
            );
        }
        let tw_step = n_total / n; // W_n^k = tw[k * tw_step]
        let p_step = n_total / p; // W_p^j = tw[j * p_step]
        let mut tmp = [Complex::default(); 5];
        for k in 0..m {
            for q in 0..p {
                let w = tw[(q * k * tw_step) % n_total];
                tmp[q] = out[q * m + k].mul(w);
            }
            for r in 0..p {
                let mut acc = tmp[0];
                for (q, t) in tmp.iter().enumerate().take(p).skip(1) {
                    acc = acc.add(t.mul(tw[(q * r * p_step) % n_total]));
                }
                out[k + r * m] = acc;
            }
        }
    }
}

/// Real-input FFT of fixed even length `n`, producing `n/2 + 1` bins
/// (DC at index 0, Nyquist at index `n/2`).
pub struct RealFft {
    n: usize,
    plan: ComplexFft,
    buf: Vec<Complex>,
}

impl RealFft {
    pub fn new(n: usize) -> Self {
        assert!(n % 2 == 0);
        Self {
            n,
            plan: ComplexFft::new(n),
            buf: vec![Complex::default(); n],
        }
    }

    /// Unnormalized forward transform: `input.len() == n`,
    /// `out.len() == n/2 + 1`.
    pub fn forward(&mut self, input: &[f32], out: &mut [Complex]) {
        debug_assert_eq!(input.len(), self.n);
        debug_assert_eq!(out.len(), self.n / 2 + 1);
        for (b, &x) in self.buf.iter_mut().zip(input) {
            *b = Complex::new(x, 0.0);
        }
        self.plan.forward(&mut self.buf);
        out.copy_from_slice(&self.buf[..self.n / 2 + 1]);
    }

    /// Unnormalized inverse transform of a Hermitian half-spectrum
    /// (`spec.len() == n/2 + 1`) into `out.len() == n` real samples. The
    /// imaginary parts of the DC and Nyquist bins are ignored, matching
    /// `realfft`'s complex-to-real semantics.
    pub fn inverse(&mut self, spec: &[Complex], out: &mut [f32]) {
        let n = self.n;
        let half = n / 2;
        // Inverse via the conjugation identity: ifft(X) = conj(fft(conj(X))).
        self.buf[0] = Complex::new(spec[0].re, 0.0);
        self.buf[half] = Complex::new(spec[half].re, 0.0);
        for k in 1..half {
            self.buf[k] = spec[k].conj();
            self.buf[n - k] = spec[k]; // conj(conj(X_k)) for the mirror bin
        }
        self.plan.forward(&mut self.buf);
        for (o, b) in out.iter_mut().zip(&self.buf) {
            *o = b.re; // conj() leaves the real part unchanged
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_dft(x: &[f32]) -> Vec<(f64, f64)> {
        let n = x.len();
        (0..=n / 2)
            .map(|k| {
                let (mut re, mut im) = (0.0, 0.0);
                for (t, &v) in x.iter().enumerate() {
                    let a = -2.0 * std::f64::consts::PI * (k * t) as f64 / n as f64;
                    re += v as f64 * a.cos();
                    im += v as f64 * a.sin();
                }
                (re, im)
            })
            .collect()
    }

    #[test]
    fn matches_naive_dft_960() {
        let n = 960;
        let x: Vec<f32> = (0..n)
            .map(|i| ((i * 7919) % 1000) as f32 / 500.0 - 1.0)
            .collect();
        let mut fft = RealFft::new(n);
        let mut out = vec![Complex::default(); n / 2 + 1];
        fft.forward(&x, &mut out);
        let reference = naive_dft(&x);
        for (k, (o, r)) in out.iter().zip(&reference).enumerate() {
            assert!(
                (o.re as f64 - r.0).abs() < 1e-2 && (o.im as f64 - r.1).abs() < 1e-2,
                "bin {k}: got {:?}, want {:?}",
                o,
                r
            );
        }
    }

    #[test]
    fn roundtrip_is_n_times_identity() {
        let n = 960;
        let x: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut fft = RealFft::new(n);
        let mut spec = vec![Complex::default(); n / 2 + 1];
        let mut back = vec![0.0f32; n];
        fft.forward(&x, &mut spec);
        fft.inverse(&spec, &mut back);
        for (a, b) in x.iter().zip(&back) {
            assert!((a * n as f32 - b).abs() < 1e-2, "{a} vs {b}");
        }
    }
}
