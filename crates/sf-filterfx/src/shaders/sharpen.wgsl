// Sharpen — unsharp mask ported from `sf-render`'s detail.wgsl (chain step 11).
//
//   blur = 0.25 * (4 von-Neumann neighbors, edges clamped to the frame edge)
//   out.rgb = c.rgb + (c.rgb - blur) * u * 1.5; alpha = src alpha
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
//
// Unlike detail.wgsl (which forces alpha to 1.0), this stack filter preserves
// the source alpha. Bit-identical to `sf_filterfx::cpu::sharpen_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, pass_selector, 0
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let x0 = select(id.x, id.x - 1u, id.x > 0u);
    let x1 = select(id.x, id.x + 1u, id.x + 1u < dims.x);
    let y0 = select(id.y, id.y - 1u, id.y > 0u);
    let y1 = select(id.y, id.y + 1u, id.y + 1u < dims.y);
    let c = textureLoad(src, vec2u(id.x, id.y), 0);
    let blur = 0.25 * (
          textureLoad(src, vec2u(x0, id.y), 0).rgb
        + textureLoad(src, vec2u(x1, id.y), 0).rgb
        + textureLoad(src, vec2u(id.x, y0), 0).rgb
        + textureLoad(src, vec2u(id.x, y1), 0).rgb);
    let rgb = c.rgb + (c.rgb - blur) * fx.u0.x * 1.5;
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, c.a));
}
