// Mirror flip (rgba16float in -> rgba16float out).
//
//   axis_code (u4.y): 0 = Horizontal (sx = w-1-x), 1 = Vertical (sy = h-1-y),
//                     2 = Both (equivalent to a 180-degree rotation)
//   out.rgb = mix(src[x, y].rgb, src[sx, sy].rgb, u)
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1);
// at u = 1 the frame is fully mirrored, at u = 0 it is untouched.
//
// Alpha is preserved from the destination pixel (documented choice: the flip
// swaps color samples, not opacity; at u = 1 a mirrored opaque frame stays
// exactly opaque). Bit-identical to `sf_filterfx::cpu::mirror_flip_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, 0, 0
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let c = textureLoad(src, vec2u(id.x, id.y), 0);
    let axis = u32(fx.u4.y);
    let sx = select(id.x, dims.x - 1u - id.x, axis == 0u || axis == 2u);
    let sy = select(id.y, dims.y - 1u - id.y, axis == 1u || axis == 2u);
    let m = textureLoad(src, vec2u(sx, sy), 0);
    let rgb = mix(c.rgb, m.rgb, fx.u0.x);
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, c.a));
}
