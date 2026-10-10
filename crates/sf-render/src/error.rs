//! Error type for `sf-render`.

/// Errors from the headless wgpu render layer.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No GPU adapter could be enumerated.
    #[error("no suitable GPU adapter found")]
    NoAdapter,
    /// `request_device` failed (message from wgpu).
    #[error("device request failed: {0}")]
    DeviceRequest(String),
    /// A texture did not have the expected size.
    #[error("texture size mismatch")]
    SizeMismatch,
    /// Mapping a download buffer failed.
    #[error("buffer map failed")]
    MapFailed,
    /// A `.cube` LUT failed to parse.
    #[error("lut error: {0}")]
    Lut(#[from] sf_color::lut::LutParseError),
    /// Filesystem I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
