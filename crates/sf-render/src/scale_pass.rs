//! GPU bilinear scaling pass.
//!
//! [`ScalePass`] owns one small compute pipeline that replaces the CPU
//! `resize_rgba_nearest`: it samples `src` with bilinear filtering and
//! writes the result into `dst` (whose size defines the output size).
//! The pipeline and bind-group layout are created once in
//! [`ScalePass::new`]; the bind group is built per call.

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

/// Bilinear GPU scaling (the GPU twin of the CPU resize path).
///
/// Created once per [`RenderContext`]; all per-frame inputs are passed to
/// [`scale`](ScalePass::scale).
pub struct ScalePass {
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
    /// Linear sampler shared by every [`scale`](ScalePass::scale) call.
    sampler: wgpu::Sampler,
}

impl ScalePass {
    /// Compiles the scale compute shader and creates the shared resources.
    ///
    /// # Errors
    ///
    /// This currently cannot fail (WGSL compile errors panic inside wgpu),
    /// but the `Result` keeps the door open for fallible pipeline creation.
    pub fn new(ctx: &RenderContext) -> Result<Self, RenderError> {
        let device = ctx.device();

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-render scale bgl"),
            entries: &[
                // binding 0: src texture, sampled with bilinear filtering.
                bgl_entry(
                    0,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                // binding 1: dst texture, write-only storage.
                bgl_entry(
                    1,
                    wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                ),
                // binding 2: linear sampler.
                bgl_entry(
                    2,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sf-render scale sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            lod_min_clamp: 0.0,
            lod_max_clamp: 0.0,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
        });

        let pipeline = compute_pipeline(
            device,
            "sf-render scale",
            &bgl,
            include_str!("shaders/scale.wgsl"),
        );

        Ok(Self {
            pipeline,
            bgl,
            sampler,
        })
    }

    /// Scales `src` (of size `src_w` x `src_h`) into `dst` (of size
    /// `dst_w` x `dst_h`) with bilinear filtering. The output size is taken
    /// from the `dst_*` parameters; the shader additionally guards against
    /// overrunning the destination texture.
    ///
    /// # Errors
    ///
    /// Currently infallible; kept as `Result` for API symmetry with the
    /// other passes.
    pub fn scale(
        &self,
        ctx: &RenderContext,
        src: &wgpu::TextureView,
        dst: &wgpu::TextureView,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<(), RenderError> {
        let _ = (src_w, src_h); // Documented via signature; size comes from views.
        let device = ctx.device();
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-render scale bg"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(src),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(dst),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sf-render scale"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sf-render scale compute"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bg, &[]);
            // Shader uses `@workgroup_size(8, 8)`.
            pass.dispatch_workgroups(dst_w.div_ceil(8), dst_h.div_ceil(8), 1);
        }
        ctx.queue().submit(Some(encoder.finish()));
        Ok(())
    }
}
