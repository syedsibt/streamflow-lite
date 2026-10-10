//! `sf-filterfx` — GPU effect filters for StreamFlow Lite (phase C5).
//!
//! [`EffectPass`] applies one effect slot at a time as a WGSL compute
//! dispatch. Source and destination are `rgba16float` linear working-space
//! textures of the same size (different textures).
//!
//! # Math contract
//!
//! Every effect has a CPU reference in [`cpu`] implementing the exact same
//! math as the WGSL shader (`src/shaders/*.wgsl`); the integration tests in
//! `tests/effects_gpu.rs` run the real shaders on lavapipe and compare
//! against the CPU reference. Uniform packing follows [`make_uniform`] —
//! see its docs for the field layout.
//!
//! # Adding a filter (workers 2/3)
//!
//! 1. Add `src/shaders/<name>.wgsl`: 16x16 workgroup, `textureLoad` only (no
//!    samplers), explicit luma expansion instead of `dot()` (see the C4
//!    decision in `BUILD_LOG.md`), and a math-contract doc comment at the
//!    top like `zebra.wgsl` in `sf-scopes`.
//! 2. Register it in [`EffectPass::new`] with
//!    `this.register(ctx, EffectKind::<Name>, include_str!("shaders/<name>.wgsl"), temporal)`.
//! 3. If the filter needs special handling (temporal input, scratch
//!    buffer), add an arm in [`EffectPass::apply_slot`]; the multi-pass
//!    scratch texture and the temporal bind-group layout already exist for
//!    this. Multi-pass dispatches must each bump `passes` so
//!    [`passes_executed`](EffectPass::passes_executed) stays honest.
//! 4. Add a CPU reference in [`cpu`] plus a GPU-vs-CPU test in
//!    `tests/effects_gpu.rs`, reusing [`test_utils`].

use std::cell::Cell;
use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use sf_filters::{EffectKind, EffectParams, FlipAxis, MaskRef};
use sf_render::{RenderContext, RenderError};
use wgpu::util::DeviceExt;

pub mod cpu;
pub mod test_utils;

/// Shared 80-byte effect uniform, packed from [`EffectParams`].
///
/// Five `vec4f`s (16-byte aligned on both the Rust and WGSL sides):
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct FxUniform {
    /// `amount_u` (= `amount * intensity`, clamped 0..1), `radius`,
    /// `softness`, `threshold`.
    pub u0: [f32; 4],
    /// `center_x`, `center_y`, `zoom`, `aspect`.
    pub u1: [f32; 4],
    /// Linear key color `key_r/g/b`, `spill`.
    pub u2: [f32; 4],
    /// `k1`, `k2`, `tolerance`, `key_softness`.
    pub u3: [f32; 4],
    /// `seed` as f32, `axis_code` (0 = Horizontal, 1 = Vertical, 2 = Both),
    /// 0, 0.
    pub u4: [f32; 4],
}

/// Compile-time size check: the uniform must stay exactly 80 bytes.
const _: () = assert!(std::mem::size_of::<FxUniform>() == 80);

/// 32-byte analytic-mask uniform for `gen_region_mask.wgsl`:
/// `p0 = [kind, x, y, radius_or_w]`, `p1 = [h, feather, 0, 0]`;
/// kind 1 = Circle, 2 = Rectangle.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct MaskUniform {
    pub p0: [f32; 4],
    pub p1: [f32; 4],
}

/// Compile-time size check: the mask uniform must stay exactly 32 bytes.
const _: () = assert!(std::mem::size_of::<MaskUniform>() == 32);

/// Packs an [`FxUniform`] from effect params, slot intensity, and frame seed.
///
/// `amount_u = (params.amount * intensity).clamp(0, 1)` — the premultiplied
/// strength both the shaders and the CPU references call `u`.
pub fn make_uniform(params: &EffectParams, intensity: f32, seed: u32) -> FxUniform {
    let amount_u = (params.amount * intensity).clamp(0.0, 1.0);
    let axis_code = match params.axis {
        FlipAxis::Horizontal => 0.0,
        FlipAxis::Vertical => 1.0,
        FlipAxis::Both => 2.0,
    };
    FxUniform {
        u0: [amount_u, params.radius, params.softness, params.threshold],
        u1: [
            params.center[0],
            params.center[1],
            params.zoom,
            params.aspect,
        ],
        u2: [
            params.color[0],
            params.color[1],
            params.color[2],
            params.spill,
        ],
        u3: [params.k1, params.k2, params.tolerance, params.key_softness],
        u4: [seed as f32, axis_code, 0.0, 0.0],
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
        format: wgpu::TextureFormat::Rgba16Float,
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

/// One compute pipeline per implemented effect kind.
///
/// Created once per [`RenderContext`]; all per-frame inputs (source,
/// destination, params, intensity, seed) go through
/// [`apply_slot`](EffectPass::apply_slot).
pub struct EffectPass {
    /// Compiled pipelines, keyed by kind. Kinds registered by later C5 work
    /// are simply absent until their shader is added (see the module docs).
    pipelines: HashMap<EffectKind, wgpu::ComputePipeline>,
    /// `[src sampled2d, dst storage rgba16float write, uniform]`.
    standard_bgl: wgpu::BindGroupLayout,
    /// `[src sampled2d, prev sampled2d, dst storage rgba16float write,
    /// uniform]` — for temporal effects (noise reduction).
    temporal_bgl: wgpu::BindGroupLayout,
    /// `[a sampled2d, b sampled2d, mask sampled2d, dst storage rgba16float
    /// write, uniform]` — for masked blends (C7: masked mix, background
    /// replacement).
    masked_bgl: wgpu::BindGroupLayout,
    /// `[dst storage rgba16float write, mask uniform]` — for analytic
    /// region-mask generation (C7).
    gen_mask_bgl: wgpu::BindGroupLayout,
    /// Scratch `rgba16float` texture for multi-pass effects (e.g. blur and
    /// bloom run H then V); recreated when the frame size changes. Unused by
    /// the group-A filters; provided for workers 2/3.
    #[allow(dead_code)]
    scratch: Option<wgpu::Texture>,
    /// Shared masked-blend pipeline (`masked_mix.wgsl`), used by
    /// [`masked_mix`](EffectPass::masked_mix) and the C7 AI-background
    /// methods.
    masked_mix_pipeline: wgpu::ComputePipeline,
    /// Background-replacement pipeline (`bg_replace.wgsl`).
    bg_replace_pipeline: wgpu::ComputePipeline,
    /// Analytic region-mask pipeline (`gen_region_mask.wgsl`).
    gen_mask_pipeline: wgpu::ComputePipeline,
    /// Total compute dispatches executed (every dispatch of every filter,
    /// including multi-pass ones).
    passes: Cell<u64>,
    /// Reused 80-byte effect uniform buffer (C8): allocated once, rewritten
    /// with `queue.write_buffer` only when the uniform bytes change, instead
    /// of a `create_buffer_init` per dispatch. The shader always sees
    /// exactly the cached bytes, so pixel output is unchanged.
    uniform_cache: Option<(FxUniform, wgpu::Buffer)>,
}

impl EffectPass {
    /// Creates the effect pass: bind-group layouts plus one compute pipeline
    /// per implemented filter (group A: vignette, film grain, letterbox,
    /// mirror flip; group C: chroma key, noise reduction). Workers 2/3 add
    /// their shaders by registering them here. C7 adds the masked-blend
    /// layout/pipelines (`masked_mix`, `bg_replace`) and the analytic
    /// region-mask pipeline.
    pub fn new(ctx: &RenderContext) -> Result<Self, RenderError> {
        let device = ctx.device();
        let standard_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-filterfx standard bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, storage_2d_write()),
                bgl_entry(2, uniform()),
            ],
        });
        let temporal_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-filterfx temporal bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, sampled_2d()),
                bgl_entry(2, storage_2d_write()),
                bgl_entry(3, uniform()),
            ],
        });
        let masked_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-filterfx masked bgl"),
            entries: &[
                bgl_entry(0, sampled_2d()),
                bgl_entry(1, sampled_2d()),
                bgl_entry(2, sampled_2d()),
                bgl_entry(3, storage_2d_write()),
                bgl_entry(4, uniform()),
            ],
        });
        let gen_mask_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sf-filterfx gen mask bgl"),
            entries: &[bgl_entry(0, storage_2d_write()), bgl_entry(1, uniform())],
        });
        // C7 pipelines, compiled against the layouts above before the struct
        // is assembled (compile only needs the context, a label, the WGSL
        // source, and the layout).
        let masked_mix_pipeline = Self::compile(
            ctx,
            "sf-filterfx masked_mix",
            include_str!("shaders/masked_mix.wgsl"),
            &masked_bgl,
        );
        let bg_replace_pipeline = Self::compile(
            ctx,
            "sf-filterfx bg_replace",
            include_str!("shaders/bg_replace.wgsl"),
            &masked_bgl,
        );
        let gen_mask_pipeline = Self::compile(
            ctx,
            "sf-filterfx gen_region_mask",
            include_str!("shaders/gen_region_mask.wgsl"),
            &gen_mask_bgl,
        );
        let mut this = Self {
            pipelines: HashMap::new(),
            standard_bgl,
            temporal_bgl,
            masked_bgl,
            gen_mask_bgl,
            scratch: None,
            masked_mix_pipeline,
            bg_replace_pipeline,
            gen_mask_pipeline,
            passes: Cell::new(0),
            uniform_cache: None,
        };
        // Group A (worker 1). New filter = new .wgsl + one register() call.
        this.register(
            ctx,
            EffectKind::Vignette,
            include_str!("shaders/vignette.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::FilmGrain,
            include_str!("shaders/film_grain.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::Letterbox,
            include_str!("shaders/letterbox.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::MirrorFlip,
            include_str!("shaders/mirror_flip.wgsl"),
            false,
        );
        // Group C (worker 3): chroma key (standard layout) and temporal
        // noise reduction (temporal layout: src + prev + dst + uniform).
        this.register(
            ctx,
            EffectKind::ChromaKey,
            include_str!("shaders/chroma_key.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::NoiseReduction,
            include_str!("shaders/noise_reduction.wgsl"),
            true,
        );
        // Group B (worker 2). GaussianBlur and Bloom are two-pass effects
        // (H then V over the temporal bind layout); they register with
        // `temporal = true` and `apply_slot` dispatches them twice, setting
        // the u4.z pass selector per dispatch (0 = horizontal pass,
        // 1 = vertical/composite pass; `make_uniform` leaves it 0).
        this.register(
            ctx,
            EffectKind::Sharpen,
            include_str!("shaders/sharpen.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::ChromaticAberration,
            include_str!("shaders/chromatic_aberration.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::Pixelate,
            include_str!("shaders/pixelate.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::GaussianBlur,
            include_str!("shaders/gaussian_blur.wgsl"),
            true,
        );
        this.register(
            ctx,
            EffectKind::Bloom,
            include_str!("shaders/bloom.wgsl"),
            true,
        );
        this.register(
            ctx,
            EffectKind::LensCorrection,
            include_str!("shaders/lens_correction.wgsl"),
            false,
        );
        this.register(
            ctx,
            EffectKind::CropZoomPan,
            include_str!("shaders/crop_zoom_pan.wgsl"),
            false,
        );
        Ok(this)
    }

    /// Compiles one WGSL compute shader against `bgl` and returns the
    /// pipeline. Used by [`register`](EffectPass::register) and by [`new`](EffectPass::new)
    /// for the C7 masked/gen-mask pipelines.
    fn compile(
        ctx: &RenderContext,
        label: &str,
        wgsl: &str,
        bgl: &wgpu::BindGroupLayout,
    ) -> wgpu::ComputePipeline {
        let device = ctx.device();
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

    /// Compiles one effect shader and stores its pipeline. `temporal`
    /// selects the 4-binding layout (src + prev + dst + uniform) instead of
    /// the standard 3-binding one.
    fn register(&mut self, ctx: &RenderContext, kind: EffectKind, wgsl: &str, temporal: bool) {
        let label = format!("sf-filterfx {kind:?}");
        let bgl = if temporal {
            &self.temporal_bgl
        } else {
            &self.standard_bgl
        };
        let pipeline = Self::compile(ctx, &label, wgsl, bgl);
        self.pipelines.insert(kind, pipeline);
    }

    /// Returns the pipeline for `kind`, or `None` when the kind has no
    /// registered shader yet (its C5 worker has not landed).
    fn pipeline_for(&self, kind: &EffectKind) -> Option<&wgpu::ComputePipeline> {
        self.pipelines.get(kind)
    }

    /// Applies one effect slot. `src`/`dst` are `rgba16float` LINEAR textures
    /// of the same size (different textures).
    ///
    /// Returns `Ok(false)` with **zero** GPU work when
    /// `params.amount * intensity <= 0.0`, when `kind.is_ai()` (AI effects
    /// are C6/C7), or when the kind has no registered shader yet. Returns
    /// `Ok(true)` after running the effect's dispatch(es).
    ///
    /// `prev`: `Some` only for [`EffectKind::NoiseReduction`] (same
    /// format/size as `src`); `seed`: the frame seed for
    /// [`EffectKind::FilmGrain`].
    #[allow(clippy::too_many_arguments)]
    /// Ensures the shared 80-byte effect uniform buffer holds `uniform`'s
    /// bytes (C8 optimization).
    ///
    /// The buffer is allocated once (with `COPY_DST`) and rewritten via
    /// `queue.write_buffer` only when the bytes differ from the cached ones
    /// — effect params change rarely, and only the grain seed changes per
    /// frame — instead of a `create_buffer_init` allocation per dispatch.
    /// The shader always sees exactly these bytes, so pixel output is
    /// unchanged.
    fn upload_uniform(
        cache: &mut Option<(FxUniform, wgpu::Buffer)>,
        ctx: &RenderContext,
        uniform: &FxUniform,
    ) {
        let bytes = bytemuck::bytes_of(uniform);
        if let Some((cached, buf)) = cache.as_mut() {
            if bytemuck::bytes_of(cached) == bytes {
                return;
            }
            ctx.queue().write_buffer(buf, 0, bytes);
            *cached = *uniform;
            return;
        }
        let buf = ctx.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("sf-filterfx uniform"),
            size: std::mem::size_of::<FxUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue().write_buffer(&buf, 0, bytes);
        *cache = Some((*uniform, buf));
    }

    /// Borrowed view of the shared uniform buffer after
    /// [`upload_uniform`](Self::upload_uniform) has run.
    fn uniform_buf(&self) -> &wgpu::Buffer {
        &self
            .uniform_cache
            .as_ref()
            .expect("upload_uniform must run before uniform_buf")
            .1
    }

    pub fn apply_slot(
        &mut self,
        ctx: &RenderContext,
        kind: &EffectKind,
        params: &EffectParams,
        intensity: f32,
        src: &wgpu::Texture,
        dst: &wgpu::Texture,
        prev: Option<&wgpu::Texture>,
        seed: u32,
    ) -> Result<bool, RenderError> {
        // AI kinds and zero-strength slots do no GPU work at all. (The
        // `!(u > 0.0)` form also bypasses NaN amounts instead of panicking
        // in `clamp`.)
        if kind.is_ai() {
            return Ok(false);
        }
        let amount_u = (params.amount * intensity).clamp(0.0, 1.0);
        if !(amount_u > 0.0) {
            return Ok(false);
        }
        // Two-pass effects (group B): H pass into the scratch texture, then
        // the V/composite pass into dst. Each dispatch bumps `passes`.
        if matches!(kind, EffectKind::GaussianBlur | EffectKind::Bloom) {
            if self.pipeline_for(kind).is_none() {
                // Registered by a later C5 worker; nothing to run yet.
                return Ok(false);
            }
            return self.apply_two_pass(ctx, kind, params, intensity, seed, src, dst);
        }
        // Upload the uniform before the pipeline lookup below: the lookup
        // borrows `self` immutably, and the upload needs `&mut`.
        let uniform = make_uniform(params, intensity, seed);
        Self::upload_uniform(&mut self.uniform_cache, ctx, &uniform);
        let Some(pipeline) = self.pipeline_for(kind) else {
            // Registered by a later C5 worker; nothing to run yet.
            return Ok(false);
        };
        let temporal = *kind == EffectKind::NoiseReduction;

        let uniform_buf = self.uniform_buf();
        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = if temporal {
            let prev = prev.ok_or(RenderError::SizeMismatch)?;
            let prev_view = prev.create_view(&wgpu::TextureViewDescriptor::default());
            ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sf-filterfx temporal bg"),
                layout: &self.temporal_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&src_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&prev_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&dst_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: uniform_buf.as_entire_binding(),
                    },
                ],
            })
        } else {
            ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sf-filterfx standard bg"),
                layout: &self.standard_bgl,
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
                        resource: uniform_buf.as_entire_binding(),
                    },
                ],
            })
        };

        let size = src.size();
        Self::dispatch(ctx, pipeline, &bg, size.width, size.height);
        self.passes.set(self.passes.get() + 1);
        Ok(true)
    }

    /// Runs a two-pass effect ([`EffectKind::GaussianBlur`] or
    /// [`EffectKind::Bloom`]) over the temporal bind layout:
    ///
    /// - pass 0 (`u4.z = 0`): tex0 = `src`, tex1 = `src` (unused dummy),
    ///   dst = scratch — the horizontal blur;
    /// - pass 1 (`u4.z = 1`): tex0 = scratch, tex1 = `src`, dst = `dst` —
    ///   the vertical blur plus the per-filter composite.
    ///
    /// The shader branches on the `u4.z` pass selector, which is set here per
    /// dispatch ([`make_uniform`] leaves it 0). Each dispatch bumps
    /// [`passes_executed`](EffectPass::passes_executed).
    fn apply_two_pass(
        &mut self,
        ctx: &RenderContext,
        kind: &EffectKind,
        params: &EffectParams,
        intensity: f32,
        seed: u32,
        src: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<bool, RenderError> {
        let device = ctx.device();
        let size = src.size();
        let (w, h) = (size.width, size.height);
        // Scope the scratch borrow: the view is an owned handle, so the
        // `&mut self` borrow ends here and the pipeline lookup below is
        // allowed.
        let scratch_view = {
            let scratch = self.scratch_for(ctx, w, h);
            scratch.create_view(&wgpu::TextureViewDescriptor::default())
        };
        // The caller checked registration. The pipeline lookup is repeated
        // per pass (a `HashMap::get`) so the `&self` borrow never spans the
        // `&mut` uniform upload below.
        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        for pass in [0.0f32, 1.0f32] {
            let mut uniform = make_uniform(params, intensity, seed);
            uniform.u4[2] = pass;
            Self::upload_uniform(&mut self.uniform_cache, ctx, &uniform);
            let pipeline = self.pipeline_for(kind).expect("pipeline checked by caller");
            let uniform_buf = self.uniform_buf();
            let (t0, t1, t2) = if pass < 0.5 {
                (&src_view, &src_view, &scratch_view)
            } else {
                (&scratch_view, &src_view, &dst_view)
            };
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sf-filterfx two-pass bg"),
                layout: &self.temporal_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(t0),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(t1),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(t2),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: uniform_buf.as_entire_binding(),
                    },
                ],
            });
            Self::dispatch(ctx, pipeline, &bg, w, h);
            self.passes.set(self.passes.get() + 1);
        }
        Ok(true)
    }

    /// Total compute dispatches executed (for bypass tests). Multi-pass
    /// filters count each dispatch.
    pub fn passes_executed(&self) -> u64 {
        self.passes.get()
    }

    /// AI background blur (C7): blurs `sharp` with a two-pass gaussian
    /// (`params.radius`, full strength, reusing the `gaussian_blur` shader
    /// over the temporal layout into a temp target), then
    /// [`masked_mix`](EffectPass::masked_mix) with `a` = blurred,
    /// `b` = `sharp`, `mask` = `person_mask`. 3 dispatches total.
    /// Returns `Ok(false)` with zero GPU work when
    /// `params.amount * intensity <= 0.0`.
    pub fn apply_background_blur(
        &mut self,
        ctx: &RenderContext,
        params: &EffectParams,
        intensity: f32,
        sharp: &wgpu::Texture,
        person_mask: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<bool, RenderError> {
        let amount_u = (params.amount * intensity).clamp(0.0, 1.0);
        if !(amount_u > 0.0) {
            return Ok(false);
        }
        // Full-strength blur; the amount gate lives in the final mix.
        let mut blur_params = params.clone();
        blur_params.amount = 1.0;
        let size = sharp.size();
        let blurred = ctx.create_linear_target(size.width, size.height);
        self.apply_two_pass(
            ctx,
            &EffectKind::GaussianBlur,
            &blur_params,
            1.0,
            0,
            sharp,
            &blurred,
        )?;
        self.masked_mix(ctx, amount_u, &blurred, sharp, person_mask, dst)?;
        Ok(true)
    }

    /// AI background replacement (C7): single `bg_replace` dispatch —
    /// `dst = mix(plate, src, mask.r)` with spill suppression near mask
    /// edges (`spill * amount_u * edge_factor`). The plate texture is a 1x1
    /// solid color (params.color) or a real image (tiled). Returns
    /// `Ok(false)` with zero GPU work when
    /// `params.amount * intensity <= 0.0`.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_background_replace(
        &mut self,
        ctx: &RenderContext,
        params: &EffectParams,
        intensity: f32,
        src: &wgpu::Texture,
        person_mask: &wgpu::Texture,
        plate: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<bool, RenderError> {
        let amount_u = (params.amount * intensity).clamp(0.0, 1.0);
        if !(amount_u > 0.0) {
            return Ok(false);
        }
        let uniform = make_uniform(params, intensity, 0);
        let size = src.size();
        let bg = self.masked_bind_group(ctx, &uniform, plate, src, person_mask, dst);
        self.dispatch_counted(ctx, &self.bg_replace_pipeline, &bg, size.width, size.height);
        Ok(true)
    }

    /// Skin smoothing (C7): subtle two-pass gaussian
    /// (`params.radius` when > 0, else 2.0) at full strength, then
    /// [`masked_mix`](EffectPass::masked_mix) with `a` = `src`,
    /// `b` = blurred, `mask` = `skin_mask` (the mask selects the blurred
    /// pixels: `dst = mix(a, b, m)`). 3 dispatches total.
    /// Returns `Ok(false)` with zero GPU work when
    /// `params.amount * intensity <= 0.0`.
    ///
    /// NOTE: the C7 brief's parenthetical read `a=blurred, b=src`, which
    /// would leave the skin (mask = 1) un-smoothed under the
    /// `mix(a, b, m)` convention and contradict the required behavior
    /// (skin variance reduced, non-skin untouched). The argument order here
    /// implements the specified behavior.
    pub fn apply_skin_smooth(
        &mut self,
        ctx: &RenderContext,
        params: &EffectParams,
        intensity: f32,
        src: &wgpu::Texture,
        skin_mask: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<bool, RenderError> {
        let amount_u = (params.amount * intensity).clamp(0.0, 1.0);
        if !(amount_u > 0.0) {
            return Ok(false);
        }
        let mut blur_params = params.clone();
        blur_params.amount = 1.0;
        blur_params.radius = if params.radius > 0.0 {
            params.radius
        } else {
            2.0
        };
        let size = src.size();
        let blurred = ctx.create_linear_target(size.width, size.height);
        self.apply_two_pass(
            ctx,
            &EffectKind::GaussianBlur,
            &blur_params,
            1.0,
            0,
            src,
            &blurred,
        )?;
        self.masked_mix(ctx, amount_u, src, &blurred, skin_mask, dst)?;
        Ok(true)
    }

    /// Single masked-blend dispatch: `dst = mix(a, b, mask.r * amount)`.
    /// `amount` is clamped to 0..1 (callers pass the premultiplied amount).
    /// Bumps [`passes_executed`](EffectPass::passes_executed).
    pub fn masked_mix(
        &mut self,
        ctx: &RenderContext,
        amount: f32,
        a: &wgpu::Texture,
        b: &wgpu::Texture,
        mask: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> Result<(), RenderError> {
        let uniform = FxUniform {
            u0: [amount.clamp(0.0, 1.0), 0.0, 0.0, 0.0],
            u1: [0.0; 4],
            u2: [0.0; 4],
            u3: [0.0; 4],
            u4: [0.0; 4],
        };
        let size = a.size();
        let bg = self.masked_bind_group(ctx, &uniform, a, b, mask, dst);
        self.dispatch_counted(ctx, &self.masked_mix_pipeline, &bg, size.width, size.height);
        Ok(())
    }

    /// Renders the analytic region mask ([`MaskRef`]) into `dst`
    /// (`.r` = weight 0..1, g/b = 0, a = 1). One dispatch.
    ///
    /// [`MaskRef::Person`] is NOT generated here — the engine supplies that
    /// texture from `sf-ai-runtime` — so it returns
    /// [`RenderError::SizeMismatch`] (there is no dedicated error variant in
    /// `sf-render` for this case).
    pub fn gen_region_mask(
        &self,
        ctx: &RenderContext,
        mask: &MaskRef,
        dst: &wgpu::Texture,
    ) -> Result<(), RenderError> {
        match mask {
            MaskRef::Circle {
                x,
                y,
                radius,
                feather,
            } => self.gen_region_mask_shape(ctx, 1, [*x, *y, *radius, 0.0], *feather, dst),
            MaskRef::Rectangle {
                x,
                y,
                w,
                h,
                feather,
            } => self.gen_region_mask_shape(ctx, 2, [*x, *y, *w, *h], *feather, dst),
            MaskRef::Person => Err(RenderError::SizeMismatch),
        }
    }

    /// Lower-level analytic mask render: `kind` 1 = Circle{x, y, radius},
    /// 2 = Rectangle{x, y, w, h} (`geo = [x, y, radius_or_w, h]`,
    /// normalized 0..1, origin top-left); `feather` in normalized units.
    /// Weight is 1 inside with a smoothstep falloff over `feather` at the
    /// edge. One dispatch; bumps
    /// [`passes_executed`](EffectPass::passes_executed).
    ///
    /// This is the lower-level path behind [`gen_region_mask`](EffectPass::gen_region_mask);
    /// the engine can also call it directly for procedurally-built shapes.
    pub fn gen_region_mask_shape(
        &self,
        ctx: &RenderContext,
        kind: u32,
        geo: [f32; 4],
        feather: f32,
        dst: &wgpu::Texture,
    ) -> Result<(), RenderError> {
        let uniform = MaskUniform {
            p0: [kind as f32, geo[0], geo[1], geo[2]],
            p1: [geo[3], feather, 0.0, 0.0],
        };
        let device = ctx.device();
        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("sf-filterfx mask uniform"),
            contents: bytemuck::bytes_of(&uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-filterfx gen mask bg"),
            layout: &self.gen_mask_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform_buf.as_entire_binding(),
                },
            ],
        });
        let size = dst.size();
        self.dispatch_counted(ctx, &self.gen_mask_pipeline, &bg, size.width, size.height);
        Ok(())
    }

    /// Builds a `masked_bgl` bind group: `a`@0, `b`@1, `mask`@2, `dst`@3,
    /// `uniform`@4.
    fn masked_bind_group(
        &self,
        ctx: &RenderContext,
        uniform: &FxUniform,
        a: &wgpu::Texture,
        b: &wgpu::Texture,
        mask: &wgpu::Texture,
        dst: &wgpu::Texture,
    ) -> wgpu::BindGroup {
        let device = ctx.device();
        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("sf-filterfx masked uniform"),
            contents: bytemuck::bytes_of(uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let a_view = a.create_view(&wgpu::TextureViewDescriptor::default());
        let b_view = b.create_view(&wgpu::TextureViewDescriptor::default());
        let mask_view = mask.create_view(&wgpu::TextureViewDescriptor::default());
        let dst_view = dst.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sf-filterfx masked bg"),
            layout: &self.masked_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&a_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&b_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&dst_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: uniform_buf.as_entire_binding(),
                },
            ],
        })
    }

    /// Records one compute dispatch (16x16 workgroups) and bumps the pass
    /// counter so [`passes_executed`](EffectPass::passes_executed) stays
    /// honest.
    fn dispatch_counted(
        &self,
        ctx: &RenderContext,
        pipeline: &wgpu::ComputePipeline,
        bg: &wgpu::BindGroup,
        width: u32,
        height: u32,
    ) {
        Self::dispatch(ctx, pipeline, bg, width, height);
        self.passes.set(self.passes.get() + 1);
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
                label: Some("sf-filterfx pass"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sf-filterfx compute"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bg, &[]);
            pass.dispatch_workgroups(width.div_ceil(16), height.div_ceil(16), 1);
        }
        ctx.queue().submit(Some(encoder.finish()));
    }
}

/// Scratch `rgba16float` texture for multi-pass effects, recreated when the
/// frame size changes. Provided for workers 2/3 (blur/bloom H-then-V passes).
#[allow(dead_code)]
impl EffectPass {
    pub(crate) fn scratch_for(&mut self, ctx: &RenderContext, w: u32, h: u32) -> &wgpu::Texture {
        let stale = self
            .scratch
            .as_ref()
            .map_or(true, |t| t.size().width != w || t.size().height != h);
        if stale {
            self.scratch = Some(ctx.create_linear_target(w, h));
        }
        self.scratch.as_ref().expect("scratch just created")
    }
}
