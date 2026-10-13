//! Headless wgpu context: device, queue, texture upload/download.
//!
//! There is no window and no surface: the context enumerates adapters with
//! [`wgpu::PowerPreference::HighPerformance`], opens one device, and exposes
//! helpers to move RGBA8 pixels in and out of GPU textures. Blocking is done
//! with [`pollster`].
//!
//! On Linux the default backend is Vulkan. To force the lavapipe software
//! driver (e.g. for headless CI), set
//! `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json` in the environment
//! before calling [`RenderContext::new_headless`].

use crate::error::RenderError;

static GPU_ERRORS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_GPU_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static ADAPTER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// GPU validation/runtime errors since start, the last message, and the
/// adapter in use (name, backend, type — e.g. a software `Cpu` adapter).
pub fn gpu_status() -> (u64, Option<String>, Option<String>) {
    (
        GPU_ERRORS.load(std::sync::atomic::Ordering::Relaxed),
        LAST_GPU_ERROR.lock().ok().and_then(|l| l.clone()),
        ADAPTER.lock().ok().and_then(|a| a.clone()),
    )
}

/// Headless wgpu device + queue pair, plus texture upload/download helpers.
pub struct RenderContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl RenderContext {
    /// Creates a headless (windowless) wgpu context.
    ///
    /// Picks a [`wgpu::PowerPreference::HighPerformance`] adapter and opens a
    /// device with no extra features. Blocks via [`pollster`].
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::NoAdapter`] when no adapter can be enumerated,
    /// or [`RenderError::DeviceRequest`] when the device request fails.
    pub fn new_headless() -> Result<Self, RenderError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok_or(RenderError::NoAdapter)?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|e| RenderError::DeviceRequest(e.to_string()))?;
        // wgpu's default handler PANICS on any validation error, which (in
        // the app) unwinds while the engine lock is held and poisons it —
        // every later command then fails. Record the error instead; the
        // operation that caused it returns an error / a blank frame.
        device.on_uncaptured_error(Box::new(|e| {
            let msg = e.to_string();
            eprintln!("GPU error: {msg}");
            GPU_ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Ok(mut last) = LAST_GPU_ERROR.lock() {
                *last = Some(msg.chars().take(400).collect());
            }
        }));
        let info = adapter.get_info();
        if let Ok(mut a) = ADAPTER.lock() {
            *a = Some(format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type));
        }
        Ok(Self { device, queue })
    }

    /// Fails (instead of a wgpu validation error) for sizes the device
    /// cannot create.
    pub fn check_size(&self, width: u32, height: u32) -> Result<(), RenderError> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Err(RenderError::TextureSize(width, height, max));
        }
        Ok(())
    }

    /// The wgpu device.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Uploads RGBA8 pixels into an `rgba8unorm` texture.
    ///
    /// The texture is created with `COPY_DST | TEXTURE_BINDING | COPY_SRC`
    /// usage, so it can feed the grade pass and still be downloaded.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::SizeMismatch`] when `pixels.len()` is not
    /// `width * height * 4`.
    pub fn upload_rgba8(
        &self,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<wgpu::Texture, RenderError> {
        if pixels.len() != (width as usize) * (height as usize) * 4 {
            return Err(RenderError::SizeMismatch);
        }
        self.check_size(width, height)?;
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-render upload rgba8"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Ok(tex)
    }

    /// Creates a linear working target: `rgba16float` with
    /// `STORAGE_BINDING | TEXTURE_BINDING | COPY_SRC` usage.
    pub fn create_linear_target(&self, width: u32, height: u32) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-render linear target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// Creates a final 8-bit target: `rgba8unorm` with
    /// `STORAGE_BINDING | TEXTURE_BINDING | COPY_SRC` usage (written by the
    /// encode pass; sampled by the scope downscale pass).
    pub fn create_rgba8_target(&self, width: u32, height: u32) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sf-render rgba8 target"),
            size: wgpu::Extent3d {
                width,
                height,
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
        })
    }

    /// Downloads an `rgba8unorm` `COPY_SRC` texture into a `Vec<u8>`.
    ///
    /// The GPU requires 256-byte row-pitch alignment for
    /// texture-to-buffer copies; the padding is stripped, so the returned
    /// vector is exactly `width * height * 4` bytes in row-major RGBA order.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::MapFailed`] when the readback buffer cannot be
    /// mapped.
    pub fn download_rgba8(&self, tex: &wgpu::Texture) -> Result<Vec<u8>, RenderError> {
        let size = tex.size();
        let (width, height) = (size.width, size.height);
        // 256-byte aligned row pitch required by copy_texture_to_buffer.
        let padded_row = ((width * 4 + 255) / 256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sf-render download buffer"),
            size: (padded_row as u64) * (height as u64),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sf-render download"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        // Drive the mapping to completion on this thread.
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|_| RenderError::MapFailed)?
            .map_err(|_| RenderError::MapFailed)?;

        let out = {
            let mapped = slice.get_mapped_range();
            let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for y in 0..height as usize {
                let start = y * padded_row as usize;
                out.extend_from_slice(&mapped[start..start + (width as usize) * 4]);
            }
            out
        };
        buffer.unmap();
        Ok(out)
    }
}
