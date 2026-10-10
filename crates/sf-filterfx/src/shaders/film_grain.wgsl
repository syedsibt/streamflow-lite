// Animated film grain (rgba16float in -> rgba16float out).
//
// Integer hash per pixel and frame seed:
//   h = x*374761393 + y*668265263 + seed*2246822519  (u32 wrapping arithmetic)
//   h = (h ^ (h >> 13)) * 1274126177
//   h ^= h >> 16
//   g = f32(h) / 4294967295.0 - 0.5
//   out.rgb = src.rgb + u * g; alpha unchanged
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
//
// `seed` arrives in the uniform as f32 (u4.x) and is converted back with
// u32(...) — exact for seeds < 2^24. The CPU reference applies the same
// u32->f32->u32 roundtrip, so both sides hash the identical value.
// Bit-identical to `sf_filterfx::cpu::film_grain_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, 0, 0
}

fn grain_hash(x: u32, y: u32, seed: u32) -> f32 {
    var h: u32 = x * 374761393u + y * 668265263u + seed * 2246822519u;
    h = (h ^ (h >> 13u)) * 1274126177u;
    h = h ^ (h >> 16u);
    return f32(h) / 4294967295.0 - 0.5;
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let c = textureLoad(src, vec2u(id.x, id.y), 0);
    let seed = u32(fx.u4.x);
    let g = grain_hash(id.x, id.y, seed);
    let rgb = c.rgb + fx.u0.x * g;
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, c.a));
}
