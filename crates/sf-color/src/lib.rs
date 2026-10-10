//! `sf-color` — color science for StreamFlow Lite.
//!
//! Pure color math plus the CPU reference implementation. This crate contains
//! **no GPU code** and performs no I/O: it parses `.cube` text supplied as
//! `&str`, bakes spline curves to 1D LUTs, normalizes [`ColorGrade`] UI
//! parameters into the [`GradeUniform`] uploaded to the wgpu grade pass, and
//! provides [`apply_grade_cpu`], the formula-exact CPU reference of that
//! shader.
//!
//! # Math contract
//!
//! The WGSL grade shader (owned by `sf-render`) and [`apply_grade_cpu`]
//! implement the **identical** formula chain — see the numbered steps in
//! [`apply_grade_cpu`]'s docs. Any change to the chain must be made in both
//! places and cross-checked with the tests in `cpu.rs`.
//!
//! # Modules
//!
//! * [`grade`] — [`ColorGrade`] UI parameters, [`LutRef`], and the 160-byte
//!   [`GradeUniform`] GPU uniform.
//! * [`temp`] — white-balance gains from color temperature + tint
//!   ([`kelvin_to_gains`]).
//! * [`curves`] — spline [`CurveSet`](curves::CurveSet), monotone-cubic
//!   evaluation, and 1024-texel [`BakedCurves`](curves::BakedCurves).
//! * [`lut`] — Adobe `.cube` parser ([`parse_cube`]) and
//!   [`trilinear_sample`], always normalized to a 33³ lattice.
//! * [`cpu`] — [`apply_grade_cpu`] plus the shared [`rgb_to_hsv`] /
//!   [`hsv_to_rgb`] helpers.

pub mod cpu;
pub mod curves;
pub mod grade;
pub mod lut;
pub mod temp;

pub use cpu::{apply_grade_cpu, hsv_to_rgb, rgb_to_hsv};
pub use curves::{BakedCurves, Curve, CurveError, CurveSet, BAKED_CURVE_SIZE, MAX_CURVE_POINTS};
pub use grade::{ColorGrade, GradeUniform, LutPosition, LutRef};
pub use lut::{parse_cube, trilinear_sample, CubeFile, LutParseError};
pub use temp::kelvin_to_gains;
