//! GPU scope passes: downscale + one compute shader per scope kind.
//!
//! [`ScopePass`] is created once per [`RenderContext`](sf_render::RenderContext).
//! [`ScopePass::downscale`] reduces the graded RGBA8 frame to the 256x144
//! analysis texture; [`ScopePass::analyze`] then runs the requested scope on
//! it and downloads the (small) result. Statistical scopes (waveform,
//! vectorscope, histogram) reduce into zero-initialized storage buffers via
//! atomics or a column scan; image scopes (false color, zebra) write RGBA8
//! overlays.
//!
//! wgpu zero-initializes freshly created buffers, so the atomic histogram
//! buffers need no explicit clear.

use wgpu::util::DeviceExt;

use sf_render::{RenderContext, RenderError};

use super::{
    FalseColorImage, HistogramData, ScopeKind, ScopePayload, VectorscopeData, WaveformData,
    ZebraOverlay, SCOPE_H, SCOPE_W, VSCOPE_BINS,
};

/// Parameters for the image scopes.
#[derive(Clone, Copy, Debug)]
pub struct ScopeParams {
    /// Luma above this is zebra-striped (overexposed). Default 0.95.
    pub zebra_hi: f32,
    /// Luma below this is zebra-striped (underexposed). Default 0.05.
    pub zebra_lo: f32,
}

impl Default for ScopeParams {
    fn default() -> Self {
        Self {
            zebra_hi: 0.95,
            zebra_lo: 0.05,
        }
    }
}

fn bgl_entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}

fn sampled_2d() -> wgpu::BindingType {
    wgpu::BindingType::Texture {
        sample_type: wgpu::TextureSampleType::Float { filterable: false },
        view_dimension: wgpu::TextureViewDimension::D2,
        multisampled: false,
    }
}

fn storage_2d_write() -> wgpu::BindingType {
    wgpu::BindingType::StorageTexture {
        access: wgpu::StorageTextureAccess::WriteOnly,
        format: wgpu::TextureFormat::Rgba8Unorm,
        view_dimension: wgpu::TextureViewDimension::D2,
    }
}

fn storage_buffer_rw() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only: false },
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

fn uniform() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: false,
        min_binding_size: None,
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

/// Zero-initialized storage buffer (wgpu guarantees zero-init on creation).
fn zeroed_storage(ctx: &RenderContext, bytes: u64, label: &str) -> wgpu::Buffer {
    ctx.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

/// Copies a GPU storage buffer into a CPU `Vec<u8>` via a readback buffer.
fn download_storage(
    ctx: &RenderContext,
    buf: &wgpu::Buffer,
    bytes: u64,
) -> Result<Vec<u8>, RenderError> {
    let readback = ctx.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("sf-scopes readback"),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = ctx
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("sf-scopes readback"),
        });
    enc.copy_buffer_to_buffer(buf, 0, &readback, 0, bytes);
    ctx.queue().submit(Some(enc.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    ctx.device().poll(wgpu::Maintain::Wait);
    rx.recv()
        .map_err(|_| RenderError::MapFailed)?
        .map_err(|_| RenderError::MapFailed)?;
    let out = slice.get_mapped_range().to_vec();
    readback.unmap();
    Ok(out)
}

/// The six scope compute pipelines.
///
/// Created once per [`RenderContext`](sf_render::RenderContext); per-call
/// resources (analysis texture, stat buffers, bind groups) are built on
/// demand. Scopes run at most at 15 Hz and only while subscribed, so
/// per-call allocation here is cheap relative to the frame budget.
pub struct ScopePass {
    downscale_pipeline: wgpu::ComputePipeline,
    waveform_pipeline: wgpu::ComputePipeline,
    vectorscope_pipeline: wgpu::ComputePipeline,
    histogram_pipeline: wgpu::ComputePipeline,
    falsecolor_pipeline: wgpu::ComputePipeline,
    zebra_pipeline: wgpu::ComputePipeline,
    downscale_bgl: wgpu::BindGroupLayout,
    stats_bgl: wgpu::BindGroupLayout,
    image_bgl: wgpu::BindGroupLayout,
    zebra_bgl: wgpu::BindGroupLayout,
}

impl ScopePass {
    /// Compiles the six scope shaders.
    ///
    /// # Errors
    ///
    /// This currently cannot fail (WGSL compile errors panic inside wgpu),
    /// but the `Result` keeps the door open for fallible pipeline creation.
    pub fn new(ctx: &RenderContext) -> Result<Self, RenderError> {
        let device = ctx.device();

        let downscale_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-scopes downscale bgl"),
            entries: &[bgl_entry(0, sampled_2d()), bgl_entry(1, storage_2d_write())],
        });
        // Shared by waveform / vectorscope / histogram: sampled input +
        // read-write storage buffer for the reduction.
        let stats_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-scopes stats bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, storage_buffer_rw()),
            ],
        });
        let image_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-scopes image bgl"),
            entries: &[bgl_entry(0, sampled_2d()), bgl_entry(1, storage_2d_write())],
        });
        let zebra_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-scopes zebra bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, storage_2d_write()),
                bgl_entry(2, uniform()),
            ],
        });

        Ok(Self {
            downscale_pipeline: compute_pipeline(
                device,
                "sf-scopes downscale",
                &downscale_bgl,
                include_str!("shaders/downscale.wgsl"),
            ),
            waveform_pipeline: compute_pipeline(
                device,
                "sf-scopes waveform",
                &stats_bgl,
                include_str!("shaders/waveform.wgsl"),
            ),
            vectorscope_pipeline: compute_pipeline(
                device,
                "sf-scopes vectorscope",
                &stats_bgl,
                include_str!("shaders/vectorscope.wgsl"),
            ),
            histogram_pipeline: compute_pipeline(
                device,
                "sf-scopes histogram",
                &stats_bgl,
                include_str!("shaders/histogram.wgsl"),
            ),
            falsecolor_pipeline: compute_pipeline(
                device,
                "sf-scopes falsecolor",
                &image_bgl,
                include_str!("shaders/falsecolor.wgsl"),
            ),
            zebra_pipeline: compute_pipeline(
                device,
                "sf-scopes zebra",
                &zebra_bgl,
                include_str!("shaders/zebra.wgsl"),
            ),
            downscale_bgl,
            stats_bgl,
            image_bgl,
            zebra_bgl,
        })
    }

    /// Downscales a graded RGBA8 frame to the 256x144 analysis texture.
    ///
    /// The returned texture has `STORAGE_BINDING | TEXTURE_BINDING | COPY_SRC`
    /// usage so scope shaders can sample it and tests can download it.
    pub fn downscale(
        &self,
        ctx: &RenderContext,
        src: &wgpu::Texture,
    ) -> Result<wgpu::Texture, RenderError> {
        let device = ctx.device();
        let dst = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-scopes analysis 256x144"),
            size: wgpu::Extent3d {
                width: SCOPE_W,
                height: SCOPE_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-scopes downscale bg"),
            layout: &self.downscale_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &src.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        &dst.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
            ],
        });
        Self::dispatch(
            ctx,
            &self.downscale_pipeline,
            &bg,
            SCOPE_W.div_ceil(16),
            SCOPE_H.div_ceil(16),
        );
        Ok(dst)
    }

    /// Runs one scope on the 256x144 analysis texture and downloads the result.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::SizeMismatch`] in debug builds when `small` is
    /// not 256x144.
    pub fn analyze(
        &self,
        ctx: &RenderContext,
        kind: ScopeKind,
        small: &wgpu::Texture,
        params: &ScopeParams,
    ) -> Result<ScopePayload, RenderError> {
        debug_assert_eq!(
            (small.size().width, small.size().height),
            (SCOPE_W, SCOPE_H),
            "scope analysis texture must be 256x144"
        );
        match kind {
            ScopeKind::Waveform => Ok(ScopePayload::Waveform(self.waveform(ctx, small)?)),
            ScopeKind::Vectorscope => Ok(ScopePayload::Vectorscope(self.vectorscope(ctx, small)?)),
            ScopeKind::Histogram => Ok(ScopePayload::Histogram(self.histogram(ctx, small)?)),
            ScopeKind::FalseColor => Ok(ScopePayload::FalseColor(self.false_color(ctx, small)?)),
            ScopeKind::Zebra => Ok(ScopePayload::Zebra(self.zebra(ctx, small, params)?)),
        }
    }

    /// Waveform scan → per-column min/max.
    pub fn waveform(
        &self,
        ctx: &RenderContext,
        small: &wgpu::Texture,
    ) -> Result<WaveformData, RenderError> {
        let buf = zeroed_storage(ctx, 256 * 8 * 4, "sf-scopes waveform");
        self.run_stats(ctx, &self.waveform_pipeline, small, &buf, 256, 1);
        let bytes = download_storage(ctx, &buf, 256 * 8 * 4)?;
        let words: &[f32] = bytemuck::cast_slice(&bytes);
        debug_assert_eq!(words.len(), 256 * 8);
        let mut d = WaveformData::empty();
        for x in 0..256 {
            let o = x * 8;
            d.luma_min[x] = words[o];
            d.luma_max[x] = words[o + 1];
            d.r_min[x] = words[o + 2];
            d.r_max[x] = words[o + 3];
            d.g_min[x] = words[o + 4];
            d.g_max[x] = words[o + 5];
            d.b_min[x] = words[o + 6];
            d.b_max[x] = words[o + 7];
        }
        Ok(d)
    }

    /// Vectorscope → 128x128 chroma bin grid.
    pub fn vectorscope(
        &self,
        ctx: &RenderContext,
        small: &wgpu::Texture,
    ) -> Result<VectorscopeData, RenderError> {
        let n = VSCOPE_BINS as u64;
        let buf = zeroed_storage(ctx, n * n * 4, "sf-scopes vectorscope");
        self.run_stats(
            ctx,
            &self.vectorscope_pipeline,
            small,
            &buf,
            SCOPE_W.div_ceil(16),
            SCOPE_H.div_ceil(16),
        );
        let bytes = download_storage(ctx, &buf, n * n * 4)?;
        let words: &[u32] = bytemuck::cast_slice(&bytes);
        Ok(VectorscopeData {
            bins: words.to_vec(),
        })
    }

    /// Histogram → 256 bins x (R, G, B, luma).
    pub fn histogram(
        &self,
        ctx: &RenderContext,
        small: &wgpu::Texture,
    ) -> Result<HistogramData, RenderError> {
        let buf = zeroed_storage(ctx, 4 * 256 * 4, "sf-scopes histogram");
        self.run_stats(
            ctx,
            &self.histogram_pipeline,
            small,
            &buf,
            SCOPE_W.div_ceil(16),
            SCOPE_H.div_ceil(16),
        );
        let bytes = download_storage(ctx, &buf, 4 * 256 * 4)?;
        let words: &[u32] = bytemuck::cast_slice(&bytes);
        debug_assert_eq!(words.len(), 1024);
        let mut d = HistogramData {
            r: [0; 256],
            g: [0; 256],
            b: [0; 256],
            luma: [0; 256],
        };
        d.r.copy_from_slice(&words[0..256]);
        d.g.copy_from_slice(&words[256..512]);
        d.b.copy_from_slice(&words[512..768]);
        d.luma.copy_from_slice(&words[768..1024]);
        Ok(d)
    }

    /// False-color exposure image (256x144 RGB).
    pub fn false_color(
        &self,
        ctx: &RenderContext,
        small: &wgpu::Texture,
    ) -> Result<FalseColorImage, RenderError> {
        let out = ctx.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-scopes falsecolor"),
            size: wgpu::Extent3d {
                width: SCOPE_W,
                height: SCOPE_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.run_image(
            ctx,
            &self.falsecolor_pipeline,
            &self.image_bgl,
            small,
            &out,
            None,
        );
        let rgba = ctx.download_rgba8(&out)?;
        let rgb: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|c| [c[0], c[1], c[2]])
            .collect();
        Ok(FalseColorImage {
            width: SCOPE_W,
            height: SCOPE_H,
            rgb,
        })
    }

    /// Zebra overlay (256x144 RGBA) with the given thresholds.
    pub fn zebra(
        &self,
        ctx: &RenderContext,
        small: &wgpu::Texture,
        params: &ScopeParams,
    ) -> Result<ZebraOverlay, RenderError> {
        let out = ctx.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-scopes zebra"),
            size: wgpu::Extent3d {
                width: SCOPE_W,
                height: SCOPE_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let params_buf = ctx
            .device()
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("sf-scopes zebra params"),
                contents: bytemuck::bytes_of(&[params.zebra_hi, params.zebra_lo, 0.0f32, 0.0]),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        self.run_image(
            ctx,
            &self.zebra_pipeline,
            &self.zebra_bgl,
            small,
            &out,
            Some(&params_buf),
        );
        let rgba = ctx.download_rgba8(&out)?;
        Ok(ZebraOverlay {
            width: SCOPE_W,
            height: SCOPE_H,
            rgba,
        })
    }

    /// Runs a stats (storage-buffer) scope: binds the analysis texture and
    /// the zeroed buffer, dispatches, and waits via the download's poll.
    fn run_stats(
        &self,
        ctx: &RenderContext,
        pipeline: &wgpu::ComputePipeline,
        small: &wgpu::Texture,
        buf: &wgpu::Buffer,
        groups_x: u32,
        groups_y: u32,
    ) {
        let device = ctx.device();
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-scopes stats bg"),
            layout: &self.stats_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &small.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buf.as_entire_binding(),
                },
            ],
        });
        Self::dispatch(ctx, pipeline, &bg, groups_x, groups_y);
    }

    /// Runs an image (storage-texture) scope, with an optional params buffer
    /// (zebra).
    #[allow(clippy::too_many_arguments)]
    fn run_image(
        &self,
        ctx: &RenderContext,
        pipeline: &wgpu::ComputePipeline,
        bgl: &wgpu::BindGroupLayout,
        small: &wgpu::Texture,
        out: &wgpu::Texture,
        params: Option<&wgpu::Buffer>,
    ) {
        let device = ctx.device();
        let small_view = small.create_view(&wgpu::TextureViewDescriptor::default());
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&small_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&out_view),
            },
        ];
        if let Some(p) = params {
            entries.push(wgpu::BindGroupEntry {
                binding: 2,
                resource: p.as_entire_binding(),
            });
        }
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-scopes image bg"),
            layout: bgl,
            entries: &entries,
        });
        Self::dispatch(
            ctx,
            pipeline,
            &bg,
            SCOPE_W.div_ceil(16),
            SCOPE_H.div_ceil(16),
        );
    }

    /// Records one compute dispatch and submits it.
    fn dispatch(
        ctx: &RenderContext,
        pipeline: &wgpu::ComputePipeline,
        bg: &wgpu::BindGroup,
        groups_x: u32,
        groups_y: u32,
    ) {
        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sf-scopes pass"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sf-scopes compute"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bg, &[]);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
        }
        ctx.queue().submit(Some(encoder.finish()));
    }
}
