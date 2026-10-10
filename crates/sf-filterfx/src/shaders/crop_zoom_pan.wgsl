// Crop / zoom / pan — centered zoom about `center`.
//
//   ze = 1 + (zoom - 1) * u                                 (zoom default 1.5)
//   suv = center + (uv - center) / ze
//   out = sample(round(suv * (w, h)), clamped)
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1);
// at u = 0, ze = 1 and the frame is untouched.
//
// The rounding is round-half-away-from-zero on both sides (see
// chromatic_aberration.wgsl). Bit-identical to
// `sf_filterfx::cpu::crop_zoom_pan_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, pass_selector, 0
}

fn round_half_away(x: f32) -> f32 {
    return select(trunc(x + 0.5), trunc(x - 0.5), x < 0.0);
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let uv = vec2f((f32(id.x) + 0.5) / f32(dims.x), (f32(id.y) + 0.5) / f32(dims.y));
    let ze = 1.0 + (fx.u1.z - 1.0) * fx.u0.x;
    let suvx = fx.u1.x + (uv.x - fx.u1.x) / ze;
    let suvy = fx.u1.y + (uv.y - fx.u1.y) / ze;
    let ix = u32(clamp(round_half_away(suvx * f32(dims.x)), 0.0, f32(dims.x) - 1.0));
    let iy = u32(clamp(round_half_away(suvy * f32(dims.y)), 0.0, f32(dims.y) - 1.0));
    textureStore(dst, vec2u(id.x, id.y), textureLoad(src, vec2u(ix, iy), 0));
}
