//! GPU alpha-compositing pass.
//!
//! [`CompositePass`] owns one small compute pipeline that replaces the CPU
//! `composite_over`: it blends a rectangular region of `src` over `dst` at
//! a destination offset with a global opacity (src-over-dst). `dst` is
//! updated in place. The pipeline and bind-group layout are created once in
//! [`CompositePass::new`]; the bind group and the small params uniform are
//! built per call.

use wgpu::util::DeviceExt;

use crate::context::RenderContext;
use crate::error::RenderError;

fn bgl_entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}

fn compute_pipeline(
    device: &wgpu::Device,
    label: &str,
    bgl: &wgpu::BindGroupLayout,
    wgsl: &str,
) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[bgl],
        push_constant_ranges: &[],
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        module: &module,
        entry_point: "main",
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

/// GPU alpha blending (the GPU twin of the CPU `composite_over`).
///
/// Created once per [`RenderContext`]; all per-frame inputs are passed to
/// [`composite`](CompositePass::composite).
pub struct CompositePass {
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
}

impl CompositePass {
    /// Compiles the composite compute shader and creates the shared
    /// resources.
    ///
    /// # Errors
    ///
    /// This currently cannot fail (WGSL compile errors panic inside wgpu),
    /// but the `Result` keeps the door open for fallible pipeline creation.
    pub fn new(ctx: &RenderContext) -> Result<Self, RenderError> {
        let device = ctx.device();

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-render composite bgl"),
            entries: &[
                // binding 0: dst texture, read-write storage (updated in place).
                bgl_entry(
                    0,
                    wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::ReadWrite,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                ),
                // binding 1: src texture, sampled (shader uses textureLoad,
                // so non-filterable sampling is sufficient).
                bgl_entry(
                    1,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                // binding 2: params uniform.
                bgl_entry(
                    2,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
            ],
        });

        let pipeline = compute_pipeline(
            device,
            "sf-render composite",
            &bgl,
            include_str!("shaders/composite.wgsl"),
        );

        Ok(Self { pipeline, bgl })
    }

    /// Blends the rectangle `(src_x, src_y, src_w, src_h)` of `src` over
    /// `dst` at destination offset `(dst_x, dst_y)` with global `opacity`,
    /// updating `dst` in place (src-over-dst).
    ///
    /// # Errors
    ///
    /// Currently infallible; kept as `Result` for API symmetry with the
    /// other passes.
    pub fn composite(
        &self,
        ctx: &RenderContext,
        dst: &wgpu::TextureView,
        src: &wgpu::TextureView,
        src_rect: (u32, u32, u32, u32),
        dst_pos: (u32, u32),
        opacity: f32,
    ) -> Result<(), RenderError> {
        let (src_x, src_y, src_w, src_h) = src_rect;
        let (dst_x, dst_y) = dst_pos;
        // 32 bytes, matching the shader's `Params` struct:
        // src_x, src_y, src_w, src_h, dst_x, dst_y (u32), opacity, _pad (f32).
        let params: [u32; 8] = [
            src_x,
            src_y,
            src_w,
            src_h,
            dst_x,
            dst_y,
            opacity.to_bits(),
            0,
        ];
        let params_buf =
            ctx.device()
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("sf-render composite params"),
                    contents: bytemuck::cast_slice(&params),
                    usage: wgpu::BufferUsages::UNIFORM,
                });

        let bg = ctx
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sf-render composite bg"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(dst),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(src),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: params_buf.as_entire_binding(),
                    },
                ],
            });

        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sf-render composite"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sf-render composite compute"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bg, &[]);
            // Shader uses `@workgroup_size(8, 8)` and guards against the
            // source rect.
            pass.dispatch_workgroups(src_w.div_ceil(8), src_h.div_ceil(8), 1);
        }
        ctx.queue().submit(Some(encoder.finish()));
        Ok(())
    }
}
