// False-color exposure map (256x144 RGB).
//
// Bands are stops from middle gray, `s = log2(max(L, 1e-4) / 0.18)`:
//   s >=  2.42 -> red     (clipped highlights)
//   s >=  1.42 -> yellow  (hot, near clipping)
//   s >=  0.50 -> green   (bright mids)
//   s >= -0.50 -> gray    (correct mid exposure)
//   s >= -1.50 -> cyan    (low mids)
//   else       -> blue    (shadows / crushed)
// Bit-identical to `sf_scopes::cpu::false_color_pixel` for pixels away from
// band boundaries (log2 may differ by 1 ulp between implementations).

// Rec.709 luma. NOTE: intentionally NOT dot(c, LUM) — backends may lower
// dot() with FMA contraction or a different summation order, which diverges
// from the CPU reference by 1 ulp and flips pixels sitting exactly on
// histogram bin boundaries. The explicit expansion keeps both sides on one
// op order (see sf_scopes::cpu::luma_of).
fn luma(c: vec3f) -> f32 {
    return c.r * 0.2126 + c.g * 0.7152 + c.b * 0.0722;
}

@group(0) @binding(0) var small: texture_2d<f32>;
@group(0) @binding(1) var out: texture_storage_2d<rgba8unorm, write>;

fn false_color(l: f32) -> vec3f {
    let s = log2(max(l, 1e-4) / 0.18);
    if (s >= 2.42) { return vec3f(1.0, 0.0, 0.0); }
    if (s >= 1.42) { return vec3f(1.0, 1.0, 0.0); }
    if (s >= 0.5) { return vec3f(0.0, 1.0, 0.0); }
    if (s >= -0.5) { return vec3f(0.5, 0.5, 0.5); }
    if (s >= -1.5) { return vec3f(0.0, 1.0, 1.0); }
    return vec3f(0.0, 0.0, 1.0);
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 144u) { return; }
    let c = textureLoad(small, vec2u(id.x, id.y), 0).rgb;
    let l = luma(c);
    textureStore(out, vec2u(id.x, id.y), vec4f(false_color(l), 1.0));
}
