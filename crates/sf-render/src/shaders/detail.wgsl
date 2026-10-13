// Detail pass: unsharp mask in linear light (chain step 11).
//
// blur = 0.25 * (4 von-Neumann neighbors, edges clamped);
// c += (c - blur) * sharpness01 * 1.5.
// Both src and dst are rgba16float linear textures.

struct DetailParams {
  sharpness01: f32,
  pad0: f32,
  pad1: f32,
  pad2: f32,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var dst_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> params: DetailParams;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  let dims = textureDimensions(src_tex);
  if (gid.x >= dims.x || gid.y >= dims.y) { return; }

  let x0 = select(gid.x, gid.x - 1u, gid.x > 0u);
  let x1 = select(gid.x, gid.x + 1u, gid.x + 1u < dims.x);
  let y0 = select(gid.y, gid.y - 1u, gid.y > 0u);
  let y1 = select(gid.y, gid.y + 1u, gid.y + 1u < dims.y);

  let px = textureLoad(src_tex, gid.xy, 0);
  let c = px.rgb;
  let blur = 0.25 * (
      textureLoad(src_tex, vec2u(x0, gid.y), 0).rgb
    + textureLoad(src_tex, vec2u(x1, gid.y), 0).rgb
    + textureLoad(src_tex, vec2u(gid.x, y0), 0).rgb
    + textureLoad(src_tex, vec2u(gid.x, y1), 0).rgb);
  let out = c + (c - blur) * params.sharpness01 * 1.5;
  textureStore(dst_tex, gid.xy, vec4f(out, px.a));
}
