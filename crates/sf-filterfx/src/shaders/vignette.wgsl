// Vignette (rgba16float in -> rgba16float out).
//
// Per-pixel darkening around `center`:
//   uv = ((x + 0.5) / w, (y + 0.5) / h)
//   d = length(uv - center), computed with an explicit sqrt(dx*dx + dy*dy)
//       (never dot() — see the luma note in sf-scopes' zebra.wgsl: backends
//       may lower dot() with FMA contraction or a different summation order,
//       diverging from the CPU reference)
//   s = smoothstep(radius, radius + softness, d)
//   out.rgb = src.rgb * (1 - u * s); alpha unchanged
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
//
// The smoothstep contract is shared by both sides:
//   t = clamp((x - e0) / (e1 - e0), 0, 1); t*t*(3 - 2*t).
// Bit-identical to `sf_filterfx::cpu::vignette_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, 0, 0
}

fn smoothstep_e(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
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
    let dx = uv.x - fx.u1.x;
    let dy = uv.y - fx.u1.y;
    let d = sqrt(dx * dx + dy * dy);
    let s = smoothstep_e(fx.u0.y, fx.u0.y + fx.u0.z, d);
    let rgb = c.rgb * (1.0 - fx.u0.x * s);
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, c.a));
}
