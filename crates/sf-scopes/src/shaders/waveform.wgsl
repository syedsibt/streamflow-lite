// Waveform monitor: per-column (256) min/max of luma and RGB.
//
// One workgroup per column scans the 144 rows; the output buffer holds
// 8 f32 per column: [luma_min, luma_max, r_min, r_max, g_min, g_max,
// b_min, b_max]. Min/max reduction is order-independent, so the CPU
// reference in `sf_scopes::cpu::waveform` agrees bit-exactly.

// Rec.709 luma. NOTE: intentionally NOT dot(c, LUM) — backends may lower
// dot() with FMA contraction or a different summation order, which diverges
// from the CPU reference by 1 ulp and flips pixels sitting exactly on
// histogram bin boundaries. The explicit expansion keeps both sides on one
// op order (see sf_scopes::cpu::luma_of).
fn luma(c: vec3f) -> f32 {
    return c.r * 0.2126 + c.g * 0.7152 + c.b * 0.0722;
}

@group(0) @binding(0) var small: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(1)
fn main(@builtin(workgroup_id) wid: vec3<u32>) {
    let x = wid.x; // 0..255
    var lmin = 1.0;
    var lmax = 0.0;
    var rmin = 1.0;
    var rmax = 0.0;
    var gmin = 1.0;
    var gmax = 0.0;
    var bmin = 1.0;
    var bmax = 0.0;
    for (var y = 0u; y < 144u; y++) {
        let c = textureLoad(small, vec2u(x, y), 0).rgb;
        let l = luma(c);
        lmin = min(lmin, l);
        lmax = max(lmax, l);
        rmin = min(rmin, c.r);
        rmax = max(rmax, c.r);
        gmin = min(gmin, c.g);
        gmax = max(gmax, c.g);
        bmin = min(bmin, c.b);
        bmax = max(bmax, c.b);
    }
    let o = x * 8u;
    out[o + 0u] = lmin;
    out[o + 1u] = lmax;
    out[o + 2u] = rmin;
    out[o + 3u] = rmax;
    out[o + 4u] = gmin;
    out[o + 5u] = gmax;
    out[o + 6u] = bmin;
    out[o + 7u] = bmax;
}
