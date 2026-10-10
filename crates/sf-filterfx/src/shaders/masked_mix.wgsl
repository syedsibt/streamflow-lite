// Masked mix — per-pixel blend of two sources through a weight mask.
//
//   m = mask.r * amount_u                        (amount_u = u0.x)
//   dst = mix(a, b, m)                           (all four channels)
//
// `a`, `b`, `mask` are rgba16float textures of the same size; the mask's
// `.r` channel carries the weight 0..1. Runs on the `masked_bgl` layout
// (a, b, mask, dst, uniform). Bit-identical to
// `sf_filterfx::cpu::masked_mix_cpu`.

struct Fx {
    u0: vec4f, // amount_u, 0, 0, 0
    u1: vec4f, // 0, 0, 0, 0
    u2: vec4f, // 0, 0, 0, 0
    u3: vec4f, // 0, 0, 0, 0
    u4: vec4f, // 0, 0, 0, 0
}

@group(0) @binding(0) var a_tex: texture_2d<f32>;
@group(0) @binding(1) var b_tex: texture_2d<f32>;
@group(0) @binding(2) var mask_tex: texture_2d<f32>;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(4) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(a_tex);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let xy = vec2u(id.x, id.y);
    let a = textureLoad(a_tex, xy, 0);
    let b = textureLoad(b_tex, xy, 0);
    let m = textureLoad(mask_tex, xy, 0).r * fx.u0.x;
    let t = 1.0 - m;
    textureStore(dst, xy, vec4f(a.rgb * t + b.rgb * m, a.a * t + b.a * m));
}
