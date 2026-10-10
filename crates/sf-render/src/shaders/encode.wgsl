// Output encode pass (chain step 12): linear -> display transfer + clamp.
//
// transfer 0.0 = pow(c, 1/2.4) (MVP), 1.0 = sRGB OETF; then clamp 0..1.
// Reads linear rgba16float, writes display-encoded rgba8unorm.

struct EncodeParams {
  transfer: f32,
  pad0: f32,
  pad1: f32,
  pad2: f32,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var dst_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> params: EncodeParams;

fn srgb_oetf(c: vec3f) -> vec3f {
  return select(c * 12.92, 1.055 * pow(c, vec3f(1.0 / 2.4)) - vec3f(0.055), c > vec3f(0.0031308));
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  let dims = textureDimensions(src_tex);
  if (gid.x >= dims.x || gid.y >= dims.y) { return; }

  var c = max(textureLoad(src_tex, gid.xy, 0).rgb, vec3f(0.0));
  if (params.transfer > 0.5) {
    c = srgb_oetf(c);
  } else {
    c = pow(c, vec3f(1.0 / 2.4));
  }
  c = clamp(c, vec3f(0.0), vec3f(1.0));
  textureStore(dst_tex, gid.xy, vec4f(c, 1.0));
}
