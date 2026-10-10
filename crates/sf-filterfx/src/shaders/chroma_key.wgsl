// Chroma key with spill suppression (rgba16float in -> rgba16float out).
//
// Math contract (linear working space; key color = params.color):
//   d = sqrt((r-kr)^2 + (g-kg)^2 + (b-kb)^2), computed with an explicit
//       sqrt — never dot() (see the luma note in sf-scopes' zebra.wgsl:
//       backends may lower dot() with FMA contraction or a different
//       summation order, diverging from the CPU reference)
//   keyed = (1 - smoothstep(tolerance, tolerance + key_softness, d)) * u
//   mx = max(r, b)
//   g2 = g > mx ? mx + (g - mx) * (1 - spill * keyed) : g
//   out.rgb = mix(src.rgb, vec3f(r, g2, b), keyed); out.a = 1 - keyed
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
//
// The smoothstep contract is shared by both sides:
//   t = clamp((x-e0) / max(e1-e0, 1e-6), 0, 1); t*t*(3-2*t).
// The max() guard keeps key_softness = 0 degenerate-but-deterministic (a
// hard key edge) instead of 0/0 NaN; bit-identical to
// `sf_filterfx::cpu::chroma_key_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, 0, 0
}

fn smoothstep_g(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / max(e1 - e0, 1e-6), 0.0, 1.0);
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
    let dr = c.r - fx.u2.x;
    let dg = c.g - fx.u2.y;
    let db = c.b - fx.u2.z;
    let d = sqrt(dr * dr + dg * dg + db * db);
    let keyed = (1.0 - smoothstep_g(fx.u3.z, fx.u3.z + fx.u3.w, d)) * fx.u0.x;
    let mx = max(c.r, c.b);
    let g2 = select(c.g, mx + (c.g - mx) * (1.0 - fx.u2.w * keyed), c.g > mx);
    let rgb = mix(c.rgb, vec3f(c.r, g2, c.b), vec3f(keyed));
    textureStore(dst, vec2u(id.x, id.y), vec4f(rgb, 1.0 - keyed));
}
