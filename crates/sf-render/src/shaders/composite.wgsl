// GPU alpha compositing: blends source over destination.
// Replaces the CPU `composite_over` for the hybrid pipeline.
//
// Bindings:
//   @group(0) @binding(0): dst texture (read-write, will be updated)
//   @group(0) @binding(1): src texture (read-only)
//   @group(0) @binding(2): params (src rect, dst offset, opacity)

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

@group(0) @binding(0) var dst_tex: texture_storage_2d<rgba8unorm, read_write>;
@group(0) @binding(1) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let y = gid.y;
    
    // Bounds check against src rect
    if (x >= params.src_w || y >= params.src_h) {
        return;
    }
    
    let dst_px = vec2<i32>(i32(params.dst_x + x), i32(params.dst_y + y));
    let src_uv = vec2<i32>(i32(params.src_x + x), i32(params.src_y + y));
    
    let src = textureLoad(src_tex, src_uv, 0);
    var dst = textureLoad(dst_tex, dst_px);
    
    // Alpha blend: src over dst, with opacity
    let sa = src.a * params.opacity;
    let da = dst.a;
    let oa = sa + da * (1.0 - sa);
    
    if (oa > 0.001) {
        let rgb = (src.rgb * sa + dst.rgb * da * (1.0 - sa)) / oa;
        dst = vec4<f32>(rgb, oa);
    }
    
    textureStore(dst_tex, dst_px, dst);
}
