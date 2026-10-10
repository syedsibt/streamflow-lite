// Pixelate / mosaic.
//
//   b = max(1, round(radius))                        (radius default 16 px)
//   cx = min((x / b) * b + b / 2, w - 1)             (integer division)
//   cy = min((y / b) * b + b / 2, h - 1)
//   out.rgb = mix(src[x, y].rgb, src[cx, cy].rgb, u); alpha = src alpha
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
//
// Integer-exact on both sides (no float math except the final mix), so the
// GPU-vs-CPU test asserts the tight 1e-3 bound. The radius rounding is
// round-half-away-from-zero, matching the CPU helper exactly.
// Bit-identical to `sf_filterfx::cpu::pixelate_cpu`.

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
    let c = textureLoad(src, vec2u(id.x, id.y), 0);
    let b = max(1u, u32(round_half_away(fx.u0.y)));
    let cx = min((id.x / b) * b + b / 2u, dims.x - 1u);
    let cy = min((id.y / b) * b + b / 2u, dims.y - 1u);
    let s = textureLoad(src, vec2u(cx, cy), 0);
    let rgb = mix(c.rgb, s.rgb, fx.u0.x);
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, c.a));
}
