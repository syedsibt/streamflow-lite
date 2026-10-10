//! `sf-ai-color` — AI-assisted color analysis for StreamFlow Lite (phase C6).
//!
//! All features analyze frames and produce **editable [`sf_color::ColorGrade`]
//! parameters** (or suggestions) — never black-box mutations. The modules:
//!
//! - [`stats`]: per-frame statistics (luma percentiles, channel means, …).
//! - [`auto`]: one-click AI auto color (spec §6.1).
//! - [`camera_match`]: AI camera matching + ColorChecker least squares (§6.2).
//! - [`skin`]: skin-tone protection masks (§6.3).
//! - [`suggest`]: AI look suggestions (§6.4).
//! - [`drift`]: auto drift guard — suggest-only, never auto-applies (§6.5).

pub mod auto;
pub mod camera_match;
pub mod drift;
pub mod skin;
pub mod stats;
pub mod suggest;
