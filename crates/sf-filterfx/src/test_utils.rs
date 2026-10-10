//! Shared GPU test helpers: f32 <-> `rgba16float` texture upload/download.
//!
//! Deliberately `pub` (not `#[cfg(test)]`-gated) so integration tests in this
//! crate, later C5 workers, and engine-level tests can all reuse them.

use sf_render::f16::{f16_to_f32, f32_to_f16};
use sf_render::RenderContext;

/// Bytes per `Rgba16Float` texel.
const BPP: u32 = 8;

/// 256-byte-aligned row pitch, required by `write_texture` and
/// texture-to-buffer copies.
fn padded_row_bytes(w: u32) -> u32 {
    (w * BPP).div_ceil(256) * 256
}

/// Creates a fresh `rgba16float` texture sized `w`x`h`, usable as an effect
/// source or destination (TEXTURE/STORAGE binding plus COPY_DST for uploads
/// and COPY_SRC for downloads).
fn linear_texture(ctx: &RenderContext, w: u32, h: u32, label: &str) -> wgpu::Texture {
    ctx.device().create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Uploads a row-major `w*h*4` f32 linear frame into an `rgba16float`
/// texture (f32 -> f16, round-to-nearest-even per channel).
///
/// # Panics
///
/// Panics when `data.len()` is not `w * h * 4`.
pub fn upload_f32_linear(ctx: &RenderContext, w: u32, h: u32, data: &[f32]) -> wgpu::Texture {
    assert_eq!(data.len(), (w * h * 4) as usize);
    let tex = linear_texture(ctx, w, h, "sf-filterfx test upload");
    let row = padded_row_bytes(w);
    let mut bytes = vec![0u8; (row * h) as usize];
    for (i, &v) in data.iter().enumerate() {
        let px = i / 4;
        let (x, y) = ((px as u32) % w, (px as u32) / w);
        let off = (y * row + x * BPP + (i % 4) as u32 * 2) as usize;
        bytes[off..off + 2].copy_from_slice(&f32_to_f16(v).to_le_bytes());
    }
    ctx.queue().write_texture(
        wgpu::ImageCopyTexture {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(row),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    tex
}

/// Downloads an `rgba16float` texture into a row-major `w*h*4` f32 frame
/// (f16 -> f32, exact per channel).
pub fn download_f32_linear(ctx: &RenderContext, tex: &wgpu::Texture) -> Vec<f32> {
    let size = tex.size();
    let (w, h) = (size.width, size.height);
    let row = padded_row_bytes(w);
    let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("sf-filterfx test download"),
        size: (row as u64) * (h as u64),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("sf-filterfx test download"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &buffer,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    ctx.queue().submit(Some(encoder.finish()));

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    // Drive the mapping to completion on this thread.
    ctx.device().poll(wgpu::Maintain::Wait);
    rx.recv().expect("map channel dropped").expect("map failed");

    let out = {
        let mapped = slice.get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let base = y * row as usize + x * BPP as usize;
                for c in 0..4 {
                    let bits = u16::from_le_bytes([mapped[base + c * 2], mapped[base + c * 2 + 1]]);
                    out.push(f16_to_f32(bits));
                }
            }
        }
        out
    };
    buffer.unmap();
    out
}

/// Round-trips an f32 frame through f16: the exact values the GPU reads after
/// an [`upload_f32_linear`] (what the CPU references must be fed to match the
/// GPU bit-for-bit, before the shader's own f16 store rounding).
pub fn quantize_f16(data: &[f32]) -> Vec<f32> {
    data.iter().map(|&v| f16_to_f32(f32_to_f16(v))).collect()
}
