//! `sf-render` — headless wgpu render layer for StreamFlow Lite.
//!
//! [`RenderContext`] owns a windowless wgpu device + queue and moves RGBA8
//! pixels in and out of GPU textures. [`GradePass`] owns the four small
//! compute pipelines of the color chain: the fused grade pass (the GPU twin
//! of [`sf_color::cpu::apply_grade_cpu`]), an optional unsharp-mask detail
//! pass, the output encode pass, and a grade-less linearize pass that feeds
//! the effect chain when the grade is identity.
//!
//! All wgpu usage is backend-agnostic (Vulkan on Linux, DX12 on Windows);
//! nothing here touches a window or a surface.

pub mod composite_pass;
pub mod context;
pub mod error;
pub mod f16;
pub mod grade_pass;
pub mod scale_pass;

pub use composite_pass::CompositePass;
pub use context::RenderContext;
pub use error::RenderError;
pub use grade_pass::GradePass;
pub use scale_pass::ScalePass;

/// Re-exported so downstream crates (e.g. `sf-engine`) can name the grade
/// parameter types in public signatures without depending on `sf-color`
/// directly.
pub use sf_color::curves::BakedCurves;
pub use sf_color::grade::ColorGrade;
pub use sf_color::lut::{self, CubeFile};
