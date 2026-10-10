// Analytic region mask generator: writes the weight into dst.r (g/b = 0,
// a = 1).
//
// Uniform (two vec4s):
//   p0 = [kind, x, y, radius_or_w], p1 = [h, feather, 0, 0]
//   kind 1 = Circle{x, y, radius}, kind 2 = Rectangle{x, y, w, h}
// All geometry normalized 0..1, origin top-left; feather in normalized units.
//
//   uv = ((x+0.5)/w, (y+0.5)/h)
//   circle:    d = sqrt(dx*dx + dy*dy) (explicit — never dot())
//              wgt = 1 - smoothstep(r - feather, r, d)
//   rectangle: per axis wx = 1 - smoothstep(half - feather, half, |uv - c|)
//              wgt = min(wx, wy)
//
// Weight is 1 inside, with a smoothstep falloff over `feather` at the edge.
// The smoothstep is guarded (denominator max(e1-e0, 1e-6)) so feather = 0
// degrades to a hard edge instead of a 0/0 NaN — the same guard as
// `smoothstep_guarded` in `sf_filterfx::cpu`. Bit-identical to
// `sf_filterfx::cpu::gen_region_mask_cpu`.

struct MaskU {
    p0: vec4f, // kind, x, y, radius_or_w
    p1: vec4f, // h, feather, 0, 0
}

@group(0) @binding(0) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(1) var<uniform> mu: MaskU;

fn sstep_guarded(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / max(e1 - e0, 1e-6), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) { return; }
    let uv = (vec2f(id.xy) + vec2f(0.5)) / vec2f(dims);
    let feather = mu.p1.y;
    var wgt = 0.0;
    if (mu.p0.x < 1.5) {
        // Circle: center (x, y), radius r.
        let dx = uv.x - mu.p0.y;
        let dy = uv.y - mu.p0.z;
        let d = sqrt(dx * dx + dy * dy);
        let r = mu.p0.w;
        wgt = 1.0 - sstep_guarded(r - feather, r, d);
    } else {
        // Rectangle: top-left (x, y), size (w, h).
        let cx = mu.p0.y + mu.p0.w * 0.5;
        let cy = mu.p0.z + mu.p1.x * 0.5;
        let ax = abs(uv.x - cx);
        let ay = abs(uv.y - cy);
        let wx = 1.0 - sstep_guarded(mu.p0.w * 0.5 - feather, mu.p0.w * 0.5, ax);
        let wy = 1.0 - sstep_guarded(mu.p1.x * 0.5 - feather, mu.p1.x * 0.5, ay);
        wgt = min(wx, wy);
    }
    textureStore(dst, vec2u(id.x, id.y), vec4f(wgt, 0.0, 0.0, 1.0));
}
