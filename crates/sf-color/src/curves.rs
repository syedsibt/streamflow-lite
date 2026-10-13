//! Spline curves: the Curves tab (RGB master, R, G, B, luma, hue-vs-sat).
//!
//! Each [`Curve`] holds up to 16 control points and is evaluated with
//! **monotone cubic (Fritsch–Carlson) interpolation**, so a monotone set of
//! points can never produce overshoot or ringing in the baked LUT.
//!
//! [`CurveSet::bake`] resamples every curve to 1024 texels, producing the
//! 1024x4 1D LUT texture the GPU grade pass samples:
//! row 0 = (R, G, B, master), row 1 = (luma, hue_vs_sat, 0, 0).

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Maximum control points per curve (matches the Curves tab spec).
pub const MAX_CURVE_POINTS: usize = 16;
/// Baked 1D LUT resolution per curve channel.
pub const BAKED_CURVE_SIZE: usize = 1024;

fn default_curve_points() -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [1.0, 1.0]]
}

/// One 1D spline curve: `[x, y]` control points with `x`, `y` in 0..1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Curve {
    /// Control points; kept sorted by `x` with unique `x` values.
    #[serde(default = "default_curve_points")]
    pub points: Vec<[f32; 2]>,
}

impl Default for Curve {
    fn default() -> Self {
        Self {
            points: default_curve_points(),
        }
    }
}

/// Error from [`Curve::set_points`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CurveError {
    /// More than [`MAX_CURVE_POINTS`] points were supplied.
    #[error("too many curve points (max {max})")]
    TooManyPoints { max: usize },
    /// No points were supplied.
    #[error("curve needs at least one point")]
    EmptyPoints,
}

impl Curve {
    /// Replace the control points.
    ///
    /// Points are clamped to [0, 1], sorted by `x`, and duplicate `x` values
    /// are collapsed keeping the **last** occurrence.
    pub fn set_points(&mut self, pts: Vec<[f32; 2]>) -> Result<(), CurveError> {
        if pts.is_empty() {
            return Err(CurveError::EmptyPoints);
        }
        if pts.len() > MAX_CURVE_POINTS {
            return Err(CurveError::TooManyPoints {
                max: MAX_CURVE_POINTS,
            });
        }
        let mut pts: Vec<[f32; 2]> = pts
            .into_iter()
            .map(|[x, y]| [x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)])
            .collect();
        // Stable sort: equal-x points keep their original order, so the
        // dedupe below keeps the last one.
        pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(Ordering::Equal));
        let mut dedup: Vec<[f32; 2]> = Vec::with_capacity(pts.len());
        for p in pts {
            match dedup.last_mut() {
                Some(last) if last[0] == p[0] => *last = p,
                _ => dedup.push(p),
            }
        }
        self.points = dedup;
        Ok(())
    }

    /// True for the neutral diagonal `[(0,0), (1,1)]`.
    pub fn is_identity(&self) -> bool {
        self.points == default_curve_points()
    }

    /// The curve blended toward the identity diagonal by `t` (0 = identity,
    /// 1 = unchanged) — used for look/preset intensity.
    pub fn scaled(&self, t: f32) -> Curve {
        let t = t.clamp(0.0, 1.0);
        if t >= 1.0 {
            return self.clone();
        }
        let mut pts: Vec<[f32; 2]> = self
            .points
            .iter()
            .map(|&[x, y]| [x, (x + (y - x) * t).clamp(0.0, 1.0)])
            .collect();
        if t <= 0.0 {
            pts = default_curve_points();
        }
        Curve { points: pts }
    }
}

/// A curve with precomputed Fritsch–Carlson tangents, ready for evaluation.
struct SolvedCurve {
    xs: Vec<f32>,
    ys: Vec<f32>,
    /// Tangent (dy/dx) at each knot.
    ms: Vec<f32>,
}

impl SolvedCurve {
    fn new(curve: &Curve) -> Self {
        let n = curve.points.len();
        let xs: Vec<f32> = curve.points.iter().map(|p| p[0]).collect();
        let ys: Vec<f32> = curve.points.iter().map(|p| p[1]).collect();
        let mut ms = vec![0.0f32; n];
        if n >= 2 {
            // Secant slopes between consecutive knots.
            let mut d = vec![0.0f32; n - 1];
            for i in 0..n - 1 {
                d[i] = (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i]);
            }
            // Endpoints take the adjacent secant slope (Fritsch–Carlson).
            ms[0] = d[0];
            ms[n - 1] = d[n - 2];
            // Interior knots: zero the tangent where the secants change sign
            // (local extremum) or flatten; otherwise use the harmonic-mean
            // tangent 3*d0*d1/(d0+d1), which is proven to keep the Hermite
            // interpolant monotone on every segment.
            for k in 1..n - 1 {
                if d[k - 1] == 0.0 || d[k] == 0.0 || d[k - 1].signum() != d[k].signum() {
                    ms[k] = 0.0;
                } else {
                    ms[k] = 3.0 * d[k - 1] * d[k] / (d[k - 1] + d[k]);
                }
            }
        }
        Self { xs, ys, ms }
    }

    fn eval(&self, x: f32) -> f32 {
        let n = self.xs.len();
        if n == 0 {
            return x.clamp(0.0, 1.0);
        }
        if n == 1 {
            return self.ys[0];
        }
        if x <= self.xs[0] {
            return self.ys[0];
        }
        if x >= self.xs[n - 1] {
            return self.ys[n - 1];
        }
        // Segment containing x (knots are strictly increasing after dedupe).
        let seg = self.xs.partition_point(|&px| px <= x);
        let i = (seg - 1).min(n - 2);
        let (x0, x1) = (self.xs[i], self.xs[i + 1]);
        let (y0, y1) = (self.ys[i], self.ys[i + 1]);
        let h = x1 - x0;
        let t = (x - x0) / h;
        let t2 = t * t;
        let t3 = t2 * t;
        // Cubic Hermite basis.
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * y0 + h10 * h * self.ms[i] + h01 * y1 + h11 * h * self.ms[i + 1]
    }
}

/// All six curves of the Curves tab.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CurveSet {
    #[serde(default)]
    pub master: Curve,
    #[serde(default)]
    pub r: Curve,
    #[serde(default)]
    pub g: Curve,
    #[serde(default)]
    pub b: Curve,
    #[serde(default)]
    pub luma: Curve,
    #[serde(default)]
    pub hue_vs_sat: Curve,
}

impl CurveSet {
    /// True when every curve is the neutral diagonal.
    /// Every curve blended toward identity by `t` (see [`Curve::scaled`]).
    pub fn scaled(&self, t: f32) -> CurveSet {
        CurveSet {
            master: self.master.scaled(t),
            r: self.r.scaled(t),
            g: self.g.scaled(t),
            b: self.b.scaled(t),
            luma: self.luma.scaled(t),
            hue_vs_sat: self.hue_vs_sat.scaled(t),
        }
    }

    pub fn is_identity(&self) -> bool {
        self.master.is_identity()
            && self.r.is_identity()
            && self.g.is_identity()
            && self.b.is_identity()
            && self.luma.is_identity()
            && self.hue_vs_sat.is_identity()
    }

    /// Bake every curve to 1024 samples at `x = i / 1023`.
    ///
    /// Layout matches the GPU 1D LUT texture: row 0 = (R, G, B, master),
    /// row 1 = (luma, hue_vs_sat, 0, 0).
    pub fn bake(&self) -> BakedCurves {
        let mut data = vec![0.0f32; 2 * 4 * BAKED_CURVE_SIZE];
        let row0 = [&self.r, &self.g, &self.b, &self.master];
        let row1 = [&self.luma, &self.hue_vs_sat];
        for (ch, curve) in row0.iter().enumerate() {
            let solved = SolvedCurve::new(curve);
            let base = ch * BAKED_CURVE_SIZE;
            for i in 0..BAKED_CURVE_SIZE {
                data[base + i] = solved.eval(i as f32 / (BAKED_CURVE_SIZE as f32 - 1.0));
            }
        }
        for (ch, curve) in row1.iter().enumerate() {
            let solved = SolvedCurve::new(curve);
            let base = (4 + ch) * BAKED_CURVE_SIZE;
            for i in 0..BAKED_CURVE_SIZE {
                let x = i as f32 / (BAKED_CURVE_SIZE as f32 - 1.0);
                let y = solved.eval(x);
                data[base + i] = if ch == 1 {
                    // Hue-vs-sat is a *multiplier*, edited in the same
                    // diagonal editor as the other curves: the deviation
                    // from the diagonal sets it (diagonal = 1.0 = no change,
                    // +0.5 above = 2x, -0.5 below = 0x). Baking the raw y
                    // value used to multiply saturation by the hue itself
                    // whenever any other curve was edited.
                    (1.0 + 2.0 * (y - x)).max(0.0)
                } else {
                    y
                };
            }
        }
        BakedCurves { data }
    }
}

/// Baked 1D LUT: `data.len() == 1024 * 2 * 4`.
///
/// Row 0 holds `(R, G, B, master)`, row 1 holds `(luma, hue_vs_sat, 0, 0)`;
/// each channel is 1024 samples at `x = i / 1023`.
#[derive(Clone, Debug)]
pub struct BakedCurves {
    pub data: Vec<f32>,
}

impl BakedCurves {
    /// Linearly interpolate between baked texels.
    ///
    /// `row` is 0 or 1, `channel` is 0..4, `x` is clamped to 0..1.
    pub fn sample(&self, row: usize, channel: usize, x: f32) -> f32 {
        debug_assert!(row < 2, "curve row out of range");
        debug_assert!(channel < 4, "curve channel out of range");
        let pos = x.clamp(0.0, 1.0) * (BAKED_CURVE_SIZE as f32 - 1.0);
        let i0 = (pos.floor() as usize).min(BAKED_CURVE_SIZE - 2);
        let f = pos - i0 as f32;
        let base = (row * 4 + channel) * BAKED_CURVE_SIZE;
        let a = self.data[base + i0];
        let b = self.data[base + i0 + 1];
        a + (b - a) * f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_bake_is_linear() {
        let cs = CurveSet::default();
        assert!(cs.is_identity());
        let baked = cs.bake();
        assert_eq!(baked.data.len(), 1024 * 2 * 4);
        for i in 0..1024 {
            let x = i as f32 / 1023.0;
            assert!((baked.sample(0, 0, x) - x).abs() < 1e-6, "R at {x}");
            assert!((baked.sample(0, 3, x) - x).abs() < 1e-6, "master at {x}");
            assert!((baked.sample(1, 0, x) - x).abs() < 1e-6, "luma at {x}");
            // Hue-vs-sat bakes a saturation *multiplier*: identity = 1.0.
            assert!(
                (baked.sample(1, 1, x) - 1.0).abs() < 1e-5,
                "hue_vs_sat at {x}"
            );
        }
    }

    #[test]
    fn s_curve_bake_is_monotone_and_hits_midpoint() {
        let mut cs = CurveSet::default();
        cs.master
            .set_points(vec![[0.0, 0.0], [0.5, 0.75], [1.0, 1.0]])
            .unwrap();
        assert!(!cs.is_identity());
        let baked = cs.bake();
        let mut prev = baked.sample(0, 3, 0.0);
        for i in 1..1024 {
            let v = baked.sample(0, 3, i as f32 / 1023.0);
            assert!(v >= prev, "not non-decreasing at texel {i}: {prev} -> {v}");
            prev = v;
        }
        let mid = baked.sample(0, 3, 512.0 / 1023.0);
        assert!((mid - 0.75).abs() < 0.01, "midpoint: {mid}");
        assert!((baked.sample(0, 3, 0.0) - 0.0).abs() < 1e-6);
        assert!((baked.sample(0, 3, 1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn set_points_validates() {
        let mut c = Curve::default();
        let pts: Vec<[f32; 2]> = (0..17)
            .map(|i| [i as f32 / 16.0, i as f32 / 16.0])
            .collect();
        assert!(matches!(
            c.set_points(pts),
            Err(CurveError::TooManyPoints { max: 16 })
        ));
        assert!(matches!(c.set_points(vec![]), Err(CurveError::EmptyPoints)));
        // 16 points is fine.
        let ok: Vec<[f32; 2]> = (0..16)
            .map(|i| [i as f32 / 15.0, i as f32 / 15.0])
            .collect();
        assert!(c.set_points(ok).is_ok());
    }

    #[test]
    fn set_points_sorts_dedups_clamps() {
        let mut c = Curve::default();
        c.set_points(vec![[0.8, 0.2], [0.2, 1.5], [0.2, 0.3], [-0.1, 0.4]])
            .unwrap();
        // clamped to [0,1], sorted by x, duplicate x=0.2 keeps the last point.
        assert_eq!(c.points, vec![[0.0, 0.4], [0.2, 0.3], [0.8, 0.2]]);
    }

    #[test]
    fn single_point_is_constant() {
        let mut c = Curve::default();
        c.set_points(vec![[0.3, 0.7]]).unwrap();
        let solved = SolvedCurve::new(&c);
        assert!((solved.eval(0.0) - 0.7).abs() < 1e-6);
        assert!((solved.eval(0.9) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn two_points_are_linear() {
        let mut c = Curve::default();
        c.set_points(vec![[0.0, 0.2], [1.0, 0.8]]).unwrap();
        let solved = SolvedCurve::new(&c);
        assert!((solved.eval(0.5) - 0.5).abs() < 1e-6);
        assert!((solved.eval(0.25) - 0.35).abs() < 1e-6);
    }
}
