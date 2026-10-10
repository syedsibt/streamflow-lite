// Background replacement with edge spill suppression.
//
//   m    = mask.r                                 (person weight 0..1)
//   edge = 1 - abs(2*m - 1)                       (0 at m=0/1, peaks at m=0.5)
//   luma = 0.2126*s.r + 0.7152*s.g + 0.0722*s.b  (explicit — never dot())
//   ds   = s + (luma - s) * (spill * amount_u * edge)
//   dst  = mix(plate, ds, m)                      (all four channels)
//
// The plate texture is sampled with wraparound: a 1x1 plate is a solid color
// (the engine binds params.color as the single texel), a larger plate tiles
// over the frame. `spill` = u2.w (params.spill), `amount_u` = u0.x. The
// amount only scales the spill term; the replace itself follows the raw mask
// (a zero amount bypasses the whole dispatch in `apply_background_replace`).
// Runs on the `masked_bgl` layout (plate, src, mask, dst, uniform).
// Bit-identical to `sf_filterfx::cpu::bg_replace_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, pass_selector, 0
}

@group(0) @binding(0) var plate_tex: texture_2d<f32>;
@group(0) @binding(1) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var mask_tex: texture_2d<f32>;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(4) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src_tex);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let xy = vec2u(id.x, id.y);
    let pdims = textureDimensions(plate_tex);
    let plate = textureLoad(plate_tex, vec2u(id.x % pdims.x, id.y % pdims.y), 0);
    let s = textureLoad(src_tex, xy, 0);
    let m = textureLoad(mask_tex, xy, 0).r;
    let edge = 1.0 - abs(2.0 * m - 1.0);
    let luma = 0.2126 * s.r + 0.7152 * s.g + 0.0722 * s.b;
    let k = fx.u2.w * fx.u0.x * edge;
    let ds = s.rgb + (vec3f(luma) - s.rgb) * k;
    let t = 1.0 - m;
    textureStore(dst, xy, vec4f(plate.rgb * t + ds * m, plate.a * t + s.a * m));
}
