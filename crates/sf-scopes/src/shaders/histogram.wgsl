// Histogram: 256 bins each for R, G, B and Rec.709 luma.
//
// Bin layout in the storage buffer: [r0..r255, g0..g255, b0..b255,
// luma0..luma255]. Bin `i` counts values in `[i/256, (i+1)/256)`; 1.0 lands
// in bin 255. One atomic add per channel per pixel.

// Rec.709 luma. NOTE: intentionally NOT dot(c, LUM) — backends may lower
// dot() with FMA contraction or a different summation order, which diverges
// from the CPU reference by 1 ulp and flips pixels sitting exactly on
// histogram bin boundaries. The explicit expansion keeps both sides on one
// op order (see sf_scopes::cpu::luma_of).
fn luma(c: vec3f) -> f32 {
    return c.r * 0.2126 + c.g * 0.7152 + c.b * 0.0722;
}

@group(0) @binding(0) var small: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> bins: array<atomic<u32>, 1024>;

fn bin_of(v: f32) -> u32 {
    return u32(clamp(floor(v * 256.0), 0.0, 255.0));
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 144u) { return; }
    let c = textureLoad(small, vec2u(id.x, id.y), 0).rgb;
    let l = luma(c);
    atomicAdd(&bins[bin_of(c.r)], 1u);
    atomicAdd(&bins[256u + bin_of(c.g)], 1u);
    atomicAdd(&bins[512u + bin_of(c.b)], 1u);
    atomicAdd(&bins[768u + bin_of(l)], 1u);
}
