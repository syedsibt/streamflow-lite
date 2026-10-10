//! C7 AI mask computation and caching for [`crate::Engine`].
//!
//! Two mask flavors feed the C7 AI effects:
//!
//! * [`MaskKind::Person`] — person segmentation weights (`1.0` = person).
//!   Computed by [`person_mask_weights`]: the [`InferenceSession`] model when
//!   one is available, else the hue heuristic as a functional fallback.
//! * [`MaskKind::Skin`] — skin-tone weights (`1.0` = skin). Computed by
//!   [`skin_mask_weights`] via the C6-tested
//!   [`skin_mask_combined`](sf_ai_color::skin::skin_mask_combined) path
//!   (model face mask unioned with the hue heuristic when available).
//!
//! # Refresh policy
//!
//! Masks are cached per input and recomputed only when the frame size changes
//! or every [`MASK_REFRESH_FRAMES`] frames (15 frames ≈ 0.5 s at 30 fps).
//! Rationale: person/skin geometry changes slowly relative to the frame rate
//! (a talking head barely moves in half a second), while recomputing every
//! frame would cost a full-frame CPU mask pass plus a texture upload per
//! frame. The 15-frame cadence keeps masks fresh enough for live streaming
//! while bounding CPU cost, and — critically for tests — it is driven by the
//! engine's deterministic per-input frame counter, never by wall-clock time.

use sf_ai_color::skin::{skin_mask_combined, skin_mask_hue};
use sf_ai_runtime::{FaceMask, InferenceSession};
use sf_render::RenderContext;

/// Recompute a cached mask when the frame size changes or this many frames
/// have passed since the cached entry (15 frames ≈ 0.5 s at 30 fps; see the
/// module docs for the rationale).
pub const MASK_REFRESH_FRAMES: u64 = 15;

/// Which AI-derived mask a cache entry holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MaskKind {
    /// Person segmentation weights (`1.0` = person); feeds background blur /
    /// background replace and `MaskRef::Person` region masks.
    Person,
    /// Skin-tone weights (`1.0` = skin); feeds skin smoothing.
    Skin,
}

/// One cached AI mask: the GPU texture plus the metadata the refresh policy
/// needs. (The mask kind lives in the cache key alongside the input id.)
pub struct MaskCacheEntry {
    /// `Rgba16Float` texture, `.r` = weight `0.0..=1.0`, g/b/a = 0.
    /// Reference-counted: the cache and the in-flight frame share the
    /// texture without copying it (`wgpu::Texture` is not `Clone`).
    pub texture: std::sync::Arc<wgpu::Texture>,
    /// Engine per-input frame counter value when this entry was computed.
    pub frame_seq: u64,
    /// Frame width the entry was computed for.
    pub width: u32,
    /// Frame height the entry was computed for.
    pub height: u32,
}

/// Person weights for the input RGBA8 frame (`1.0` = person).
///
/// * Model available: `session.face_mask()` on the RGB bytes, nearest-resampled
///   to `width`×`height` when the session runs at its own resolution. Any
///   inference error — or a malformed mask — falls back to the hue path.
/// * Model unavailable: the hue heuristic
///   ([`skin_mask_hue`](sf_ai_color::skin::skin_mask_hue)) serves as the
///   person approximation. Documented degradation: skin regions stay sharp
///   while clothing is treated as background — wrong vs true segmentation,
///   but functional (background still blurs, faces stay sharp).
///
/// A short input buffer (length ≠ `width*height*4`) yields an all-zero mask
/// instead of panicking; the engine validates the length earlier via the
/// texture upload, so this is defense-in-depth only.
pub fn person_mask_weights(
    rgba8: &[u8],
    width: u32,
    height: u32,
    session: &mut dyn InferenceSession,
) -> Vec<f32> {
    let n = width as usize * height as usize;
    if rgba8.len() != n * 4 {
        return vec![0.0; n];
    }
    if session.model_available() {
        let mut rgb = Vec::with_capacity(n * 3);
        for px in rgba8.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
        if let Ok(face) = session.face_mask(&rgb, width, height) {
            if let Some(w) = resample_face_weights(&face, width, height) {
                return w;
            }
        }
    }
    // Hue fallback: model missing, inference failed, or malformed mask.
    skin_mask_hue(rgba8, width, height).weights
}

/// Skin weights for the input RGBA8 frame (`1.0` = skin) — the C6 tested
/// path: [`skin_mask_combined`](sf_ai_color::skin::skin_mask_combined) with
/// the session's face mask when a model is available (`None` on any error or
/// when unavailable), i.e. the hue heuristic alone as the functional
/// fallback.
pub fn skin_mask_weights(
    rgba8: &[u8],
    width: u32,
    height: u32,
    session: &mut dyn InferenceSession,
) -> Vec<f32> {
    let n = width as usize * height as usize;
    if rgba8.len() != n * 4 {
        return vec![0.0; n];
    }
    let face: Option<FaceMask> = if session.model_available() {
        let mut rgb = Vec::with_capacity(n * 3);
        for px in rgba8.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
        session.face_mask(&rgb, width, height).ok()
    } else {
        None
    };
    skin_mask_combined(rgba8, width, height, face.as_ref()).weights
}

/// Nearest-neighbor resample of a [`FaceMask`] to `width`×`height`.
/// Returns `None` for malformed masks (zero dims, length mismatch) so the
/// caller can fall back to the hue path. Weights are clamped to `0.0..=1.0`.
fn resample_face_weights(face: &FaceMask, width: u32, height: u32) -> Option<Vec<f32>> {
    if face.width == 0 || face.height == 0 {
        return None;
    }
    if face.weights.len() != face.width as usize * face.height as usize {
        return None;
    }
    if width == 0 || height == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        let fy =
            (y as u64 * face.height as u64 / height as u64).min(face.height as u64 - 1) as usize;
        for x in 0..width {
            let fx =
                (x as u64 * face.width as u64 / width as u64).min(face.width as u64 - 1) as usize;
            out.push(face.weights[fy * face.width as usize + fx].clamp(0.0, 1.0));
        }
    }
    Some(out)
}

/// 256-byte-aligned row pitch for `Rgba16Float` uploads.
fn padded_row_bytes(w: u32) -> u32 {
    (w * 8).div_ceil(256) * 256
}

/// Uploads a row-major `w*h*4` f32 frame into a fresh `Rgba16Float` texture
/// (f32 → f16). The texture carries `COPY_DST | TEXTURE_BINDING | COPY_SRC`
/// so AI passes can sample it and tests can download it back.
pub fn upload_rgba_f32(
    ctx: &RenderContext,
    width: u32,
    height: u32,
    data: &[f32],
    label: &str,
) -> wgpu::Texture {
    assert_eq!(data.len(), (width as usize) * (height as usize) * 4);
    let tex = ctx.device().create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
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
    });
    let row = padded_row_bytes(width);
    let mut bytes = vec![0u8; (row * height) as usize];
    for (i, &v) in data.iter().enumerate() {
        let px = i / 4;
        let (x, y) = ((px as u32) % width, (px as u32) / width);
        let off = (y * row + x * 8 + (i % 4) as u32 * 2) as usize;
        bytes[off..off + 2].copy_from_slice(&sf_render::f16::f32_to_f16(v).to_le_bytes());
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
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    tex
}

/// Uploads per-pixel weights as an `Rgba16Float` mask texture
/// (`.r` = weight, g/b/a = 0).
pub fn upload_mask_texture(
    ctx: &RenderContext,
    width: u32,
    height: u32,
    weights: &[f32],
) -> wgpu::Texture {
    assert_eq!(weights.len(), (width as usize) * (height as usize));
    let mut data = Vec::with_capacity(weights.len() * 4);
    for &w in weights {
        data.extend_from_slice(&[w, 0.0, 0.0, 0.0]);
    }
    upload_rgba_f32(ctx, width, height, &data, "sf-engine ai mask")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sf_ai_runtime::StubSession;

    fn solid_rgba8(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&[r, g, b, 255]);
        }
        v
    }

    #[test]
    fn person_mask_stub_falls_back_to_hue() {
        // No model: skin patch -> high weights, blue -> ~0 (the documented
        // degraded-but-functional person approximation).
        let mut s = StubSession::new("person-seg");
        let skin = person_mask_weights(&solid_rgba8(8, 8, 200, 150, 120), 8, 8, &mut s);
        let mean: f32 = skin.iter().sum::<f32>() / skin.len() as f32;
        assert!(mean > 0.7, "skin mean = {mean}");
        let blue = person_mask_weights(&solid_rgba8(8, 8, 0, 0, 255), 8, 8, &mut s);
        let mean_b: f32 = blue.iter().sum::<f32>() / blue.len() as f32;
        assert!(mean_b < 0.05, "blue mean = {mean_b}");
    }

    #[test]
    fn skin_mask_stub_equals_hue_path() {
        let mut s = StubSession::new("person-seg");
        let px = solid_rgba8(8, 8, 200, 150, 120);
        let got = skin_mask_weights(&px, 8, 8, &mut s);
        let want = skin_mask_hue(&px, 8, 8).weights;
        assert_eq!(got, want);
    }

    #[test]
    fn short_buffer_yields_zero_mask_without_panic() {
        let mut s = StubSession::new("person-seg");
        let short = vec![0u8; 10];
        assert_eq!(person_mask_weights(&short, 8, 8, &mut s), vec![0.0; 64]);
        assert_eq!(skin_mask_weights(&short, 8, 8, &mut s), vec![0.0; 64]);
    }

    /// A fake model-backed session for the model-available paths.
    struct FakeModel {
        fw: u32,
        fh: u32,
        weights: Vec<f32>,
    }

    impl InferenceSession for FakeModel {
        fn name(&self) -> &str {
            "fake-seg"
        }
        fn model_available(&self) -> bool {
            true
        }
        fn face_mask(
            &mut self,
            rgb: &[u8],
            width: u32,
            height: u32,
        ) -> Result<FaceMask, sf_ai_runtime::AiError> {
            assert_eq!(rgb.len(), width as usize * height as usize * 3);
            Ok(FaceMask {
                width: self.fw,
                height: self.fh,
                weights: self.weights.clone(),
            })
        }
    }

    #[test]
    fn person_mask_uses_model_and_resamples() {
        // Model returns a 2x2 mask over an 8x8 frame: nearest resample must
        // carry the quadrants through, clamped to 0..1.
        let mut s = FakeModel {
            fw: 2,
            fh: 2,
            weights: vec![0.0, 2.0, -1.0, 0.5],
        };
        let px = solid_rgba8(8, 8, 0, 0, 255); // blue: hue path would say 0
        let w = person_mask_weights(&px, 8, 8, &mut s);
        assert_eq!(w[0], 0.0); // top-left quadrant
        assert_eq!(w[7], 1.0); // top-right quadrant (2.0 clamped)
        assert_eq!(w[8 * 7], 0.0); // bottom-left (-1.0 clamped)
        assert_eq!(w[8 * 7 + 7], 0.5); // bottom-right
    }

    #[test]
    fn person_mask_malformed_model_mask_falls_back_to_hue() {
        let mut s = FakeModel {
            fw: 2,
            fh: 2,
            weights: vec![1.0; 3], // length mismatch: malformed
        };
        let px = solid_rgba8(8, 8, 200, 150, 120);
        let got = person_mask_weights(&px, 8, 8, &mut s);
        let want = skin_mask_hue(&px, 8, 8).weights;
        assert_eq!(got, want);
    }

    #[test]
    fn skin_mask_unions_model_evidence() {
        // Blue frame (hue says 0) + all-ones model mask -> protected.
        let mut s = FakeModel {
            fw: 8,
            fh: 8,
            weights: vec![1.0; 64],
        };
        let px = solid_rgba8(8, 8, 0, 0, 255);
        let w = skin_mask_weights(&px, 8, 8, &mut s);
        let mean: f32 = w.iter().sum::<f32>() / w.len() as f32;
        assert!(mean > 0.99, "union mean = {mean}");
    }
}
