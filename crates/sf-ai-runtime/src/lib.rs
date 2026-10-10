//! `sf-ai-runtime` — ONNX inference backend for StreamFlow Lite phase C6 (AI color).
//!
//! This crate owns the *inference plumbing*: model session lifecycle, graceful
//! degradation when a model file is absent, and the [`InferenceSession`] trait.
//! The statistical AI-color features themselves (auto color, camera match,
//! skin-tone protection, look suggest, drift guard) live in `sf-ai-color`,
//! which codes against the types here — notably [`FaceMask`], passed to
//! `sf-ai-color`'s `skin_mask_combined` as `Option<&FaceMask>`.
//!
//! # Backends
//!
//! - [`StubSession`] (always available): [`InferenceSession::model_available`]
//!   returns `false` and [`InferenceSession::face_mask`] returns
//!   [`AiError::ModelMissing`]. Use it wherever a model file has not been
//!   vendored — it never panics and never performs inference.
//! - A real ONNX backend plugs in via the [`InferenceSession`] trait. The
//!   `ort` (ONNX Runtime) crate was evaluated for this during C6 and
//!   **abandoned**: `ort` 2.x is still prerelease (2.0.0-rc.13) and its
//!   prebuilt-binary download (`cdn.pyke.io`) does not complete reliably in
//!   this build environment (failed the ~15-minute build timebox twice, once
//!   on a TLS EOF and once hanging until timeout). The hue-based skin
//!   protection path is the tested default until a real backend lands.
//!
//! Graceful degradation is a spec rule: a missing model yields a clean
//! [`AiError::ModelMissing`], never a panic, and everything else keeps
//! working.

use thiserror::Error;

/// Per-pixel face/skin mask, values in 0..=1.
///
/// `weights.len()` must equal `width * height`; the producer guarantees this.
#[derive(Debug, Clone, PartialEq)]
pub struct FaceMask {
    /// Mask width in pixels.
    pub width: u32,
    /// Mask height in pixels.
    pub height: u32,
    /// Row-major per-pixel weights, each in 0.0..=1.0.
    pub weights: Vec<f32>,
}

/// Errors from an [`InferenceSession`].
#[derive(Debug, Error)]
pub enum AiError {
    /// The model file is missing (or the session has no backend).
    #[error("model missing: {model}")]
    ModelMissing {
        /// Name of the model that could not be provided.
        model: String,
    },
    /// The model file exists but inference failed.
    #[error("inference failed: {0}")]
    InferenceFailed(String),
    /// The input buffer did not match its declared dimensions.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// A model-backed face/skin mask session.
///
/// Object-safe: usable as `Box<dyn InferenceSession>`. Implementations must
/// be `Send` so the engine can run inference on a worker thread.
pub trait InferenceSession: Send {
    /// Human-readable model name (e.g. `"ultraface-rfb"`).
    fn name(&self) -> &str;
    /// Whether a real model is loaded and inference can run.
    fn model_available(&self) -> bool;
    /// Compute a per-pixel face/skin mask for an RGB frame.
    ///
    /// `rgb` must hold exactly `width * height * 3` bytes. Mismatched
    /// dimensions return [`AiError::InvalidInput`] — never panic.
    fn face_mask(&mut self, rgb: &[u8], width: u32, height: u32) -> Result<FaceMask, AiError>;
}

/// Stub backend used when no model file is vendored.
///
/// `model_available()` is `false` and `face_mask()` always returns
/// [`AiError::ModelMissing`] carrying the model name given at construction.
pub struct StubSession {
    model: String,
}

impl StubSession {
    /// Create a stub for the named model.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }
}

impl InferenceSession for StubSession {
    fn name(&self) -> &str {
        &self.model
    }

    fn model_available(&self) -> bool {
        false
    }

    fn face_mask(&mut self, rgb: &[u8], width: u32, height: u32) -> Result<FaceMask, AiError> {
        validate_rgb(rgb, width, height)?;
        Err(AiError::ModelMissing {
            model: self.model.clone(),
        })
    }
}

/// Validate an RGB buffer against its declared dimensions.
///
/// Returns [`AiError::InvalidInput`] on any mismatch — never panics, even for
/// garbage dims (overflow-checked arithmetic).
pub(crate) fn validate_rgb(rgb: &[u8], width: u32, height: u32) -> Result<(), AiError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(3))
        .ok_or_else(|| AiError::InvalidInput("dimensions overflow usize".to_string()))?;
    if rgb.len() != expected {
        return Err(AiError::InvalidInput(format!(
            "buffer length {} does not match {width}x{height}x3 = {expected}",
            rgb.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_unavailable() {
        let mut session = StubSession::new("ultraface-rfb");
        assert!(!session.model_available());
        assert_eq!(session.name(), "ultraface-rfb");

        // Well-formed garbage input: clean ModelMissing, not a panic.
        let rgb = vec![0u8; 4 * 4 * 3];
        match session.face_mask(&rgb, 4, 4) {
            Err(AiError::ModelMissing { model }) => assert_eq!(model, "ultraface-rfb"),
            other => panic!("expected ModelMissing, got {other:?}"),
        }
    }

    #[test]
    fn stub_rejects_mismatched_dims_without_panic() {
        let mut session = StubSession::new("ultraface-rfb");

        // Buffer shorter than declared dims.
        let short = vec![0u8; 4 * 4 * 3];
        match session.face_mask(&short, 8, 8) {
            Err(AiError::InvalidInput(_)) => {}
            other => panic!("expected InvalidInput, got {other:?}"),
        }

        // Buffer longer than declared dims.
        let long = vec![0u8; 8 * 8 * 3 + 1];
        match session.face_mask(&long, 8, 8) {
            Err(AiError::InvalidInput(_)) => {}
            other => panic!("expected InvalidInput, got {other:?}"),
        }

        // Empty buffer with nonzero dims (pins the checked-arithmetic path).
        let empty: Vec<u8> = Vec::new();
        match session.face_mask(&empty, 64, 64) {
            Err(AiError::InvalidInput(_)) => {}
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn trait_is_object_safe() {
        let session: Box<dyn InferenceSession> = Box::new(StubSession::new("ultraface-rfb"));
        assert!(!session.model_available());
        assert_eq!(session.name(), "ultraface-rfb");

        let mut boxed = session;
        let rgb = vec![128u8; 2 * 2 * 3];
        match boxed.face_mask(&rgb, 2, 2) {
            Err(AiError::ModelMissing { model }) => assert_eq!(model, "ultraface-rfb"),
            other => panic!("expected ModelMissing, got {other:?}"),
        }
    }
}
