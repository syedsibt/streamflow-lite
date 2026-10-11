// GPU alpha compositing: blends source over destination.
// Replaces the CPU `composite_over` for the hybrid pipeline.
//
// This version uses separate src/dst/output textures (no read-write
// storage) for maximum GPU compatibility.
//
// Bindings:
//   @group(0) @binding(0): dst texture (read-only, sampled)
//   @group(0) @binding(1): src texture (read-only, sampled)
//   @group(0) @binding(2): out texture (write-only storage)
//   @group(0) @binding(3): params (src rect, dst offset, opacity)

struct Params {
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    dst_x: u32,
    dst_y: u32,
    opacity: f32,
    _pad: f32,
};

@group(0) @binding(0) var dst_tex: texture_2d<f32>;
@group(0) @binding(1) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(3) var<uniform> params: Params;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let out_size = textureDimensions(out_tex);
    if (gid.x >= out_size.x || gid.y >= out_size.y) {
        return;
    }

    // Start with destination pixel.
    var result = textureLoad(dst_tex, gid.xy, 0);

    // Check if this pixel is within the source rect.
    let sx = i32(gid.x) - i32(params.dst_x);
    let sy = i32(gid.y) - i32(params.dst_y);
    if (sx >= 0 && sy >= 0 && u32(sx) < params.src_w && u32(sy) < params.src_h) {
        let src_uv = vec2<i32>(i32(params.src_x) + sx, i32(params.src_y) + sy);
        let src = textureLoad(src_tex, src_uv, 0);
        let dst = result;

        // Alpha blend: src over dst, with opacity.
        let sa = src.a * params.opacity;
        let da = dst.a;
        let oa = sa + da * (1.0 - sa);

        if (oa > 0.001) {
            let rgb = (src.rgb * sa + dst.rgb * da * (1.0 - sa)) / oa;
            result = vec4<f32>(rgb, oa);
        }
    }

    textureStore(out_tex, gid.xy, result);
}
