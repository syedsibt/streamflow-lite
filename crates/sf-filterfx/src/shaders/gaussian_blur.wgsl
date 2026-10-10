// Gaussian blur — separable 9-tap, two passes over the temporal bind layout.
//
//   sigma = max(radius * 0.5, 0.8)                   (radius default 4.0 px)
//   w_i = exp(-(i*i) / (2*sigma*sigma)), i in -4..=4, normalized to sum 1
//   pass 0 (u4.z < 0.5): scratch = H blur of src     (tex0 = src)
//   pass 1 (u4.z >= 0.5): dst = mix(src, V blur of scratch, u)
//                        (tex0 = scratch, tex1 = src)
// u = amount_u from the uniform (params.amount * intensity, clamped 0..1).
//
// Out-of-range taps clamp to the frame edge. `apply_slot` runs pass 0 then
// pass 1 as two dispatches (bumping the pass counter twice); the u4.z
// selector is set per dispatch there — `make_uniform` leaves it 0.
// The exp() builtin may differ from the CPU libm by an ulp, hence the 3e-3
// test bound. Otherwise bit-identical to
// `sf_filterfx::cpu::gaussian_blur_cpu`.

struct Fx {
    u0: vec4f, // amount_u, radius, softness, threshold
    u1: vec4f, // center_x, center_y, zoom, aspect
    u2: vec4f, // key_r, key_g, key_b, spill
    u3: vec4f, // k1, k2, tolerance, key_softness
    u4: vec4f, // seed_as_f32, axis_code, pass_selector, 0
}

@group(0) @binding(0) var tex0: texture_2d<f32>;
@group(0) @binding(1) var tex1: texture_2d<f32>;
@group(0) @binding(2) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(3) var<uniform> fx: Fx;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(tex0);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let sigma = max(fx.u0.y * 0.5, 0.8);
    var wsum = 0.0;
    var wt = array<f32, 9>();
    for (var k = 0; k < 9; k++) {
        let fi = f32(k - 4);
        let wgt = exp(-(fi * fi) / (2.0 * sigma * sigma));
        wt[k] = wgt;
        wsum = wsum + wgt;
    }
    for (var k = 0; k < 9; k++) {
        wt[k] = wt[k] / wsum;
    }
    if (fx.u4.z < 0.5) {
        // Horizontal pass: blur tex0 (src) into dst (the scratch texture).
        var acc = vec4f(0.0);
        for (var k = 0; k < 9; k++) {
            let sx = u32(clamp(f32(i32(id.x) + k - 4), 0.0, f32(dims.x) - 1.0));
            acc = acc + wt[k] * textureLoad(tex0, vec2u(sx, id.y), 0);
        }
        textureStore(dst, vec2u(id.x, id.y), acc);
    } else {
        // Vertical pass + composite: blur tex0 (scratch), mix with tex1 (src).
        var acc = vec4f(0.0);
        for (var k = 0; k < 9; k++) {
            let sy = u32(clamp(f32(i32(id.y) + k - 4), 0.0, f32(dims.y) - 1.0));
            acc = acc + wt[k] * textureLoad(tex0, vec2u(id.x, sy), 0);
        }
        let c = textureLoad(tex1, vec2u(id.x, id.y), 0);
        let m = 1.0 - fx.u0.x;
        textureStore(dst, vec2u(id.x, id.y),
            vec4f(c.rgb * m + acc.rgb * fx.u0.x, c.a * m + acc.a * fx.u0.x));
    }
}
