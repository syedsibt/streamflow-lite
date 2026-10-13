//! CPU references for the group-A and group-C effect filters — the exact
//! math contract.
//!
//! `src` is a row-major `w*h*4` f32 linear frame; each function returns a new
//! `Vec<f32>` in the same layout. Alpha is preserved from the source pixel
//! for every filter except [`chroma_key_cpu`], which writes `1 - keyed`.
//!
//! `u` is the premultiplied amount = `params.amount * intensity`, clamped to
//! `0..1` (see [`crate::make_uniform`]). Feed the CPU references
//! [`crate::test_utils::quantize_f16`] input, matching the f16 values the
//! GPU actually reads.

use sf_filters::{EffectParams, FlipAxis};

/// The smoothstep contract both sides implement exactly:
/// `t = clamp((x-e0)/(e1-e0), 0, 1); t*t*(3-2*t)`.
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Guarded smoothstep for chroma key: identical to [`smoothstep`] but with
/// `max(e1 - e0, 1e-6)` in the denominator so `key_softness = 0` degrades to
/// a hard key edge instead of a 0/0 NaN. Both the WGSL shader and
/// [`chroma_key_cpu`] implement this exact guard.
fn smoothstep_guarded(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Vignette: darkens pixels away from `params.center`.
///
/// `uv = ((x+0.5)/w, (y+0.5)/h)`; `d = sqrt(dx*dx + dy*dy)` (explicit — see
/// the "never dot()" rule); `s = smoothstep(radius, radius+softness, d)`;
/// `out.rgb = src.rgb * (1 - u*s)`; alpha unchanged.
pub fn vignette_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let (wf, hf) = (w as f32, h as f32);
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let dx = (x as f32 + 0.5) / wf - p.center[0];
            let dy = (y as f32 + 0.5) / hf - p.center[1];
            let d = (dx * dx + dy * dy).sqrt();
            let s = smoothstep(p.radius, p.radius + p.softness, d);
            let m = 1.0 - u * s;
            out.push(src[i] * m);
            out.push(src[i + 1] * m);
            out.push(src[i + 2] * m);
            out.push(src[i + 3]);
        }
    }
    out
}

/// The integer grain hash, identical to the WGSL version (u32 wrapping
/// arithmetic; `f32(h)` conversion is IEEE round-to-nearest on both sides).
fn grain_hash(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = x
        .wrapping_mul(374761393)
        .wrapping_add(y.wrapping_mul(668265263))
        .wrapping_add(seed.wrapping_mul(2246822519));
    h = (h ^ (h >> 13)).wrapping_mul(1274126177);
    h ^= h >> 16;
    (h as f32) / 4294967295.0 - 0.5
}

/// Film grain: adds per-pixel hash noise scaled by `u`.
///
/// `seed` goes through the same u32→f32→u32 roundtrip the uniform applies
/// (exact for seeds < 2^24). `out.rgb = src.rgb + u*g`; alpha unchanged.
pub fn film_grain_cpu(
    w: u32,
    h: u32,
    src: &[f32],
    p: &EffectParams,
    u: f32,
    seed: u32,
) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let _ = p;
    let seed_u = (seed as f32) as u32;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let g = grain_hash(x, y, seed_u);
            let n = u * g;
            out.push(src[i] + n);
            out.push(src[i + 1] + n);
            out.push(src[i + 2] + n);
            out.push(src[i + 3]);
        }
    }
    out
}

/// Letterbox: darkens the top/bottom bars toward black when the frame is
/// narrower than `params.aspect`.
///
/// `fa = w/h`; when `fa < aspect`, `bar = (1 - fa/aspect)/2` and pixels with
/// `uv.y < bar || uv.y > 1-bar` get `out.rgb = mix(src.rgb, 0, u)` (i.e.
/// `src.rgb * (1-u)`, matching WGSL `mix`'s `a*(1-t) + b*t` with `b = 0`);
/// alpha unchanged.
pub fn letterbox_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let fa = w as f32 / h as f32;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let (mut r, mut g, mut b) = (src[i], src[i + 1], src[i + 2]);
            if fa < p.aspect {
                let bar = (1.0 - fa / p.aspect) * 0.5;
                let uvy = (y as f32 + 0.5) / h as f32;
                if uvy < bar || uvy > 1.0 - bar {
                    let m = 1.0 - u;
                    r *= m;
                    g *= m;
                    b *= m;
                }
            }
            out.extend_from_slice(&[r, g, b, src[i + 3]]);
        }
    }
    out
}

/// Round-half-away-from-zero, identical to the WGSL helper of the same name
/// in the chromatic-aberration / pixelate / lens-correction / crop-zoom-pan
/// shaders (WGSL's `round()` is ties-to-even and would disagree on exact .5
/// positions).
fn round_half_away(x: f32) -> f32 {
    if x < 0.0 {
        (x - 0.5).trunc()
    } else {
        (x + 0.5).trunc()
    }
}

/// Float sample position -> clamped integer texel coordinate, matching the
/// WGSL `clamp_coord` (clamp in f32, then convert).
fn clamp_coord(v: f32, hi: u32) -> u32 {
    round_half_away(v).clamp(0.0, hi as f32 - 1.0) as u32
}

/// Sharpen: unsharp mask ported from `sf-render`'s detail.wgsl.
///
/// `blur = 0.25 * (4 von-Neumann neighbors, edges clamped)`;
/// `out.rgb = c.rgb + (c.rgb - blur) * u * 1.5`; alpha = src alpha.
pub fn sharpen_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let _ = p;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let xm = x.saturating_sub(1);
            let xp = (x + 1).min(w - 1);
            let ym = y.saturating_sub(1);
            let yp = (y + 1).min(h - 1);
            let i = ((y * w + x) * 4) as usize;
            // Accumulate in the same tap order as the WGSL (x-, x+, y-, y+).
            let (mut br, mut bg, mut bb) = (0.0f32, 0.0f32, 0.0f32);
            for (nx, ny) in [(xm, y), (xp, y), (x, ym), (x, yp)] {
                let j = ((ny * w + nx) * 4) as usize;
                br += src[j];
                bg += src[j + 1];
                bb += src[j + 2];
            }
            let (br, bg, bb) = (0.25 * br, 0.25 * bg, 0.25 * bb);
            out.push(src[i] + (src[i] - br) * u * 1.5);
            out.push(src[i + 1] + (src[i + 1] - bg) * u * 1.5);
            out.push(src[i + 2] + (src[i + 2] - bb) * u * 1.5);
            out.push(src[i + 3]);
        }
    }
    out
}

/// Chromatic aberration: radial RGB channel shift.
///
/// `uv = ((x+0.5)/w, (y+0.5)/h)`; `dir = uv - center`;
/// `off = u * radius * dir`; r samples `(x - off.x, y - off.y)`, g the center
/// pixel, b samples `(x + off.x, y + off.y)`; float positions round to the
/// nearest integer (round-half-away) and clamp to the frame edge;
/// `out = (r.r, g.g, b.b, a)`.
pub fn chromatic_aberration_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let (wf, hf) = (w as f32, h as f32);
    let s = u * p.radius;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let uvx = (x as f32 + 0.5) / wf;
            let uvy = (y as f32 + 0.5) / hf;
            let ox = s * (uvx - p.center[0]);
            let oy = s * (uvy - p.center[1]);
            let rx = clamp_coord(x as f32 - ox, w);
            let ry = clamp_coord(y as f32 - oy, h);
            let bx = clamp_coord(x as f32 + ox, w);
            let by = clamp_coord(y as f32 + oy, h);
            let rj = ((ry * w + rx) * 4) as usize;
            let bj = ((by * w + bx) * 4) as usize;
            out.push(src[rj]);
            out.push(src[i + 1]);
            out.push(src[bj + 2]);
            out.push(src[i + 3]);
        }
    }
    out
}

/// Pixelate / mosaic.
///
/// `b = max(1, round(radius))`; `cx = min((x / b) * b + b / 2, w - 1)`
/// (integer division), same for y;
/// `out.rgb = src[x,y].rgb * (1-u) + src[cx,cy].rgb * u`; alpha = src alpha.
pub fn pixelate_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let b = 1u32.max(round_half_away(p.radius) as u32);
    let m = 1.0 - u;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let cx = ((x / b) * b + b / 2).min(w - 1);
            let cy = ((y / b) * b + b / 2).min(h - 1);
            let j = ((cy * w + cx) * 4) as usize;
            out.push(src[i] * m + src[j] * u);
            out.push(src[i + 1] * m + src[j + 1] * u);
            out.push(src[i + 2] * m + src[j + 2] * u);
            out.push(src[i + 3]);
        }
    }
    out
}

/// The separable 9-tap Gaussian weights both sides use:
/// `sigma = max(radius * 0.5, 0.8)`; `w_i = exp(-(i*i)/(2*sigma*sigma))`,
/// `i in -4..=4`, normalized to sum 1.
fn gaussian_weights(radius: f32) -> [f32; 9] {
    let sigma = (radius * 0.5).max(0.8);
    let mut weights = [0.0f32; 9];
    let mut wsum = 0.0f32;
    for (k, i) in (-4..=4).enumerate() {
        let fi = i as f32;
        let wgt = (-(fi * fi) / (2.0 * sigma * sigma)).exp();
        weights[k] = wgt;
        wsum += wgt;
    }
    for wgt in weights.iter_mut() {
        *wgt /= wsum;
    }
    weights
}

/// Gaussian blur: separable 9-tap.
///
/// Horizontal pass into a scratch frame, then the vertical pass mixes with
/// the source: `out = src * (1-u) + blurred * u` (all four channels).
/// Out-of-range taps clamp to the frame edge.
pub fn gaussian_blur_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let weights = gaussian_weights(p.radius);
    // Horizontal pass.
    let mut tmp = vec![0.0f32; src.len()];
    for y in 0..h {
        for x in 0..w {
            let (mut ar, mut ag, mut ab, mut aa) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for (k, i) in (-4..=4).enumerate() {
                let sx = ((x as i32) + i).clamp(0, w as i32 - 1) as u32;
                let j = ((y * w + sx) * 4) as usize;
                let wt = weights[k];
                ar += wt * src[j];
                ag += wt * src[j + 1];
                ab += wt * src[j + 2];
                aa += wt * src[j + 3];
            }
            let o = ((y * w + x) * 4) as usize;
            tmp[o] = ar;
            tmp[o + 1] = ag;
            tmp[o + 2] = ab;
            tmp[o + 3] = aa;
        }
    }
    // Vertical pass + mix with the source.
    let m = 1.0 - u;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let (mut ar, mut ag, mut ab, mut aa) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for (k, i) in (-4..=4).enumerate() {
                let sy = ((y as i32) + i).clamp(0, h as i32 - 1) as u32;
                let j = ((sy * w + x) * 4) as usize;
                let wt = weights[k];
                ar += wt * tmp[j];
                ag += wt * tmp[j + 1];
                ab += wt * tmp[j + 2];
                aa += wt * tmp[j + 3];
            }
            let i = ((y * w + x) * 4) as usize;
            out.push(src[i] * m + ar * u);
            out.push(src[i + 1] * m + ag * u);
            out.push(src[i + 2] * m + ab * u);
            out.push(src[i + 3] * m + aa * u);
        }
    }
    out
}

/// Bloom / glow: bright-pass + separable 9-tap blur, additive composite.
///
/// `bright = max(src - threshold, 0)` per channel; blur `bright` with the
/// same separable 9-tap as [`gaussian_blur_cpu`] (sigma from `radius`);
/// `out.rgb = src.rgb + u * softness * blurred_bright`; alpha unchanged.
pub fn bloom_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    // Twin of bloom.wgsl: soft-knee bright pass, 25 taps spread over the
    // radius (step = max(radius/12, 1)), sigma = 4 taps.
    assert_eq!(src.len(), (w * h * 4) as usize);
    let step = (p.radius / 12.0).max(1.0);
    let mut wt = [0.0f32; 25];
    let mut wsum = 0.0f32;
    for (k, slot) in wt.iter_mut().enumerate() {
        let fi = k as f32 - 12.0;
        *slot = (-(fi * fi) / 32.0).exp();
        wsum += *slot;
    }
    let offs: Vec<i32> = (0..25)
        .map(|k| ((k as f32 - 12.0) * step + 0.5).floor() as i32)
        .collect();
    let knee = (p.softness * 0.5).max(0.001);
    let t = p.threshold;
    let smooth = |e0: f32, e1: f32, x: f32| {
        let tt = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
        tt * tt * (3.0 - 2.0 * tt)
    };
    let bright = |r: f32, g: f32, b: f32| -> [f32; 3] {
        let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let f = smooth(t - knee, t + knee, l);
        [r * f, g * f, b * f]
    };
    // Horizontal pass over the bright-extracted taps.
    let mut tmp = vec![0.0f32; src.len()];
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 3];
            for k in 0..25 {
                let sx = ((x as i32) + offs[k]).clamp(0, w as i32 - 1) as u32;
                let j = ((y * w + sx) * 4) as usize;
                let bb = bright(src[j], src[j + 1], src[j + 2]);
                let ww = wt[k] / wsum;
                for c in 0..3 {
                    acc[c] += ww * bb[c];
                }
            }
            let o = ((y * w + x) * 4) as usize;
            tmp[o..o + 3].copy_from_slice(&acc);
        }
    }
    // Vertical pass + additive composite with the source.
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 3];
            for k in 0..25 {
                let sy = ((y as i32) + offs[k]).clamp(0, h as i32 - 1) as u32;
                let j = ((sy * w + x) * 4) as usize;
                let ww = wt[k] / wsum;
                for c in 0..3 {
                    acc[c] += ww * tmp[j + c];
                }
            }
            let i = ((y * w + x) * 4) as usize;
            out.push(src[i] + u * acc[0]);
            out.push(src[i + 1] + u * acc[1]);
            out.push(src[i + 2] + u * acc[2]);
            out.push(src[i + 3]);
        }
    }
    out
}

/// Lens correction: radial (de)distortion around `p.center`.
///
/// `fa = w / h`; `px = (uv.x - cx) * fa`, `py = uv.y - cy`;
/// `r2 = px*px + py*py` (explicit — never `dot()`);
/// `s = 1 + (k1*r2 + k2*r2*r2) * u`;
/// `out = sample(round((cx + px*s/fa, cy + py*s) * (w, h)), clamped)`.
pub fn lens_correction_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let (wf, hf) = (w as f32, h as f32);
    let fa = wf / hf;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let uvx = (x as f32 + 0.5) / wf;
            let uvy = (y as f32 + 0.5) / hf;
            let px = (uvx - p.center[0]) * fa;
            let py = uvy - p.center[1];
            let r2 = px * px + py * py;
            let s = 1.0 + (p.k1 * r2 + p.k2 * r2 * r2) * u;
            let suvx = p.center[0] + px * s / fa;
            let suvy = p.center[1] + py * s;
            let ix = clamp_coord(suvx * wf, w);
            let iy = clamp_coord(suvy * hf, h);
            let j = ((iy * w + ix) * 4) as usize;
            out.extend_from_slice(&src[j..j + 4]);
        }
    }
    out
}

/// Crop / zoom / pan: centered zoom about `p.center`.
///
/// `ze = 1 + (zoom - 1) * u`;
/// `out = sample(round((center + (uv - center) / ze) * (w, h)), clamped)`.
pub fn crop_zoom_pan_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let (wf, hf) = (w as f32, h as f32);
    let ze = 1.0 + (p.zoom - 1.0) * u;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let uvx = (x as f32 + 0.5) / wf;
            let uvy = (y as f32 + 0.5) / hf;
            let suvx = p.center[0] + (uvx - p.center[0]) / ze;
            let suvy = p.center[1] + (uvy - p.center[1]) / ze;
            let ix = clamp_coord(suvx * wf, w);
            let iy = clamp_coord(suvy * hf, h);
            let j = ((iy * w + ix) * 4) as usize;
            out.extend_from_slice(&src[j..j + 4]);
        }
    }
    out
}

/// Mirror flip: `out.rgb = mix(src[x,y].rgb, src[sx,sy].rgb, u)` where
/// `(sx, sy)` is the flipped coordinate for `params.axis`
/// (Horizontal: `w-1-x`; Vertical: `h-1-y`; Both: both). Alpha is preserved
/// from the destination pixel — the flip swaps color samples, not opacity.
pub fn mirror_flip_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let flip_x = matches!(p.axis, FlipAxis::Horizontal | FlipAxis::Both);
    let flip_y = matches!(p.axis, FlipAxis::Vertical | FlipAxis::Both);
    let m = 1.0 - u;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let sx = if flip_x { w - 1 - x } else { x };
            let sy = if flip_y { h - 1 - y } else { y };
            let j = ((sy * w + sx) * 4) as usize;
            out.push(src[i] * m + src[j] * u);
            out.push(src[i + 1] * m + src[j + 1] * u);
            out.push(src[i + 2] * m + src[j + 2] * u);
            out.push(src[i + 3]);
        }
    }
    out
}

/// Chroma key with spill suppression (group C): keys out pixels near the
/// linear-space key color `p.color`.
///
/// `d = sqrt((r-kr)^2 + (g-kg)^2 + (b-kb)^2)` (explicit — see the "never
/// dot()" rule); `keyed = (1 - smoothstep(tolerance, tolerance+key_softness,
/// d)) * u`; `mx = max(r, b)`; `g2 = g > mx ? mx + (g-mx)*(1-spill*keyed) :
/// g`; `out.rgb = mix(src.rgb, (r, g2, b), keyed)`; `out.a = 1 - keyed`.
/// Unlike the group-A filters this one changes alpha (keyed pixels become
/// transparent). Bit-identical to `shaders/chroma_key.wgsl`.
pub fn chroma_key_cpu(w: u32, h: u32, src: &[f32], p: &EffectParams, u: f32) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    let (kr, kg, kb) = (p.color[0], p.color[1], p.color[2]);
    let mut out = Vec::with_capacity(src.len());
    for i in (0..src.len()).step_by(4) {
        let (r, g, b) = (src[i], src[i + 1], src[i + 2]);
        let dr = r - kr;
        let dg = g - kg;
        let db = b - kb;
        let d = (dr * dr + dg * dg + db * db).sqrt();
        let keyed = (1.0 - smoothstep_guarded(p.tolerance, p.tolerance + p.key_softness, d)) * u;
        let mx = r.max(b);
        let g2 = if g > mx {
            mx + (g - mx) * (1.0 - p.spill * keyed)
        } else {
            g
        };
        let m = 1.0 - keyed;
        // WGSL mix(a, b, t) = a*(1-t) + b*t, written out per channel.
        out.push(r * m + r * keyed);
        out.push(g * m + g2 * keyed);
        out.push(b * m + b * keyed);
        out.push(1.0 - keyed);
    }
    out
}

/// Temporal noise reduction (group C): blends the current frame with the
/// previous one.
///
/// `out.rgb = mix(cur.rgb, prev.rgb, u)` (i.e. `cur + (prev - cur) * u`);
/// alpha is the current frame's alpha. `prev = None` (first frame) returns
/// the current frame unchanged — the engine instead passes the current
/// frame texture as `prev`, which is the same blend. Bit-identical to
/// `shaders/noise_reduction.wgsl`.
pub fn noise_reduction_cpu(
    w: u32,
    h: u32,
    cur: &[f32],
    prev: Option<&[f32]>,
    p: &EffectParams,
    u: f32,
) -> Vec<f32> {
    assert_eq!(cur.len(), (w * h * 4) as usize);
    let _ = p;
    let Some(prev) = prev else {
        return cur.to_vec();
    };
    assert_eq!(prev.len(), cur.len());
    let m = 1.0 - u;
    let mut out = Vec::with_capacity(cur.len());
    for i in (0..cur.len()).step_by(4) {
        out.push(cur[i] * m + prev[i] * u);
        out.push(cur[i + 1] * m + prev[i + 1] * u);
        out.push(cur[i + 2] * m + prev[i + 2] * u);
        out.push(cur[i + 3]);
    }
    out
}

/// Masked mix (C7): per-pixel blend of two frames through a weight mask.
///
/// `m = mask.r * u` (`u` clamped to 0..1);
/// `out = mix(a, b, m)` = `a*(1-m) + b*m` on all four channels.
/// Bit-identical to `shaders/masked_mix.wgsl`.
pub fn masked_mix_cpu(w: u32, h: u32, a: &[f32], b: &[f32], mask: &[f32], amount: f32) -> Vec<f32> {
    assert_eq!(a.len(), (w * h * 4) as usize);
    assert_eq!(b.len(), a.len());
    assert_eq!(mask.len(), a.len());
    let u = amount.clamp(0.0, 1.0);
    let mut out = Vec::with_capacity(a.len());
    for i in (0..a.len()).step_by(4) {
        let m = mask[i] * u;
        let t = 1.0 - m;
        out.push(a[i] * t + b[i] * m);
        out.push(a[i + 1] * t + b[i + 1] * m);
        out.push(a[i + 2] * t + b[i + 2] * m);
        out.push(a[i + 3] * t + b[i + 3] * m);
    }
    out
}

/// Background replacement with edge spill suppression (C7).
///
/// `plate` is a `pw*ph*4` f32 frame tiled over the output
/// (`plate[(x % pw, y % ph)]`); a 1x1 plate is a solid color.
/// `m = mask.r`; `edge = 1 - |2m - 1|` (0 at m=0/1, peaks at m=0.5);
/// `luma = 0.2126*r + 0.7152*g + 0.0722*b` (explicit — never `dot()`);
/// `ds = s + (luma - s) * (spill * u * edge)`;
/// `out = mix(plate, ds, m)` on all four channels (alpha mixes the plate
/// and source alphas). Bit-identical to `shaders/bg_replace.wgsl`.
#[allow(clippy::too_many_arguments)]
pub fn bg_replace_cpu(
    w: u32,
    h: u32,
    src: &[f32],
    plate: &[f32],
    pw: u32,
    ph: u32,
    mask: &[f32],
    p: &EffectParams,
    u: f32,
) -> Vec<f32> {
    assert_eq!(src.len(), (w * h * 4) as usize);
    assert_eq!(plate.len(), (pw * ph * 4) as usize);
    assert_eq!(mask.len(), src.len());
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let pj = (((y % ph) * pw + (x % pw)) * 4) as usize;
            let (r, g, b, sa) = (src[i], src[i + 1], src[i + 2], src[i + 3]);
            let m = mask[i];
            let edge = 1.0 - (2.0 * m - 1.0).abs();
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let k = p.spill * u * edge;
            let dsr = r + (luma - r) * k;
            let dsg = g + (luma - g) * k;
            let dsb = b + (luma - b) * k;
            let t = 1.0 - m;
            out.push(plate[pj] * t + dsr * m);
            out.push(plate[pj + 1] * t + dsg * m);
            out.push(plate[pj + 2] * t + dsb * m);
            out.push(plate[pj + 3] * t + sa * m);
        }
    }
    out
}

/// Analytic region mask (C7): writes the weight into `.r` (g/b = 0, a = 1).
///
/// `kind` 1 = Circle{x, y, radius} (`geo = [x, y, radius, _]`),
/// `kind` 2 = Rectangle{x, y, w, h} (`geo = [x, y, w, h]`); all normalized
/// `0..1`, origin top-left. `uv = ((x+0.5)/w, (y+0.5)/h)`.
/// Circle: `w = 1 - sstep_guarded(r - feather, r, d)` with `d` an explicit
/// sqrt. Rectangle: per axis `w = 1 - sstep_guarded(half - feather, half,
/// |uv - center|)`, weight = `min` of the two axes. Weight is 1 inside with
/// a smoothstep falloff over `feather` at the edge. Bit-identical to
/// `shaders/gen_region_mask.wgsl`.
pub fn gen_region_mask_cpu(w: u32, h: u32, kind: u32, geo: [f32; 4], feather: f32) -> Vec<f32> {
    let (wf, hf) = (w as f32, h as f32);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let uvx = (x as f32 + 0.5) / wf;
            let uvy = (y as f32 + 0.5) / hf;
            let wgt = if kind == 1 {
                let dx = uvx - geo[0];
                let dy = uvy - geo[1];
                let d = (dx * dx + dy * dy).sqrt();
                1.0 - smoothstep_guarded(geo[2] - feather, geo[2], d)
            } else {
                let cx = geo[0] + geo[2] * 0.5;
                let cy = geo[1] + geo[3] * 0.5;
                let ax = (uvx - cx).abs();
                let ay = (uvy - cy).abs();
                let wx = 1.0 - smoothstep_guarded(geo[2] * 0.5 - feather, geo[2] * 0.5, ax);
                let wy = 1.0 - smoothstep_guarded(geo[3] * 0.5 - feather, geo[3] * 0.5, ay);
                wx.min(wy)
            };
            out.extend_from_slice(&[wgt, 0.0, 0.0, 1.0]);
        }
    }
    out
}
