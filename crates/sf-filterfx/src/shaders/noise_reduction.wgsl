// Temporal noise reduction (rgba16float in -> rgba16float out).
//
// Temporal bind-group layout: [src sampled, prev sampled, dst storage,
// uniform] — see `EffectPass::register` with `temporal = true`.
//
// Per-pixel frame blend:
//   out.rgb = mix(cur.rgb, prev.rgb, u); out.a = cur.a
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
// u = 1 replaces the frame with the previous frame; u = 0 is a bypass.
// Bit-identical to `sf_filterfx::cpu::noise_reduction_cpu`.
//
// Note: `apply_slot` requires `prev` for this kind (Some texture); the
// engine passes the current frame itself on the first frame, so prev == cur
// and the blend is a no-op there.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, 0, 0
}

@group(0) @binding(0) var cur: texture_2d<f32>;
@group(0) @binding(1) var prev: texture_2d<f32>;
@group(0) @binding(2) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(3) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(cur);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let c = textureLoad(cur, vec2u(id.x, id.y), 0);
    let p = textureLoad(prev, vec2u(id.x, id.y), 0);
    let rgb = mix(c.rgb, p.rgb, vec3f(fx.u0.x));
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, c.a));
}
