//! `sf-engine` — minimal orchestrator for StreamFlow Lite.
//!
//! [`Engine`] ties the pieces together: it owns a headless [`RenderContext`],
//! [`GradePass`] and [`EffectPass`], the live [`State`], and a
//! [`LookRegistry`], and processes one input frame at a time. The pipeline is
//! upload -> effective grade (base grade + look-filter stack folded in) ->
//! optional unsharp detail -> effect slots in stack order -> output encode.
//! Identity grades with no active effects bypass the GPU entirely and return
//! the input pixels unchanged.
//!
//! Scopes are managed by [`ScopeHub`]: the UI subscribes per input and scope
//! kind (mirroring the `scopes.subscribe` / `scopes.unsubscribe` IPC
//! commands), and the hub taps the graded frame, runs the GPU scope passes
//! at most at 15 Hz, and queues [`ScopeFrame`]s for the UI to drain. With
//! zero subscribers no GPU scope work happens at all.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sf_ai_runtime::{InferenceSession, StubSession};
use sf_filterfx::EffectPass;
use sf_filters::{resolve_grade, EffectKind, EffectParams, Filter, LookRegistry, MaskRef};
use sf_render::lut::CubeFile;
use sf_render::{GradePass, RenderContext, RenderError};
use sf_scopes::{ScopeFrame, ScopeKind, ScopeParams, ScopePass, SCOPE_INTERVAL_MICROS};
use sf_state::State;

mod ai_masks;

use ai_masks::{MaskCacheEntry, MaskKind, MASK_REFRESH_FRAMES};

/// Minimum interval between two scope frames of one subscription (15 Hz).
const SCOPE_INTERVAL: Duration = Duration::from_micros(SCOPE_INTERVAL_MICROS);

/// Errors from the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The render layer failed.
    #[error("render: {0}")]
    Render(#[from] RenderError),
    /// No input with this id exists in the state.
    #[error("unknown input: {0}")]
    UnknownInput(String),
    /// A `.cube` LUT file could not be loaded or parsed.
    #[error("lut load failed: {0}")]
    LutLoad(String),
    /// Filesystem I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// One display-encoded RGBA8 frame.
pub struct Frame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row-major RGBA8 pixels, `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

/// Per-subscription throttle state.
struct Subscription {
    /// Last emission time; `None` means "emit the next frame immediately".
    last_emit: Option<Instant>,
    /// Monotonic sequence number for `scopes.frame` events.
    seq: u64,
}

/// Clock source for [`ScopeHub`]'s 15 Hz throttle.
///
/// Production always uses the wall clock. Tests can install a manual clock
/// via [`ScopeHub::set_manual_clock`] / [`ScopeHub::advance_manual_clock`]
/// to make throttle-boundary tests deterministic: with the wall clock, five
/// back-to-back frames on a slow software GL device can span the 66.7 ms
/// throttle boundary and emit twice (~1-in-5 flake on cold lavapipe), which
/// no amount of `sleep` tuning fixes — only controlling the clock does.
#[derive(Clone, Debug, Default)]
struct ScopeClock {
    /// When `Some`, this instant is used instead of `Instant::now()`.
    /// Test-only; production code never sets it.
    manual_now: Option<Instant>,
}

impl ScopeClock {
    fn now(&self) -> Instant {
        self.manual_now.unwrap_or_else(Instant::now)
    }
}

/// Scope subscription manager: 15 Hz throttle + frame outbox.
///
/// The hub never touches the GPU itself; [`Engine`] passes it the graded
/// frame texture and the [`ScopePass`], and the hub decides per subscription
/// whether a scope is due, runs it, and queues the resulting [`ScopeFrame`].
pub struct ScopeHub {
    subs: HashMap<(String, ScopeKind), Subscription>,
    outbox: Vec<ScopeFrame>,
    params: ScopeParams,
    /// Number of GPU scope analyses actually executed (for tests and
    /// telemetry — proves no scope work happens with zero subscribers).
    gpu_invocations: u64,
    /// Throttle clock: wall clock in production, manual in tests.
    clock: ScopeClock,
}

impl ScopeHub {
    /// Empty hub with default scope parameters.
    pub fn new() -> Self {
        Self {
            subs: HashMap::new(),
            outbox: Vec::new(),
            params: ScopeParams::default(),
            gpu_invocations: 0,
            clock: ScopeClock::default(),
        }
    }

    /// Scope parameters (zebra thresholds).
    pub fn params(&self) -> &ScopeParams {
        &self.params
    }

    /// Mutable scope parameters.
    pub fn params_mut(&mut self) -> &mut ScopeParams {
        &mut self.params
    }

    /// Number of active subscriptions.
    pub fn subscription_count(&self) -> usize {
        self.subs.len()
    }

    /// Number of GPU scope analyses executed so far.
    pub fn gpu_invocations(&self) -> u64 {
        self.gpu_invocations
    }

    /// Subscribes an input to a scope kind (idempotent).
    pub fn subscribe(&mut self, input_id: &str, kind: ScopeKind) {
        self.subs
            .entry((input_id.to_string(), kind))
            .or_insert(Subscription {
                last_emit: None,
                seq: 0,
            });
    }

    /// Removes a subscription; returns whether one existed.
    pub fn unsubscribe(&mut self, input_id: &str, kind: ScopeKind) -> bool {
        self.subs.remove(&(input_id.to_string(), kind)).is_some()
    }

    /// Drains queued scope frames (the `scopes.frame` event payloads).
    pub fn drain_frames(&mut self) -> Vec<ScopeFrame> {
        std::mem::take(&mut self.outbox)
    }

    /// Installs a manual clock for the 15 Hz throttle (test helper).
    ///
    /// From this point the hub measures throttle intervals against `now`
    /// instead of the wall clock, until [`advance_manual_clock`][Self::advance_manual_clock]
    /// moves it. Production code never calls this.
    pub fn set_manual_clock(&mut self, now: Instant) {
        self.clock.manual_now = Some(now);
    }

    /// Advances the manual clock installed by
    /// [`set_manual_clock`][Self::set_manual_clock] (test helper).
    ///
    /// Panics if no manual clock is installed — a test that advances time
    /// must pin the clock first, otherwise the throttle would mix wall and
    /// manual time.
    pub fn advance_manual_clock(&mut self, dt: Duration) {
        let cur = self
            .clock
            .manual_now
            .expect("advance_manual_clock without set_manual_clock");
        self.clock.manual_now = Some(cur + dt);
    }

    /// Taps a graded frame: for every subscription of `input_id` whose
    /// 15 Hz budget has elapsed, runs the GPU scope and queues a frame.
    ///
    /// Returns immediately — without any GPU work — when the input has no
    /// subscriptions. The 256x144 downscale runs once even when several
    /// kinds are due on the same frame.
    pub fn maybe_emit(
        &mut self,
        ctx: &RenderContext,
        pass: &ScopePass,
        input_id: &str,
        graded_frame: &wgpu::Texture,
    ) -> Result<(), RenderError> {
        // The throttle clock: wall clock in production, the test-installed
        // manual clock when one is set. Sampling it once per call keeps the
        // budget check consistent across all subscriptions of this frame.
        let now = self.clock.now();
        let due: Vec<ScopeKind> = self
            .subs
            .iter()
            .filter(|((id, _), sub)| {
                id == input_id
                    && sub
                        .last_emit
                        .map_or(true, |t| now.duration_since(t) >= SCOPE_INTERVAL)
            })
            .map(|((_, kind), _)| *kind)
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        let small = pass.downscale(ctx, graded_frame)?;
        for kind in due {
            let payload = pass.analyze(ctx, kind, &small, &self.params)?;
            self.gpu_invocations += 1;
            let sub = self
                .subs
                .get_mut(&(input_id.to_string(), kind))
                .expect("subscription vanished mid-emit");
            sub.last_emit = Some(now);
            sub.seq += 1;
            self.outbox.push(ScopeFrame {
                input_id: input_id.to_string(),
                kind,
                seq: sub.seq,
                payload,
            });
        }
        Ok(())
    }
}

impl Default for ScopeHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Reused per-frame working targets (C8 optimization).
///
/// The two linear ping-pong textures and the final RGBA8 output texture
/// persist across frames and are recreated only when the frame size changes,
/// instead of allocating ~40 MB of textures fresh every frame at 1080p.
/// Every pass fully overwrites its target each frame, so reusing them cannot
/// change pixel output. Size-keyed per engine (not per input): alternating
/// frame sizes would recreate them, but that path stays correct.
///
/// Textures are held as `Arc` handles (`wgpu::Texture` itself is not
/// `Clone` in wgpu 22); cloning an `Arc` is a refcount bump, and `&Arc<T>`
/// deref-coerces to `&Texture` at the pass call sites.
struct WorkTargets {
    width: u32,
    height: u32,
    linear: [Arc<wgpu::Texture>; 2],
    out: Arc<wgpu::Texture>,
}

/// The minimal engine: headless wgpu context + grade pass + effect pass +
/// live state.
///
/// LUT files referenced by an input's grade are loaded relative to the
/// current working directory, parsed once, and cached by path.
pub struct Engine {
    ctx: RenderContext,
    pass: GradePass,
    scope_pass: ScopePass,
    fx: EffectPass,
    look_registry: LookRegistry,
    state: State,
    lut_cache: HashMap<String, CubeFile>,
    scopes: ScopeHub,
    /// Previous linear frame per input, feeding temporal noise reduction.
    /// Recreated when the frame size changes.
    nr_prev: HashMap<String, wgpu::Texture>,
    /// Monotonic per-input frame counter; doubles as the effect frame seed.
    frame_seq: HashMap<String, u64>,
    /// Background-replace slots skipped for lack of a model (the spec's
    /// grey-out rule: hue fallback is too unreliable for replacement, so the
    /// slot does zero GPU work and the frame still processes).
    ai_skipped: u64,
    /// AI inference session backing the C7 AI effects (person/skin masks).
    /// Defaults to a model-less stub; swap a real session in with
    /// [`set_ai_session`](Engine::set_ai_session).
    ai_session: Box<dyn InferenceSession>,
    /// Per-input cached AI mask textures, keyed by `(input id, mask kind)`.
    /// Refreshed when the frame size changes or every
    /// [`MASK_REFRESH_FRAMES`] frames — see [`ai_masks`] for the policy.
    mask_cache: HashMap<(String, MaskKind), MaskCacheEntry>,
    /// Number of mask texture (re)uploads so far (for the caching tests —
    /// proves masks are computed once and reused, not per frame).
    mask_uploads: u64,
    /// Reused working targets for [`grade_input_frame`](Engine::grade_input_frame)
    /// (C8): two linear ping-pong textures + the final RGBA8 target,
    /// recreated only when the frame size changes.
    work: Option<WorkTargets>,
}

impl Engine {
    /// Creates an engine with a headless wgpu context and empty state.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Render`] when no GPU adapter or device can be
    /// opened.
    pub fn new_headless() -> Result<Self, EngineError> {
        let ctx = RenderContext::new_headless()?;
        let pass = GradePass::new(&ctx)?;
        let scope_pass = ScopePass::new(&ctx)?;
        let fx = EffectPass::new(&ctx)?;
        Ok(Self {
            ctx,
            pass,
            scope_pass,
            fx,
            look_registry: LookRegistry::default(),
            state: State::new(),
            lut_cache: HashMap::new(),
            scopes: ScopeHub::new(),
            nr_prev: HashMap::new(),
            frame_seq: HashMap::new(),
            ai_skipped: 0,
            ai_session: Box::new(StubSession::new("person-seg")),
            mask_cache: HashMap::new(),
            mask_uploads: 0,
            work: None,
        })
    }

    /// The engine's live project state (inputs, scenes, grades).
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Mutable access to the engine's live project state.
    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    /// The scope subscription hub.
    pub fn scopes(&self) -> &ScopeHub {
        &self.scopes
    }

    /// Mutable access to the scope subscription hub.
    pub fn scopes_mut(&mut self) -> &mut ScopeHub {
        &mut self.scopes
    }

    /// Mutable access to the look registry.
    ///
    /// Empty by default: the future UI layer loads `.sflook` files here
    /// (e.g. from `assets/looks/`), and `grade_input_frame` folds them into
    /// each input's grade via its filter stack.
    pub fn look_registry_mut(&mut self) -> &mut LookRegistry {
        &mut self.look_registry
    }

    /// Total effect compute dispatches executed so far (for bypass tests).
    pub fn effect_passes(&self) -> u64 {
        self.fx.passes_executed()
    }

    /// Background-replace slots skipped so far for lack of a model.
    ///
    /// Only [`EffectKind::BackgroundReplace`] without an available model
    /// counts here (the spec's grey-out rule); background blur and skin
    /// smoothing are functional without a model via the hue fallback, so
    /// they never increment this counter.
    pub fn ai_skipped(&self) -> u64 {
        self.ai_skipped
    }

    /// Number of AI mask texture (re)uploads so far — the caching telemetry.
    pub fn mask_uploads(&self) -> u64 {
        self.mask_uploads
    }

    /// Grade-pass resource cache accounting `(hits, misses)` — see
    /// [`GradePass::cache_stats`](sf_render::GradePass::cache_stats).
    ///
    /// C8 soak telemetry: with an unchanged grade, the first frame records
    /// 3 misses (uniform + curves + LUT/dummy) and every later frame must
    /// record exactly 3 hits — proving per-frame GPU resources are reused,
    /// not rebuilt.
    pub fn grade_cache_stats(&self) -> (u64, u64) {
        self.pass.cache_stats()
    }

    /// Swaps the AI inference session (e.g. a real ONNX backend when one
    /// lands). The mask cache is dropped: masks from the previous session
    /// must not leak into the new one.
    pub fn set_ai_session(&mut self, s: Box<dyn InferenceSession>) {
        self.ai_session = s;
        self.mask_cache.clear();
    }

    /// Whether the current AI session has a usable model.
    pub fn ai_model_available(&self) -> bool {
        self.ai_session.model_available()
    }

    /// Cached person-mask texture for this input frame (`.r` = weight).
    ///
    /// Recomputes (and re-uploads, bumping [`mask_uploads`](Engine::mask_uploads))
    /// when the frame size changed or [`MASK_REFRESH_FRAMES`] frames have
    /// passed since the cached entry; otherwise returns the cached texture.
    fn person_mask_texture(
        &mut self,
        input_id: &str,
        width: u32,
        height: u32,
        pixels: &[u8],
        seq: u64,
    ) -> Result<std::sync::Arc<wgpu::Texture>, EngineError> {
        self.cached_mask_texture(input_id, MaskKind::Person, width, height, pixels, seq)
    }

    /// Cached skin-mask texture for this input frame (`.r` = weight).
    /// Same refresh policy as [`person_mask_texture`](Engine::person_mask_texture).
    fn skin_mask_texture(
        &mut self,
        input_id: &str,
        width: u32,
        height: u32,
        pixels: &[u8],
        seq: u64,
    ) -> Result<std::sync::Arc<wgpu::Texture>, EngineError> {
        self.cached_mask_texture(input_id, MaskKind::Skin, width, height, pixels, seq)
    }

    /// Shared cache lookup behind [`person_mask_texture`](Engine::person_mask_texture)
    /// and [`skin_mask_texture`](Engine::skin_mask_texture). Returns a
    /// reference-counted handle so no borrow of the cache outlives the call
    /// (`&Arc<Texture>` derefs to `&Texture` at the call sites).
    fn cached_mask_texture(
        &mut self,
        input_id: &str,
        kind: MaskKind,
        width: u32,
        height: u32,
        pixels: &[u8],
        seq: u64,
    ) -> Result<std::sync::Arc<wgpu::Texture>, EngineError> {
        let key = (input_id.to_string(), kind);
        let stale = match self.mask_cache.get(&key) {
            // Refresh when the frame size changed or MASK_REFRESH_FRAMES
            // frames have passed since this entry (seq is monotonic, so no
            // underflow; saturating for defense-in-depth).
            Some(e) => {
                e.width != width
                    || e.height != height
                    || seq.saturating_sub(e.frame_seq) >= MASK_REFRESH_FRAMES
            }
            None => true,
        };
        if !stale {
            return Ok(self
                .mask_cache
                .get(&key)
                .expect("cache entry checked above")
                .texture
                .clone());
        }
        let weights = match kind {
            MaskKind::Person => {
                ai_masks::person_mask_weights(pixels, width, height, self.ai_session.as_mut())
            }
            MaskKind::Skin => {
                ai_masks::skin_mask_weights(pixels, width, height, self.ai_session.as_mut())
            }
        };
        let texture = std::sync::Arc::new(ai_masks::upload_mask_texture(
            &self.ctx, width, height, &weights,
        ));
        self.mask_uploads += 1;
        self.mask_cache.insert(
            key,
            MaskCacheEntry {
                texture: texture.clone(),
                frame_seq: seq,
                width,
                height,
            },
        );
        Ok(texture)
    }

    /// 1x1 `Rgba16Float` plate texture holding `params.color` (linear) —
    /// the solid-color background for [`EffectKind::BackgroundReplace`].
    /// Future work can bind a real image plate here instead.
    fn solid_plate(ctx: &RenderContext, color: [f32; 3]) -> wgpu::Texture {
        ai_masks::upload_rgba_f32(
            ctx,
            1,
            1,
            &[color[0], color[1], color[2], 1.0],
            "sf-engine bg-replace plate",
        )
    }

    /// Subscribes an input to a scope kind (mirrors the `scopes.subscribe`
    /// IPC command). Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::UnknownInput`] when the input does not exist.
    pub fn scopes_subscribe(&mut self, input_id: &str, kind: ScopeKind) -> Result<(), EngineError> {
        if self.state.input_color(input_id).is_none() {
            return Err(EngineError::UnknownInput(input_id.to_string()));
        }
        self.scopes.subscribe(input_id, kind);
        Ok(())
    }

    /// Removes a scope subscription (mirrors `scopes.unsubscribe`); returns
    /// whether one existed.
    pub fn scopes_unsubscribe(&mut self, input_id: &str, kind: ScopeKind) -> bool {
        self.scopes.unsubscribe(input_id, kind)
    }

    /// Drains queued `scopes.frame` payloads for the UI.
    pub fn drain_scope_frames(&mut self) -> Vec<ScopeFrame> {
        self.scopes.drain_frames()
    }

    /// Grades one input frame with that input's grade and filter stack from
    /// the state.
    ///
    /// * Unknown `input_id` -> [`EngineError::UnknownInput`].
    /// * The effective grade is the input's base grade with its look-filter
    ///   stack folded in ([`resolve_grade`]).
    /// * Identity effective grade + no active effects -> the input pixels
    ///   are returned **unchanged** (bit-exact; no GPU work beyond upload).
    /// * Otherwise: upload -> grade pass, or the grade-less
    ///   [`linearize`](sf_render::GradePass::linearize) pass when the
    ///   effective grade is identity -> optional unsharp mask when
    ///   `sharpness > 0` -> active effect slots in stack order -> output
    ///   encode -> download. Effects run in linear light between grade and
    ///   encode (the C5 decision).
    /// * AI effect kinds (C7 policy):
    ///   - [`EffectKind::BackgroundBlur`]: person mask from the AI session
    ///     (model) or the hue fallback — functional without a model.
    ///   - [`EffectKind::SkinSmooth`]: skin mask via the C6-tested
    ///     hue/model-union path — functional without a model.
    ///   - [`EffectKind::BackgroundReplace`]: ONLY when the AI session has a
    ///     model; without one the slot is skipped gracefully and counted in
    ///     [`ai_skipped`](Engine::ai_skipped) (the spec's grey-out rule —
    ///     the hue fallback is too unreliable for replacement), with zero
    ///     GPU work. Non-AI slots keep working either way.
    /// * Effect slots with `mask: Some(...)` run region-masked: the effect
    ///   renders into a scratch target, then
    ///   [`masked_mix`](sf_filterfx::EffectPass::masked_mix) blends it with
    ///   the unmasked frame through the region mask — masked-out pixels are
    ///   bit-identical to the unmasked frame. `Circle`/`Rectangle` masks are
    ///   generated analytically per frame;
    ///   [`MaskRef::Person`](sf_filters::MaskRef::Person) reuses the cached
    ///   person mask. (Look slots with masks: grade-level masking is out of
    ///   C7 scope — TODO, not implemented.)
    /// * [`EffectKind::NoiseReduction`] blends with the input's previous
    ///   frame; on the first frame the current frame doubles as `prev` (a
    ///   blend of a frame with itself is the frame — correct first-frame
    ///   behavior).
    ///
    /// After the final frame exists on the GPU, the scope hub taps it: the
    /// tap moved from the graded frame to the post-effects frame in C5, so
    /// scopes see exactly what the program outputs. With zero subscribers
    /// the tap is a no-op — no GPU scope work runs.
    ///
    /// LUT files named by `grade.lut.file` (relative to the current working
    /// directory) are parsed once and cached; the effective (look-folded)
    /// grade drives the grade uniform, curves, and LUT.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] on unknown inputs, bad pixel lengths, LUT
    /// load failures, or any render-layer error.
    pub fn grade_input_frame(
        &mut self,
        input_id: &str,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<Frame, EngineError> {
        let input = self
            .state
            .inputs
            .iter()
            .find(|i| i.id == input_id)
            .ok_or_else(|| EngineError::UnknownInput(input_id.to_string()))?;
        let base = input.color.grade.clone();
        let stack = input.filters.clone();

        // 1. Effective grade: base + look filters folded in, in stack order.
        let effective = resolve_grade(&base, &stack, &self.look_registry);

        // 2. Active effect slots, in stack order. AI kinds route to the C7
        //    AI paths below (BackgroundReplace only with a model, otherwise
        //    counted in ai_skipped); zero-strength and disabled slots never
        //    reach the GPU.
        let mut active: Vec<(EffectKind, EffectParams, f32, Option<MaskRef>)> = Vec::new();
        for slot in stack.slots() {
            if !slot.enabled || slot.intensity <= 0.0 {
                continue;
            }
            if let Filter::Effect { kind, params } = &slot.filter {
                active.push((
                    kind.clone(),
                    params.clone(),
                    slot.intensity,
                    slot.mask.clone(),
                ));
            }
        }

        let src = self.ctx.upload_rgba8(width, height, pixels)?;

        // 3. Fast path: identity grade and no effects -> bit-exact bypass.
        let out_tex: Arc<wgpu::Texture>;
        if effective.is_identity() && active.is_empty() {
            out_tex = Arc::new(src);
        } else {
            // Resolve + cache the LUT (if any) before touching the GPU.
            let lut_path: Option<String> = effective.lut.as_ref().map(|l| l.file.clone());
            if let Some(path) = &lut_path {
                if !self.lut_cache.contains_key(path) {
                    let text = std::fs::read_to_string(path)?;
                    let cube = sf_render::lut::parse_cube(&text)
                        .map_err(|e| EngineError::LutLoad(format!("{path}: {e}")))?;
                    self.lut_cache.insert(path.clone(), cube);
                }
            }
            // Working targets are acquired before `lut` borrows
            // `self.lut_cache`: `work_targets` needs `&mut self`.
            let (linear_a, linear_b, out) = self.work_targets(width, height);
            let lut = lut_path.as_ref().and_then(|path| self.lut_cache.get(path));

            let baked = effective.curves.bake();

            // 4. Grade into linear working space — or grade-less linearize
            //    when the effective grade is identity (effects still need
            //    linear light). The working targets are reused across frames
            //    (C8) and recreated only when the frame size changes.
            if effective.is_identity() {
                // transfer 0.0 = gamma-2.4 decode (matches
                // GradeUniform.transfer in the MVP).
                self.pass.linearize(&self.ctx, 0.0, &src, &linear_a)?;
            } else {
                self.pass
                    .apply(&self.ctx, &effective, &baked, lut, &src, &linear_a)?;
            }
            let (mut cur, mut other) = (linear_a, linear_b);

            // Optional detail pass (unsharp mask in linear light).
            if effective.sharpness > 0.0 {
                let sharpness01 = (effective.sharpness / 100.0).clamp(0.0, 1.0);
                self.pass
                    .apply_detail(&self.ctx, sharpness01, &cur, &other)?;
                std::mem::swap(&mut cur, &mut other);
            }

            // 5. Effect slots in stack order; the per-input frame counter
            //    doubles as the frame seed (film grain) and drives the AI
            //    mask refresh cadence.
            let seq = self.frame_seq.entry(input_id.to_string()).or_insert(0);
            *seq += 1;
            let seq = *seq;
            let mut nr_active = false;
            for (kind, params, intensity, mask) in &active {
                // NOTE: the fx methods return Ok(false) with zero GPU work
                // when the premultiplied amount is <= 0 — in that case `dst`
                // is untouched, so the swap must be skipped (swapping would
                // promote an uninitialized texture into the pipeline).
                match kind {
                    // C7 AI background blur: person mask (model, else hue
                    // fallback) — functional without a model.
                    EffectKind::BackgroundBlur => {
                        let person =
                            self.person_mask_texture(input_id, width, height, pixels, seq)?;
                        if self.fx.apply_background_blur(
                            &self.ctx, params, *intensity, &cur, &person, &other,
                        )? {
                            std::mem::swap(&mut cur, &mut other);
                        }
                    }
                    // C7 AI skin smoothing: skin mask (C6 hue/model union) —
                    // functional without a model.
                    EffectKind::SkinSmooth => {
                        let skin = self.skin_mask_texture(input_id, width, height, pixels, seq)?;
                        if self
                            .fx
                            .apply_skin_smooth(&self.ctx, params, *intensity, &cur, &skin, &other)?
                        {
                            std::mem::swap(&mut cur, &mut other);
                        }
                    }
                    // C7 AI background replacement: ONLY with a model. Without
                    // one the spec's grey-out rule applies — the hue fallback
                    // is too unreliable for replacement, so the slot does
                    // zero GPU work (counted) and the frame still processes.
                    EffectKind::BackgroundReplace => {
                        if self.ai_session.model_available() {
                            let person =
                                self.person_mask_texture(input_id, width, height, pixels, seq)?;
                            let plate = Self::solid_plate(&self.ctx, params.color);
                            if self.fx.apply_background_replace(
                                &self.ctx, params, *intensity, &cur, &person, &plate, &other,
                            )? {
                                std::mem::swap(&mut cur, &mut other);
                            }
                        } else {
                            self.ai_skipped += 1;
                        }
                    }
                    // Non-AI effects, optionally region-masked.
                    _ => {
                        let prev = if *kind == EffectKind::NoiseReduction {
                            nr_active = true;
                            // First frame: no previous frame yet — the current
                            // frame doubles as prev. Read-only double-binding
                            // is legal; the blend of a frame with itself is
                            // the frame.
                            Some(self.nr_prev.get(input_id).unwrap_or(&cur))
                        } else {
                            None
                        };
                        match mask {
                            None => {
                                if self.fx.apply_slot(
                                    &self.ctx, kind, params, *intensity, &cur, &other, prev,
                                    seq as u32,
                                )? {
                                    std::mem::swap(&mut cur, &mut other);
                                }
                            }
                            Some(m) => {
                                // (a) Render the effect into a scratch target.
                                let scratch_tex = self.ctx.create_linear_target(width, height);
                                if self.fx.apply_slot(
                                    &self.ctx,
                                    kind,
                                    params,
                                    *intensity,
                                    &cur,
                                    &scratch_tex,
                                    prev,
                                    seq as u32,
                                )? {
                                    // (b) The region mask texture: analytic for
                                    // Circle/Rectangle, cached person mask for
                                    // Person.
                                    let mask_tex = match m {
                                        MaskRef::Circle { .. } | MaskRef::Rectangle { .. } => {
                                            let region_tex =
                                                self.ctx.create_linear_target(width, height);
                                            self.fx.gen_region_mask(&self.ctx, m, &region_tex)?;
                                            std::sync::Arc::new(region_tex)
                                        }
                                        MaskRef::Person => self.person_mask_texture(
                                            input_id, width, height, pixels, seq,
                                        )?,
                                    };
                                    // (c) Blend: mask = 1 inside the region selects
                                    // the EFFECTED pixels, so a = original (cur),
                                    // b = effected (scratch). Masked-out pixels
                                    // are bit-identical to the unmasked frame.
                                    self.fx.masked_mix(
                                        &self.ctx,
                                        1.0,
                                        &cur,
                                        &scratch_tex,
                                        &mask_tex,
                                        &other,
                                    )?;
                                    std::mem::swap(&mut cur, &mut other);
                                }
                            }
                        }
                    }
                }
            }

            // 6. Stash the post-effects frame for next frame's NR, sized
            //    like the current frame (recreated when stale).
            if nr_active {
                let stale = self
                    .nr_prev
                    .get(input_id)
                    .map_or(true, |t| t.size() != cur.size());
                if stale {
                    self.nr_prev.insert(
                        input_id.to_string(),
                        Self::nr_prev_target(&self.ctx, width, height),
                    );
                }
                let prev_tex = self.nr_prev.get(input_id).expect("nr_prev just stored");
                let mut encoder =
                    self.ctx
                        .device()
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("sf-engine nr prev copy"),
                        });
                let size = cur.size();
                encoder.copy_texture_to_texture(
                    wgpu::ImageCopyTexture {
                        texture: &cur,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::ImageCopyTexture {
                        texture: prev_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: size.width,
                        height: size.height,
                        depth_or_array_layers: 1,
                    },
                );
                self.ctx.queue().submit(Some(encoder.finish()));
            }

            // 7. Output encode to display RGBA8 (reused target, C8).
            let transfer = effective.to_uniform().transfer;
            self.pass.encode_output(&self.ctx, transfer, &cur, &out)?;
            out_tex = out;
        }

        // Scope tap on the FINAL frame (C5 decision): post-effects, exactly
        // what the program outputs. No-op when nobody is subscribed.
        self.scopes
            .maybe_emit(&self.ctx, &self.scope_pass, input_id, &out_tex)?;

        let pixels = self.ctx.download_rgba8(&out_tex)?;
        Ok(Frame {
            width,
            height,
            pixels,
        })
    }

    /// Reused working targets for the effect pipeline (C8 optimization):
    /// two linear ping-pong textures plus the final RGBA8 target, recreated
    /// only when the frame size changes.
    ///
    /// Returns cheap `Arc` handle clones, so the caller can swap `cur`/`other`
    /// freely exactly as before. Every pass fully overwrites its target each
    /// frame, so pixel output is identical to allocating fresh textures.
    fn work_targets(
        &mut self,
        width: u32,
        height: u32,
    ) -> (Arc<wgpu::Texture>, Arc<wgpu::Texture>, Arc<wgpu::Texture>) {
        let stale = self
            .work
            .as_ref()
            .map_or(true, |w| w.width != width || w.height != height);
        if stale {
            self.work = Some(WorkTargets {
                width,
                height,
                linear: [
                    Arc::new(self.ctx.create_linear_target(width, height)),
                    Arc::new(self.ctx.create_linear_target(width, height)),
                ],
                out: Arc::new(self.ctx.create_rgba8_target(width, height)),
            });
        }
        let w = self.work.as_ref().expect("work targets just stored");
        (w.linear[0].clone(), w.linear[1].clone(), w.out.clone())
    }

    /// Previous-frame texture for noise reduction: linear working format
    /// with `COPY_DST` so the end-of-frame copy can land in it
    /// ([`RenderContext::create_linear_target`](sf_render::RenderContext::create_linear_target)
    /// lacks `COPY_DST`).
    fn nr_prev_target(ctx: &RenderContext, width: u32, height: u32) -> wgpu::Texture {
        ctx.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-engine nr prev"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }
}
