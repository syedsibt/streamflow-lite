// Chromatic aberration — radial RGB channel shift.
//
//   uv = ((x + 0.5) / w, (y + 0.5) / h)
//   dir = uv - center
//   off = u * radius * dir                            (radius default 8.0 px)
//   r = sample(round(x - off.x), round(y - off.y))     (nearest int, clamped)
//   g = center pixel
//   b = sample(round(x + off.x), round(y + off.y))
//   out = (r.r, g.g, b.b, a)
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
//
// The rounding is round-half-away-from-zero on both sides: WGSL's round() is
// ties-to-even and would disagree with the CPU on exact .5 positions.
// Out-of-range taps clamp to the frame edge.
// Bit-identical to `sf_filterfx::cpu::chromatic_aberration_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, pass_selector, 0
}

// Round-half-away-from-zero, matching the CPU helper exactly (WGSL round()
// is ties-to-even).
fn round_half_away(x: f32) -> f32 {
    return select(trunc(x + 0.5), trunc(x - 0.5), x < 0.0);
}

// Float sample position -> clamped integer texel coordinate.
fn clamp_coord(v: f32, hi: u32) -> u32 {
    return u32(clamp(round_half_away(v), 0.0, f32(hi) - 1.0));
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let c = textureLoad(src, vec2u(id.x, id.y), 0);
    let uv = vec2f((f32(id.x) + 0.5) / f32(dims.x), (f32(id.y) + 0.5) / f32(dims.y));
    let dir = uv - fx.u1.xy;
    let off = fx.u0.x * fx.u0.y * dir;
    let rxy = vec2u(
        clamp_coord(f32(id.x) - off.x, dims.x),
        clamp_coord(f32(id.y) - off.y, dims.y));
    let bxy = vec2u(
        clamp_coord(f32(id.x) + off.x, dims.x),
        clamp_coord(f32(id.y) + off.y, dims.y));
    let r = textureLoad(src, rxy, 0);
    let b = textureLoad(src, bxy, 0);
    textureStore(dst, vec2u(id.x, id.y), vec4f(r.r, c.g, b.b, c.a));
}
