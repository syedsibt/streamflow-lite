//! MediaFoundation camera capture (Windows-only scaffold).
//!
//! # Status: scaffold — NOT implemented
//!
//! This module declares the *plausible public shape* of the Windows camera
//! source so engine code can be written against it, but every constructor
//! body is `todo!()`. Real implementation needs:
//!
//! * `windows-rs` (`windows::Win32::MediaFoundation::*`): `MFCreateSourceReaderFromMediaSource`
//!   / `MFCreateDeviceSource`, `IMFSourceReader::ReadSample`,
//! * `MFVideoFormat_RGB32` subtype negotiation, then a BGRA→RGBA swizzle
//!   (or a `IMFTransform` to RGB32), sample time → `pts_us` via
//!   `IMFSample::GetSampleTime` (100 ns units → µs),
//! * Device enumeration via `MFEnumDeviceSources` (`MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID`).
//!
//! Compiled only on Windows (`#[cfg(windows)]` in lib.rs).

use crate::{CaptureError, CapturedFrame, FrameProducer, StreamInfo};

/// MediaFoundation camera source (Windows).
///
/// Opens the `index`-th video-capture device and yields RGBA8 frames at
/// `width`×`height`, `fps`.
pub struct MfCameraProducer {
    _private: (),
}

impl MfCameraProducer {
    /// Open the `index`-th MF video-capture device.
    pub fn open(_index: usize, _width: u32, _height: u32, _fps: u32) -> Result<Self, CaptureError> {
        todo!("Windows-only scaffold: implement via IMFSourceReader (MFCreateDeviceSource + ReadSample)")
    }

    /// List available MF capture devices as `(index, friendly_name)`.
    pub fn list_devices() -> Result<Vec<(usize, String)>, CaptureError> {
        todo!("Windows-only scaffold: implement via MFEnumDeviceSources")
    }
}

impl FrameProducer for MfCameraProducer {
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        todo!("Windows-only scaffold: IMFSourceReader::ReadSample → RGB32 → RGBA")
    }

    fn stream_info(&self) -> StreamInfo {
        todo!("Windows-only scaffold")
    }
}
