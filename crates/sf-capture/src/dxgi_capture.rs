//! DXGI desktop duplication screen capture (Windows-only scaffold).
//!
//! # Status: scaffold — NOT implemented
//!
//! This module declares the *plausible public shape* of the Windows screen
//! source so engine code can be written against it, but every constructor
//! body is `todo!()`. Real implementation needs:
//!
//! * `windows-rs` (`windows::Win32::Graphics::Dxgi::*`): `CreateDXGIFactory1`
//!   → `EnumAdapters1` → `EnumOutputs` → `IDXGIOutput1::DuplicateOutput`,
//! * `AcquireNextFrame(timeout)` → `IDXGIResource` → `QueryInterface::<ID3D11Texture2D>()`,
//!   copy to a staging texture, map, and convert BGRA→RGBA8 on the CPU
//!   (or hand the texture to wgpu via shared-handle interop),
//! * `ReleaseFrame` after each copy; handle `DXGI_ERROR_ACCESS_LOST` by
//!   re-creating the duplication interface (e.g. on mode change).
//!
//! Compiled only on Windows (`#[cfg(windows)]` in lib.rs).

use crate::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// DXGI desktop-duplication screen source (Windows).
///
/// Captures the `monitor`-th adapter output at its native resolution.
/// `region` optionally crops to `(x, y, w, h)`; `None` = full monitor.
pub struct DxgiProducer {
    _private: (),
}

impl DxgiProducer {
    /// Start duplicating `monitor` (0 = primary).
    pub fn open(
        _monitor: usize,
        _region: Option<(u32, u32, u32, u32)>,
    ) -> Result<Self, CaptureError> {
        todo!("Windows-only scaffold: implement via IDXGIOutput1::DuplicateOutput")
    }

    /// List monitors as `(index, name, width, height)`.
    pub fn list_monitors() -> Result<Vec<(usize, String, u32, u32)>, CaptureError> {
        todo!("Windows-only scaffold: implement via IDXGIAdapter::EnumOutputs")
    }
}

impl FrameProducer for DxgiProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        todo!("Windows-only scaffold: AcquireNextFrame → staging texture → BGRA→RGBA")
    }

    fn stream_info(&self) -> StreamInfo {
        todo!("Windows-only scaffold")
    }
}
