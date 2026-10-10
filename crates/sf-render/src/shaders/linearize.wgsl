// Linearize pass: display-encoded rgba8unorm -> linear rgba16float.
//
// transfer 0.0 = pow(c, 2.4) (gamma 2.4 decode, the MVP default — matches
// GradeUniform.transfer), 1.0 = sRGB EOTF inverse:
//   c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055)^2.4.
// Alpha passes through untouched.
//
// Used when the effective grade is identity but effect filters are active:
// effects work in linear light, so the uploaded frame is linearized without
// applying any grade. Bind-group layout shape mirrors the encode pass
// (sampled src, storage dst, uniform), with the storage format swapped to
// rgba16float.

struct LinearizeParams {
  transfer: f32,
  pad0: f32,
  pad1: f32,
  pad2: f32,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var dst_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> params: LinearizeParams;

fn srgb_eotf_inv(c: vec3f) -> vec3f {
  return select(c / 12.92, pow((c + vec3f(0.055)) / vec3f(1.055), vec3f(2.4)), c > vec3f(0.04045));
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  let dims = textureDimensions(src_tex);
  if (gid.x >= dims.x || gid.y >= dims.y) { return; }

  let c = textureLoad(src_tex, gid.xy, 0);
  var rgb = c.rgb;
  if (params.transfer > 0.5) {
    rgb = srgb_eotf_inv(c.rgb);
  } else {
    rgb = pow(c.rgb, vec3f(2.4));
  }
  textureStore(dst_tex, gid.xy, vec4f(rgb, c.a));
}
