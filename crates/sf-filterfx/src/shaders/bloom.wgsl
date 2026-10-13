// Glow / bloom — soft-knee bright-pass + separable 9-tap gaussian whose
// taps are spread over the requested radius, additive composite.
//
//   l      = luma(src)
//   bright = src.rgb * smoothstep(t - knee, t + knee, l)
//            t = threshold (0..1), knee = max(softness * 0.5, 0.001)
//            (hue-preserving; softness = how gradually glow starts)
//   taps   k = -12..12 at offset floor(k * step + 0.5) px, step = max(radius/12, 1)
//            weight exp(-k^2 / 32) (sigma = 4 taps), normalized
//            -> the glow really extends ~radius px (the old kernel stopped
//               at ±4 px whatever the radius, so the Radius slider did nothing
//               at broadcast resolutions); 25 taps keep large radii smooth
//               (no "ghost copies"). Cost: 25 taps x 2 passes.
//   pass 0 (u4.z < 0.5): scratch = H blur of bright  (tex0 = src)
//   pass 1 (u4.z >= 0.5): dst.rgb = src.rgb + amount_u * V-blurred bright
//                         dst.a = src.a             (tex0 = scratch, tex1 = src)
// amount_u = params.amount * slot intensity, clamped 0..1.
//
// Twin of `sf_filterfx::cpu::bloom_cpu` (same tap rounding: floor(x + 0.5)).

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

const LUM: vec3f = vec3f(0.2126, 0.7152, 0.0722);

fn bright(s: vec4f) -> vec3f {
    let knee = max(fx.u0.z * 0.5, 0.001);
    let t = fx.u0.w;
    let f = smoothstep(t - knee, t + knee, dot(s.rgb, LUM));
    return s.rgb * f;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(tex0);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let step = max(fx.u0.y / 12.0, 1.0);
    var wt = array<f32, 25>();
    var wsum = 0.0;
    for (var k = 0; k < 25; k++) {
        let fi = f32(k - 12);
        let wgt = exp(-(fi * fi) / 32.0);
        wt[k] = wgt;
        wsum = wsum + wgt;
    }
    if (fx.u4.z < 0.5) {
        // Horizontal pass: bright-extract each tap, blur into scratch.
        var acc = vec3f(0.0);
        for (var k = 0; k < 25; k++) {
            let off = i32(floor(f32(k - 12) * step + 0.5));
            let sx = u32(clamp(i32(id.x) + off, 0, i32(dims.x) - 1));
            acc = acc + (wt[k] / wsum) * bright(textureLoad(tex0, vec2u(sx, id.y), 0));
        }
        textureStore(dst, vec2u(id.x, id.y), vec4f(acc, 0.0));
    } else {
        // Vertical pass + composite: blur the bright scratch, add to src.
        var acc = vec3f(0.0);
        for (var k = 0; k < 25; k++) {
            let off = i32(floor(f32(k - 12) * step + 0.5));
            let sy = u32(clamp(i32(id.y) + off, 0, i32(dims.y) - 1));
            acc = acc + (wt[k] / wsum) * textureLoad(tex0, vec2u(id.x, sy), 0).rgb;
        }
        let c = textureLoad(tex1, vec2u(id.x, id.y), 0);
        textureStore(dst, vec2u(id.x, id.y), vec4f(c.rgb + fx.u0.x * acc, c.a));
    }
}
