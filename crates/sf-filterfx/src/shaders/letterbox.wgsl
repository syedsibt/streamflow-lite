// Letterbox bars (rgba16float in -> rgba16float out).
//
//   fa = w / h (frame aspect)
//   if fa < aspect:
//       bar = (1 - fa / aspect) / 2          (bar height in uv units)
//       if uv.y < bar || uv.y > 1 - bar:
//           out.rgb = mix(src.rgb, vec3(0.0), u)
//   else: out = src
// alpha is unchanged everywhere (the bars only darken rgb).
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1);
// at u = 1 the bars are pure black, at u = 0 the frame is untouched.
// Bit-identical to `sf_filterfx::cpu::letterbox_cpu`.

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
    let fa = f32(dims.x) / f32(dims.y);
    var rgb = c.rgb;
    if (fa < fx.u1.w) {
        let bar = (1.0 - fa / fx.u1.w) * 0.5;
        let uvy = (f32(id.y) + 0.5) / f32(dims.y);
        if (uvy < bar || uvy > 1.0 - bar) {
            rgb = mix(c.rgb, vec3f(0.0), fx.u0.x);
        }
    }
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, c.a));
}
