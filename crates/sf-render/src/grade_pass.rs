//! The four small compute pipelines of the color chain.
//!
//! [`GradePass`] owns the fused [`grade`](GradePass::apply) pass (the GPU twin
//! of [`sf_color::cpu::apply_grade_cpu`]), the optional
//! [`unsharp-mask`](GradePass::apply_detail) pass, the
//! [`output encode`](GradePass::encode_output) pass, and the
//! [`linearize`](GradePass::linearize) pass (grade-less sRGB -> linear for
//! the effect chain). Per-call resources (uniform buffer, baked-curves
//! texture, LUT texture, bind groups) are built on demand; the pipelines,
//! bind-group layouts, sampler, and the dummy LUT are created once in
//! [`GradePass::new`].

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use wgpu::util::DeviceExt;

use sf_color::curves::BakedCurves;
use sf_color::grade::ColorGrade;
use sf_color::lut::CubeFile;

use crate::context::RenderContext;
use crate::error::RenderError;
use crate::f16::f32_to_f16;

/// Normalized 3D LUT lattice size everything is resampled to (see `sf-color`).
const LUT_SIZE: u32 = 33;
/// Baked 1D curve resolution.
const CURVE_SIZE: u32 = 1024;

/// Content hash for cache dirty-checking.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Cached per-grade GPU resources for [`GradePass::apply`].
///
/// The LUT is keyed by the address of the caller's [`CubeFile`] (as `usize`,
/// not a raw pointer, so no auto-trait issues). Contract: the caller keeps
/// the `CubeFile` alive across calls and never mutates it in place — the
/// same address then always means the same content. The engine's LUT cache
/// owns its `CubeFile`s for the whole session, so this holds.
struct GradeCache {
    uniform_hash: u64,
    curves_hash: u64,
    lut_addr: Option<usize>,
    uniform_buf: wgpu::Buffer,
    curves_view: wgpu::TextureView,
    lut_view: wgpu::TextureView,
}

/// The four small compute pipelines of the color chain.
///
/// Created once per [`RenderContext`]; all per-frame inputs are passed to
/// [`apply`](GradePass::apply), [`apply_detail`](GradePass::apply_detail),
/// [`encode_output`](GradePass::encode_output) and
/// [`linearize`](GradePass::linearize).
pub struct GradePass {
    grade_pipeline: wgpu::ComputePipeline,
    detail_pipeline: wgpu::ComputePipeline,
    encode_pipeline: wgpu::ComputePipeline,
    linearize_pipeline: wgpu::ComputePipeline,
    grade_bgl: wgpu::BindGroupLayout,
    detail_bgl: wgpu::BindGroupLayout,
    encode_bgl: wgpu::BindGroupLayout,
    linearize_bgl: wgpu::BindGroupLayout,
    linear_sampler: wgpu::Sampler,
    /// 1x1x1 `rgba16float` 3D texture bound when no LUT is active (the shader
    /// never samples it then, but the bind group must stay valid).
    dummy_lut: wgpu::Texture,
    /// Cached uniform buffer / curves texture / LUT texture, rebuilt only
    /// when their content hash changes (dirty-flag caching).
    cache: Option<GradeCache>,
    /// Cache accounting: (resource hits, resource misses); three resources
    /// are checked per [`apply`](GradePass::apply) call.
    cache_stats: (u64, u64),
    /// Reused 16-byte param buffer shared by [`apply_detail`](GradePass::apply_detail),
    /// [`encode_output`](GradePass::encode_output) and
    /// [`linearize`](GradePass::linearize) (C8): all three upload one
    /// `vec4f` (`[param, 0, 0, 0]`). The buffer is allocated once and
    /// rewritten with `queue.write_buffer` only when the value changes,
    /// instead of a `create_buffer_init` per call. `RefCell` keeps the
    /// `&self` method signatures; the codebase already uses interior
    /// mutability for this kind of accounting (`EffectPass::passes`).
    params_buf: RefCell<Option<([f32; 4], wgpu::Buffer)>>,
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

fn storage_2d_write(format: wgpu::TextureFormat) -> wgpu::BindingType {
    wgpu::BindingType::StorageTexture {
        access: wgpu::StorageTextureAccess::WriteOnly,
        format,
        view_dimension: wgpu::TextureViewDimension::D2,
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

impl GradePass {
    /// Compiles the four compute shaders and creates the shared resources.
    ///
    /// # Errors
    ///
    /// This currently cannot fail (WGSL compile errors panic inside wgpu),
    /// but the `Result` keeps the door open for fallible pipeline creation.
    pub fn new(ctx: &RenderContext) -> Result<Self, RenderError> {
        let device = ctx.device();

        let grade_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-render grade bgl"),
            entries: &[
                bgl_entry(0, uniform()),
                bgl_entry(1, sampled_2d()),
                bgl_entry(2, storage_2d_write(wgpu::TextureFormat::Rgba16Float)),
                bgl_entry(3, sampled_2d()),
                bgl_entry(
                    4,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                ),
                bgl_entry(
                    5,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
            ],
        });
        let detail_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-render detail bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, storage_2d_write(wgpu::TextureFormat::Rgba16Float)),
                bgl_entry(2, uniform()),
            ],
        });
        let encode_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-render encode bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, storage_2d_write(wgpu::TextureFormat::Rgba8Unorm)),
                bgl_entry(2, uniform()),
            ],
        });
        // Same binding shape as the encode layout (sampled src, storage dst,
        // uniform), but the storage target is linear rgba16float.
        let linearize_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-render linearize bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, storage_2d_write(wgpu::TextureFormat::Rgba16Float)),
                bgl_entry(2, uniform()),
            ],
        });

        let grade_pipeline = compute_pipeline(
            device,
            "sf-render grade",
            &grade_bgl,
            include_str!("shaders/grade.wgsl"),
        );
        let detail_pipeline = compute_pipeline(
            device,
            "sf-render detail",
            &detail_bgl,
            include_str!("shaders/detail.wgsl"),
        );
        let encode_pipeline = compute_pipeline(
            device,
            "sf-render encode",
            &encode_bgl,
            include_str!("shaders/encode.wgsl"),
        );
        let linearize_pipeline = compute_pipeline(
            device,
            "sf-render linearize",
            &linearize_bgl,
            include_str!("shaders/linearize.wgsl"),
        );

        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sf-render lut sampler"),
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

        let dummy_lut = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-render dummy lut"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue().write_texture(
            wgpu::ImageCopyTexture {
                texture: &dummy_lut,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0u8; 8],
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(8),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        Ok(Self {
            grade_pipeline,
            detail_pipeline,
            encode_pipeline,
            linearize_pipeline,
            grade_bgl,
            detail_bgl,
            encode_bgl,
            linearize_bgl,
            linear_sampler,
            dummy_lut,
            cache: None,
            cache_stats: (0, 0),
            params_buf: RefCell::new(None),
        })
    }

    /// Cache accounting: `(resource_hits, resource_misses)` since creation.
    ///
    /// Three resources (uniform buffer, curves texture, LUT texture) are
    /// checked per [`apply`](GradePass::apply) call, so one fully-cached
    /// call contributes 3 hits.
    pub fn cache_stats(&self) -> (u64, u64) {
        self.cache_stats
    }

    /// Returns the shared 16-byte param buffer holding `value` (C8).
    ///
    /// Allocated once (with `COPY_DST`); rewritten via `queue.write_buffer`
    /// only when the value changes — `transfer`/`sharpness` change rarely —
    /// instead of allocating a buffer per call. The shader always sees
    /// exactly `[param, 0, 0, 0]`, so pixel output is unchanged.
    fn params_buffer(
        &self,
        ctx: &RenderContext,
        value: [f32; 4],
    ) -> std::cell::RefMut<'_, wgpu::Buffer> {
        let mut cache = self.params_buf.borrow_mut();
        match cache.as_mut() {
            Some((v, buf)) if *v == value => {}
            Some((v, buf)) => {
                ctx.queue().write_buffer(buf, 0, bytemuck::bytes_of(&value));
                *v = value;
            }
            None => {
                let buf = ctx.device().create_buffer(&wgpu::BufferDescriptor {
                    label: Some("sf-render params"),
                    size: 16,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                ctx.queue()
                    .write_buffer(&buf, 0, bytemuck::bytes_of(&value));
                *cache = Some((value, buf));
            }
        }
        std::cell::RefMut::map(cache, |c| {
            &mut c.as_mut().expect("params cache filled above").1
        })
    }

    /// Fused color grade: `src` (`rgba8unorm`) -> `dst` (`rgba16float` linear,
    /// no output encode).
    ///
    /// Returns `Ok(false)` **without touching the GPU** when
    /// [`ColorGrade::is_identity`] is true — the caller keeps using the
    /// source in that case.
    ///
    /// Per-grade resources are dirty-flag cached: the 160-byte uniform
    /// buffer (from [`ColorGrade::to_uniform`](sf_color::grade::ColorGrade::to_uniform)),
    /// the 1024x2 `rgba32float` curves texture (from `baked.data`,
    /// interleaved to texels on upload), and the 33^3 `rgba16float` 3D LUT
    /// texture (or the 1^3 dummy texture when no LUT is given) are rebuilt
    /// only when their content hash changes. The 3D upload pads each row
    /// from 33*8=264 bytes to the 256-aligned 512 bytes. The bind group is
    /// rebuilt every call (cheap) because the src/dst views differ per
    /// frame. See [`cache_stats`](GradePass::cache_stats).
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::SizeMismatch`] when `src` and `dst` differ in
    /// size.
    pub fn apply(
        &mut self,
        ctx: &RenderContext,
        grade: &ColorGrade,
        baked: &BakedCurves,
        lut: Option<&CubeFile>,
        src: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<bool, RenderError> {
        if grade.is_identity() {
            return Ok(false);
        }
        if src.size() != dst.size() {
            return Err(RenderError::SizeMismatch);
        }
        let device = ctx.device();

        let uniform = grade.to_uniform();
        let uniform_hash = hash_bytes(bytemuck::bytes_of(&uniform));
        let curves_hash = hash_bytes(bytemuck::cast_slice::<f32, u8>(&baked.data));
        let lut_addr = lut.map(|c| std::ptr::from_ref(c) as usize);

        // Take the old cache apart; each resource is reused iff its content
        // hash still matches, otherwise rebuilt.
        let old = self.cache.take();
        let (mut old_uniform, mut old_curves, mut old_lut) = (None, None, None);
        let (mut old_uh, mut old_ch, mut old_la) = (0u64, 0u64, None);
        if let Some(c) = old {
            old_uniform = Some(c.uniform_buf);
            old_curves = Some(c.curves_view);
            old_lut = Some(c.lut_view);
            old_uh = c.uniform_hash;
            old_ch = c.curves_hash;
            old_la = c.lut_addr;
        }
        let (mut hits, mut misses) = (0u64, 0u64);

        let uniform_buf = match old_uniform {
            Some(buf) if old_uh == uniform_hash => {
                hits += 1;
                buf
            }
            _ => {
                misses += 1;
                device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("sf-render grade uniform"),
                    contents: bytemuck::bytes_of(&uniform),
                    usage: wgpu::BufferUsages::UNIFORM,
                })
            }
        };
        let curves_view = match old_curves {
            Some(view) if old_ch == curves_hash => {
                hits += 1;
                view
            }
            _ => {
                misses += 1;
                Self::upload_curves(ctx, baked)
            }
        };
        let lut_view = match old_lut {
            Some(view) if old_la == lut_addr => {
                hits += 1;
                view
            }
            _ => {
                misses += 1;
                match lut {
                    Some(cube) => Self::upload_lut(ctx, cube),
                    None => self
                        .dummy_lut
                        .create_view(&wgpu::TextureViewDescriptor::default()),
                }
            }
        };
        self.cache_stats.0 += hits;
        self.cache_stats.1 += misses;
        self.cache = Some(GradeCache {
            uniform_hash,
            curves_hash,
            lut_addr,
            uniform_buf,
            curves_view,
            lut_view,
        });
        let cached = self.cache.as_ref().expect("cache just stored");
        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());

        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-render grade bg"),
            layout: &self.grade_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: cached.uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&cached.curves_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&cached.lut_view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
                },
            ],
        });

        let size = src.size();
        Self::dispatch(ctx, &self.grade_pipeline, &bg, size.width, size.height);
        Ok(true)
    }

    /// Optional unsharp mask in linear light (chain step 11).
    ///
    /// `src` and `dst` are both `rgba16float` linear textures of the same
    /// size (they must be different textures — the pass reads `src` while
    /// writing `dst`). `sharpness01` is the 0..1 normalized sharpness.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::SizeMismatch`] when `src` and `dst` differ in
    /// size.
    pub fn apply_detail(
        &self,
        ctx: &RenderContext,
        sharpness01: f32,
        src: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<(), RenderError> {
        if src.size() != dst.size() {
            return Err(RenderError::SizeMismatch);
        }
        let device = ctx.device();
        let params_buf = self.params_buffer(ctx, [sharpness01, 0.0, 0.0, 0.0]);
        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-render detail bg"),
            layout: &self.detail_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });
        let size = src.size();
        Self::dispatch(ctx, &self.detail_pipeline, &bg, size.width, size.height);
        Ok(())
    }

    /// Output encode: linear `rgba16float` `src` -> display-encoded
    /// `rgba8unorm` `dst`.
    ///
    /// `transfer` 0.0 = `pow(c, 1/2.4)`, 1.0 = sRGB OETF; the result is
    /// clamped to 0..1 before the 8-bit store.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::SizeMismatch`] when `src` and `dst` differ in
    /// size.
    pub fn encode_output(
        &self,
        ctx: &RenderContext,
        transfer: f32,
        src: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<(), RenderError> {
        if src.size() != dst.size() {
            return Err(RenderError::SizeMismatch);
        }
        let device = ctx.device();
        let params_buf = self.params_buffer(ctx, [transfer, 0.0, 0.0, 0.0]);
        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-render encode bg"),
            layout: &self.encode_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });
        let size = src.size();
        Self::dispatch(ctx, &self.encode_pipeline, &bg, size.width, size.height);
        Ok(())
    }

    /// Linearize-only pass: display-encoded `rgba8unorm` `src` -> linear
    /// `rgba16float` `dst` (no grade applied).
    ///
    /// Used when the effective grade is identity but effect filters are
    /// active: effects work in linear light, so the uploaded frame must be
    /// linearized without touching its colors. `transfer` 0.0 =
    /// `pow(c, 2.4)` (MVP default, matches `GradeUniform.transfer`), 1.0 =
    /// sRGB EOTF inverse; alpha passes through untouched. The bind-group
    /// layout mirrors the encode pass's shape (sampled src, storage dst,
    /// uniform) with the storage format swapped to `rgba16float`.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::SizeMismatch`] when `src` and `dst` differ in
    /// size.
    pub fn linearize(
        &self,
        ctx: &RenderContext,
        transfer: f32,
        src: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<(), RenderError> {
        if src.size() != dst.size() {
            return Err(RenderError::SizeMismatch);
        }
        let device = ctx.device();
        let params_buf = self.params_buffer(ctx, [transfer, 0.0, 0.0, 0.0]);
        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-render linearize bg"),
            layout: &self.linearize_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });
        let size = src.size();
        Self::dispatch(ctx, &self.linearize_pipeline, &bg, size.width, size.height);
        Ok(())
    }

    /// Uploads the baked curves as a 1024x2 `rgba32float` texture.
    ///
    /// `BakedCurves.data` stores channels in separate 1024-blocks
    /// (row 0 = R,G,B,master; row 1 = luma,hue_vs_sat); they are interleaved
    /// into texels here so the shader can `textureLoad` per texel.
    fn upload_curves(ctx: &RenderContext, baked: &BakedCurves) -> wgpu::TextureView {
        debug_assert_eq!(baked.data.len(), 2 * 4 * CURVE_SIZE as usize);
        let n = CURVE_SIZE as usize;
        let mut texels = vec![0.0f32; 2 * 4 * n];
        for i in 0..n {
            texels[i * 4] = baked.data[i];
            texels[i * 4 + 1] = baked.data[n + i];
            texels[i * 4 + 2] = baked.data[2 * n + i];
            texels[i * 4 + 3] = baked.data[3 * n + i];
            texels[4 * n + i * 4] = baked.data[4 * n + i];
            texels[4 * n + i * 4 + 1] = baked.data[5 * n + i];
        }
        let tex = ctx.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-render curves"),
            size: wgpu::Extent3d {
                width: CURVE_SIZE,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue().write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&texels),
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(CURVE_SIZE * 16),
                rows_per_image: Some(2),
            },
            wgpu::Extent3d {
                width: CURVE_SIZE,
                height: 2,
                depth_or_array_layers: 1,
            },
        );
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// Uploads a normalized 33^3 [`CubeFile`] as an `rgba16float` 3D texture.
    ///
    /// Rows are 33 texels * 8 bytes = 264 bytes, padded to the 256-aligned
    /// 512 bytes required by `write_texture` for non-multiple row pitches.
    fn upload_lut(ctx: &RenderContext, cube: &CubeFile) -> wgpu::TextureView {
        debug_assert_eq!(cube.size, LUT_SIZE as usize);
        let n = LUT_SIZE as usize;
        let padded_row: usize = 512;
        let mut bytes = vec![0u8; n * n * padded_row];
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let e = cube.data[r + n * (g + n * b)];
                    let off = b * n * padded_row + g * padded_row + r * 8;
                    for c in 0..4 {
                        let h = f32_to_f16(e[c]);
                        bytes[off + c * 2..off + c * 2 + 2].copy_from_slice(&h.to_le_bytes());
                    }
                }
            }
        }
        let tex = ctx.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-render lut 3d"),
            size: wgpu::Extent3d {
                width: LUT_SIZE,
                height: LUT_SIZE,
                depth_or_array_layers: LUT_SIZE,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
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
                bytes_per_row: Some(padded_row as u32),
                rows_per_image: Some(LUT_SIZE),
            },
            wgpu::Extent3d {
                width: LUT_SIZE,
                height: LUT_SIZE,
                depth_or_array_layers: LUT_SIZE,
            },
        );
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// Records one compute dispatch (16x16 workgroups) and submits it.
    fn dispatch(
        ctx: &RenderContext,
        pipeline: &wgpu::ComputePipeline,
        bg: &wgpu::BindGroup,
        width: u32,
        height: u32,
    ) {
        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sf-render pass"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sf-render compute"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bg, &[]);
            pass.dispatch_workgroups(width.div_ceil(16), height.div_ceil(16), 1);
        }
        ctx.queue().submit(Some(encoder.finish()));
    }
}
