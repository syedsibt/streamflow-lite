// Fused color grade pass (compute, 16x16 workgroups).
//
// This shader is the GPU twin of `sf_color::cpu::apply_grade_cpu`: every step
// mirrors the CPU reference one-to-one in f32. Any change to the chain must be
// made in both places.
//
// The `Grade` struct below must match `sf_color::GradeUniform` (160 bytes)
// field for field. Note `_pad1a/_pad1b/_pad1c`: three f32 pads (offsets
// 148/152/156) — a single vec3f pad would be misaligned.

struct Grade {
  exposure_ev: f32, transfer: f32, use_limited_range: f32, brightness: f32, // 0..12
  wb_gain: vec3f, _pad0: f32,                                              // 16..28
  lift: vec4f,                                                             // 32
  gain: vec4f,                                                             // 48
  gamma_w: vec4f,                                                          // 64
  offset: vec4f,                                                           // 80
  contrast: f32, saturation: f32, vibrance: f32, highlights: f32,           // 96..108
  shadows: f32, whites: f32, blacks: f32, basic_gamma: f32,                 // 112..124
  sharpness: f32, lut_intensity: f32, lut_flags: f32,                       // 128..136
  skin_protect: f32, curves_enabled: f32,                                   // 140..144
  _pad1a: f32, _pad1b: f32, _pad1c: f32,                                    // 148..156
};

@group(0) @binding(0) var<uniform> grade: Grade;
@group(0) @binding(1) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var dst_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(3) var curves_tex: texture_2d<f32>;
@group(0) @binding(4) var lut_tex: texture_3d<f32>;
@group(0) @binding(5) var lut_samp: sampler;

const LUM: vec3f = vec3f(0.2126, 0.7152, 0.0722);

// --- HSV helpers (exact branch structure of sf_color::cpu) ---
//
// NOTE: this is NOT standard colorimetric HSV — the red-max branch omits the
// usual /6, so e.g. yellow maps to hue 0 rather than 1/6. This is deliberate:
// the CPU reference implements the same branches, and CPU/GPU agreement is
// the contract. Do not "fix" one side independently.

fn rgb_to_hsv(c: vec3f) -> vec3f {
  let mx = max(c.r, max(c.g, c.b));
  let mn = min(c.r, min(c.g, c.b));
  let d = mx - mn;
  var h = 0.0;
  if (d > 1e-6) {
    if (mx == c.r) { h = fract((c.g - c.b) / d + 1.0); }
    else if (mx == c.g) { h = (c.b - c.r) / d + 1.0 / 3.0; }
    else { h = (c.r - c.g) / d + 2.0 / 3.0; }
  }
  let s = select(0.0, d / max(mx, 1e-6), mx > 1e-6);
  return vec3f(fract(h), s, mx);
}

fn hsv_to_rgb(hsv: vec3f) -> vec3f {
  let k = fract(hsv.r + vec3f(0.0, 2.0 / 3.0, 1.0 / 3.0)) * 6.0 - vec3f(3.0);
  return hsv.b * mix(vec3f(1.0), clamp(abs(k) - vec3f(1.0), vec3f(0.0), vec3f(1.0)), hsv.g);
}

// --- 3D LUT sampling ---
//
// Texel-center-correct trilinear: normalized coord c in 0..1 maps to
// texel position p = c*32 in a 33-wide lattice; sampling at (p+0.5)/33 puts
// the filter kernel exactly on the lattice points, which is identical to the
// CPU's `trilinear_sample` mapping x = c * (size - 1).

fn lut_sample(c: vec3f) -> vec3f {
  return textureSampleLevel(lut_tex, lut_samp, (c * 32.0 + 0.5) / 33.0, 0.0).rgb;
}

// --- Baked 1D curves ---
//
// The curves texture is 1024x2 rgba32float, texel (x, 0) = (R, G, B, master),
// texel (x, 1) = (luma, hue_vs_sat, 0, 0), texel i holds the curve value at
// x = i / 1023. Lookup uses textureLoad + manual linear interpolation (never
// filtered sampling) so texels cannot bleed into neighboring channels/rows.
// `chan` is a u32 because WGSL forbids dynamic vector indexing.

fn curve_sample(chan: u32, row: u32, x: f32) -> f32 {
  let fx = clamp(x, 0.0, 1.0) * 1023.0;
  let i0 = u32(floor(fx));
  let i1 = min(i0 + 1u, 1023u);
  let t = fract(fx);
  let a = textureLoad(curves_tex, vec2i(i32(i0), i32(row)), 0);
  let b = textureLoad(curves_tex, vec2i(i32(i1), i32(row)), 0);
  let m = mix(a, b, t);
  if (chan == 0u) { return m.r; }
  if (chan == 1u) { return m.g; }
  if (chan == 2u) { return m.b; }
  return m.a;
}

// Inverse sRGB OETF (sRGB EOTF), for the transfer == 1 path.
fn srgb_eotf(c: vec3f) -> vec3f {
  return select(c / 12.92, pow((c + 0.055) / 1.055, vec3f(2.4)), c > vec3f(0.04045));
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  let dims = textureDimensions(src_tex);
  if (gid.x >= dims.x || gid.y >= dims.y) { return; }

  let flags = u32(grade.lut_flags);
  let lut_enabled = (flags & 1u) != 0u;
  let lut_before = (flags & 2u) != 0u;

  // 1. Clamp + linearize (MVP: gamma 2.4; the transfer == 1 sRGB branch is
  //    implemented for completeness but the MVP always sends transfer = 0).
  var c = clamp(textureLoad(src_tex, gid.xy, 0).rgb, vec3f(0.0), vec3f(1.0));
  if (grade.transfer > 0.5) {
    c = srgb_eotf(c);
  } else {
    c = pow(c, vec3f(2.4));
  }

  // 2. LUT before the grade.
  if (lut_enabled && lut_before) {
    c = mix(c, lut_sample(clamp(c, vec3f(0.0), vec3f(1.0))), grade.lut_intensity);
  }

  // 3. Exposure.
  c *= exp2(grade.exposure_ev);

  // 4. White balance (gains precomputed on the CPU from temperature + tint).
  c *= grade.wb_gain;

  // 5. Lift / gamma / gain / offset wheels.
  c += grade.lift.rgb + grade.lift.a;
  c = pow(max(c, vec3f(0.0)), vec3f(1.0) / grade.gamma_w.rgb);
  c = pow(max(c, vec3f(0.0)), vec3f(1.0 / grade.gamma_w.a));
  c *= grade.gain.rgb * grade.gain.a;
  c += grade.offset.rgb + grade.offset.a;

  // 6. Tone controls; the luma windows are evaluated on post-wheels luma.
  let l = dot(c, LUM);
  let ck = 1.0 + grade.contrast;
  c = (c - vec3f(0.18)) * ck + vec3f(0.18);
  let badd = grade.brightness * 0.25;
  let hadd = grade.highlights * 0.5 * smoothstep(0.5, 1.0, l);
  let sadd = grade.shadows * 0.5 * (1.0 - smoothstep(0.0, 0.5, l));
  let wadd = grade.whites * 0.5 * smoothstep(0.55, 1.0, l);
  let kadd = grade.blacks * 0.5 * (1.0 - smoothstep(0.0, 0.45, l));
  c += badd + hadd + sadd + wadd + kadd;

  // 7. Basic gamma.
  c = pow(max(c, vec3f(0.0)), vec3f(grade.basic_gamma));

  // 8. Saturation around luma, with the skin-tone protection heuristic.
  let l2 = dot(c, LUM);
  var skin_m = 0.0;
  if (grade.skin_protect > 0.5) {
    skin_m = smoothstep(0.05, 0.25, c.r - c.g)
           * smoothstep(0.02, 0.12, c.g - c.b)
           * smoothstep(0.15, 0.45, l2);
  }
  var sat_eff = grade.saturation;
  if (grade.skin_protect > 0.5 && sat_eff > 1.0) {
    sat_eff = mix(sat_eff, 1.0 + (sat_eff - 1.0) * 0.4, skin_m);
  }
  c = mix(vec3f(l2), c, sat_eff);

  // 9. Vibrance: strength falls off as chroma rises; skin-limited when boosted.
  let l3 = dot(c, LUM);
  let chroma = max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b));
  var vscale = 1.0 + grade.vibrance * (1.0 - clamp(chroma, 0.0, 1.0));
  if (grade.skin_protect > 0.5 && grade.vibrance > 0.0) {
    vscale = mix(vscale, 1.0 + (vscale - 1.0) * 0.4, skin_m);
  }
  c = mix(vec3f(l3), c, vscale);

  // 10. Curves: per-channel, then master, then luma delta, then hue-vs-sat.
  if (grade.curves_enabled > 0.5) {
    let r = curve_sample(3u, 0u, curve_sample(0u, 0u, c.r));
    let g = curve_sample(3u, 0u, curve_sample(1u, 0u, c.g));
    let b = curve_sample(3u, 0u, curve_sample(2u, 0u, c.b));
    var cc = vec3f(r, g, b);
    let l4 = dot(cc, LUM);
    let dl = curve_sample(0u, 1u, l4) - l4;
    cc += vec3f(dl);
    let hsv = rgb_to_hsv(cc);
    let s2 = hsv.g * curve_sample(1u, 1u, hsv.r);
    c = hsv_to_rgb(vec3f(hsv.r, s2, hsv.b));
  }

  // 11. LUT after the grade.
  if (lut_enabled && !lut_before) {
    c = mix(c, lut_sample(clamp(c, vec3f(0.0), vec3f(1.0))), grade.lut_intensity);
  }

  // Output is linear; the encode pass handles output transfer + clamp.
  textureStore(dst_tex, gid.xy, vec4f(c, 1.0));
}
