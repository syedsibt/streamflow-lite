// Box downscale of the graded RGBA8 frame to 256x144.
//
// Each output pixel averages 4 taps at the 1/4 and 3/4 points of its source
// rect (floored to texel centers, clamped) — bit-identical to
// `sf_scopes::cpu::downscale_box`. `textureLoad` (not filtered sampling) is
// used so the result is exactly reproducible on the CPU.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 144u) { return; }
    let dims = textureDimensions(src);
    let sw = f32(dims.x);
    let sh = f32(dims.y);
    let fx0 = f32(id.x) * sw / 256.0;
    let fx1 = f32(id.x + 1u) * sw / 256.0;
    let fy0 = f32(id.y) * sh / 144.0;
    let fy1 = f32(id.y + 1u) * sh / 144.0;
    var acc = vec4f(0.0);
    for (var ay = 0u; ay < 2u; ay++) {
        let oy = select(0.25, 0.75, ay == 1u);
        for (var ax = 0u; ax < 2u; ax++) {
            let ox = select(0.25, 0.75, ax == 1u);
            let sx = fx0 + ox * (fx1 - fx0);
            let sy = fy0 + oy * (fy1 - fy0);
            let ix = u32(clamp(floor(sx), 0.0, sw - 1.0));
            let iy = u32(clamp(floor(sy), 0.0, sh - 1.0));
            acc += textureLoad(src, vec2u(ix, iy), 0);
        }
    }
    textureStore(dst, vec2u(id.x, id.y), acc * 0.25);
}
