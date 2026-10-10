// Zebra overlay (256x144 RGBA).
//
// Pixels with luma above `hi` or below `lo` get diagonal stripes:
// `(x + y) % 16 < 8` selects white stripe cells, the rest are black, both
// at 0.75 alpha. All other pixels are fully transparent. The UI composites
// this over the preview. Bit-identical to `sf_scopes::cpu::zebra`.

// Rec.709 luma. NOTE: intentionally NOT dot(c, LUM) — backends may lower
// dot() with FMA contraction or a different summation order, which diverges
// from the CPU reference by 1 ulp and flips pixels sitting exactly on
// histogram bin boundaries. The explicit expansion keeps both sides on one
// op order (see sf_scopes::cpu::luma_of).
fn luma(c: vec3f) -> f32 {
    return c.r * 0.2126 + c.g * 0.7152 + c.b * 0.0722;
}

struct Params {
    hi: f32,
    lo: f32,
    _pad0: f32,
    _pad1: f32,
}

@group(0) @binding(0) var small: texture_2d<f32>;
@group(0) @binding(1) var out: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> p: Params;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 144u) { return; }
    let c = textureLoad(small, vec2u(id.x, id.y), 0).rgb;
    let l = luma(c);
    var px = vec4f(0.0, 0.0, 0.0, 0.0);
    if (l > p.hi || l < p.lo) {
        let stripe = (id.x + id.y) % 16u < 8u;
        let v = select(0.0, 1.0, stripe);
        px = vec4f(v, v, v, 0.75);
    }
    textureStore(out, vec2u(id.x, id.y), px);
}
