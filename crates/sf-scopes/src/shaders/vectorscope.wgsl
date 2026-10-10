// Vectorscope: 128x128 chroma bin grid over normalized (Cb, Cr).
//
// Rec.709: Cb = (B - Y) / (2(1 - Kb)), Cr = (R - Y) / (2(1 - Kr)), binned
// from the nominal [-0.5, 0.5] range and clamped to the grid edges. Each
// pixel does one atomic add; bin (bx, by) lives at `by * 128 + bx`.
// The skin-tone guide (~123°) is a UI overlay drawn from
// `sf_scopes::SKIN_TONE_ANGLE_DEG`, not part of this data.

// Rec.709 luma. NOTE: intentionally NOT dot(c, LUM) — backends may lower
// dot() with FMA contraction or a different summation order, which diverges
// from the CPU reference by 1 ulp and flips pixels sitting exactly on
// histogram bin boundaries. The explicit expansion keeps both sides on one
// op order (see sf_scopes::cpu::luma_of).
fn luma(c: vec3f) -> f32 {
    return c.r * 0.2126 + c.g * 0.7152 + c.b * 0.0722;
}
const N: u32 = 128u;

@group(0) @binding(0) var small: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> bins: array<atomic<u32>, 16384>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 144u) { return; }
    let c = textureLoad(small, vec2u(id.x, id.y), 0).rgb;
    let y = luma(c);
    let cb = (c.b - y) / (2.0 * (1.0 - 0.0722));
    let cr = (c.r - y) / (2.0 * (1.0 - 0.2126));
    let bx = u32(clamp(floor((cb + 0.5) * f32(N)), 0.0, f32(N) - 1.0));
    let by = u32(clamp(floor((cr + 0.5) * f32(N)), 0.0, f32(N) - 1.0));
    atomicAdd(&bins[by * N + bx], 1u);
}
