// GPU bilinear scaling: replaces CPU resize_rgba_nearest.
// Scales src texture to dst texture dimensions.
//
// Bindings:
//   @group(0) @binding(0): src texture (read-only, sampled)
//   @group(0) @binding(1): dst texture (write-only storage)
//   @group(0) @binding(2): sampler

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var dst_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var samp: sampler;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dst_size = textureDimensions(dst_tex);
    if (gid.x >= dst_size.x || gid.y >= dst_size.y) {
        return;
    }
    // Normalized UV at texel center for bilinear filtering.
    let uv = (vec2<f32>(gid.xy) + 0.5) / vec2<f32>(dst_size);
    let color = textureSampleLevel(src_tex, samp, uv, 0.0);
    textureStore(dst_tex, gid.xy, color);
}
