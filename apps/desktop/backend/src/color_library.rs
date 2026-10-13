//! Color Studio services that live outside the grade math:
//!
//! * **LUT library** — import a `.cube` (validated up front, copied into the
//!   user's data folder so projects keep working when the original moves),
//!   list, remove.
//! * **Color presets** — save/apply/delete the complete look of an input:
//!   grade (basic, curves, wheels, LUT + intensity, creative, skin) and its
//!   filter stack (look + intensity, glow, skin smoothing, …).
//! * **Live preview tap** — the Color Studio preview is the *same* graded
//!   frame the media tick composites into the program/encoder (moved out of
//!   `render_scene_frame`, never re-graded, never pulled twice from the
//!   camera), with Before / After / Split modes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sf_color::ColorGrade;
use sf_engine::Frame;
use sf_filters::{FilterSlot, FilterStack};
use sf_ipc::FilterTarget;
use uuid::Uuid;

use super::{downscale_bilinear, encode_jpeg, Shell, ShellError, UiEvent};

/// A tap older than this is considered stale (the input left the program
/// and preview scenes) and the preview falls back to grading on demand.
const TAP_FRESH: Duration = Duration::from_millis(400);
/// The Color Studio counts as "watching" for this long after its last poll;
/// after that the tick stops handing frames to the tap (zero cost).
const WATCH_TIMEOUT: Duration = Duration::from_millis(1500);

/// Per-user data folder: `%APPDATA%\StreamFlow` on Windows,
/// `~/.config/streamflow` elsewhere; `STREAMFLOW_DATA_DIR` overrides.
pub fn default_data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("STREAMFLOW_DATA_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    #[cfg(windows)]
    if let Ok(a) = std::env::var("APPDATA") {
        return PathBuf::from(a).join("StreamFlow");
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".config").join("streamflow");
    }
    PathBuf::from(".streamflow")
}

/// Preview comparison mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewMode {
    /// Processed output (what goes to the stream).
    After,
    /// Original camera image.
    Before,
    /// Left half original, right half processed.
    Split,
}

impl PreviewMode {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|v| v.to_ascii_lowercase()) {
            Some(v) if v == "before" => PreviewMode::Before,
            Some(v) if v == "split" => PreviewMode::Split,
            _ => PreviewMode::After,
        }
    }
}

/// Frames handed over by `render_scene_frame` for the watched input.
pub(crate) struct StudioTap {
    input_id: String,
    graded: Frame,
    raw: Option<Frame>,
    at: Instant,
}

/// One LUT in the user's library.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LutInfo {
    /// File name inside the library (stable id).
    pub id: String,
    /// Display name: the file's `TITLE`, else the file stem.
    pub name: String,
    /// Absolute path (what `color_load_lut` receives).
    pub path: String,
    /// Non-fatal notes from the parser (resampled size, shaper, …).
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// One saved color preset.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColorPresetInfo {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
struct ColorPresetFile {
    version: u32,
    name: String,
    grade: ColorGrade,
    #[serde(default)]
    slots: Vec<FilterSlot>,
}

/// Reads and parses a LUT file; returns its parse result or a clear,
/// user-facing error (unsupported format, unreadable, malformed).
pub(crate) fn validate_lut_file(path: &str) -> Result<sf_color::lut::CubeFile, ShellError> {
    let p = Path::new(path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "cube" => {}
        "3dl" | "look" | "csp" | "lut" | "mga" | "m3d" | "vf" | "itx" | "icc" => {
            return Err(ShellError::Lut(format!(
                ".{ext} LUTs are not supported — StreamFlow reads the industry-standard .cube format. \
                 Export the LUT as .cube (Premiere Pro: Lumetri Color ▸ ⋯ ▸ Export .cube; Resolve: right-click the grade ▸ Generate LUT ▸ 33 Point Cube)."
            )))
        }
        _ => {
            return Err(ShellError::Lut(format!(
                "'{}' is not a .cube LUT file",
                p.file_name().and_then(|n| n.to_str()).unwrap_or(path)
            )))
        }
    }
    let bytes = std::fs::read(p).map_err(|e| ShellError::Lut(format!("cannot read LUT: {e}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ShellError::Lut("this .cube file is not text (binary or wrong encoding)".into()))?;
    sf_color::lut::parse_cube(&text)
        .map_err(|e| ShellError::Lut(format!("invalid .cube LUT: {e}")))
}

/// Lowercase, filesystem-safe stem.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if (ch == ' ' || ch == '-' || ch == '_') && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

/// `TITLE "…"` from the first lines of a cube file, without a full parse.
fn cube_title(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines().take(50) {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("TITLE") {
            let t = rest.trim().trim_matches('"').trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

impl Shell {
    /// Override the per-user data folder (tests, portable installs).
    pub fn set_data_dir(&mut self, dir: impl Into<PathBuf>) {
        self.data_dir = dir.into();
    }

    fn lut_dir(&self) -> PathBuf {
        self.data_dir.join("luts")
    }

    fn preset_dir(&self) -> PathBuf {
        self.data_dir.join("presets")
    }

    // ------------------------------------------------------------------
    // LUT library
    // ------------------------------------------------------------------

    /// Imports a `.cube` into the LUT library: validated first (clear error
    /// on unsupported/malformed files), then copied so the grade keeps
    /// working if the original file moves.
    pub fn lut_import(&mut self, src: &str) -> Result<LutInfo, ShellError> {
        let cube = validate_lut_file(src)?;
        let src_path = Path::new(src);
        let stem = src_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("lut")
            .to_string();
        let dir = self.lut_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| ShellError::Lut(format!("cannot create LUT library: {e}")))?;
        let text = std::fs::read(src_path)
            .map_err(|e| ShellError::Lut(format!("cannot read LUT: {e}")))?;
        let base = slug(&stem);
        let mut dest = dir.join(format!("{base}.cube"));
        let mut n = 2;
        while dest.exists() {
            // Re-importing the identical file is a no-op, not a duplicate.
            if std::fs::read(&dest).map(|d| d == text).unwrap_or(false) {
                break;
            }
            dest = dir.join(format!("{base}-{n}.cube"));
            n += 1;
        }
        std::fs::write(&dest, &text)
            .map_err(|e| ShellError::Lut(format!("cannot save LUT: {e}")))?;
        let path = dest.to_string_lossy().to_string();
        self.engine.invalidate_lut(&path);
        Ok(LutInfo {
            id: dest
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string(),
            name: cube.title.clone().filter(|t| !t.is_empty()).unwrap_or(stem),
            path,
            warnings: cube.warnings,
        })
    }

    /// LUTs in the library, sorted by name.
    pub fn lut_library(&self) -> Vec<LutInfo> {
        let mut out: Vec<LutInfo> = std::fs::read_dir(self.lut_dir())
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.eq_ignore_ascii_case("cube"))
                            .unwrap_or(false)
                    })
                    .map(|p| {
                        let id = p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .to_string();
                        let stem = p
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .to_string();
                        LutInfo {
                            name: cube_title(&p).unwrap_or(stem),
                            path: p.to_string_lossy().to_string(),
                            id,
                            warnings: Vec::new(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        out
    }

    /// Deletes a LUT from the library and detaches it from any input.
    pub fn lut_remove(&mut self, id: &str) -> Result<(), ShellError> {
        if id.contains('/') || id.contains('\\') || id.contains("..") {
            return Err(ShellError::Lut("invalid LUT id".into()));
        }
        let path = self.lut_dir().join(id);
        let path_s = path.to_string_lossy().to_string();
        std::fs::remove_file(&path).map_err(|e| ShellError::Lut(format!("cannot remove LUT: {e}")))?;
        self.engine.invalidate_lut(&path_s);
        let ids: Vec<String> = self
            .engine
            .state()
            .inputs
            .iter()
            .filter(|i| i.color.grade.lut.as_ref().map(|l| l.file == path_s).unwrap_or(false))
            .map(|i| i.id.clone())
            .collect();
        for input_id in ids {
            if let Some(c) = self.engine.state_mut().input_color_mut(&input_id) {
                c.grade.lut = None;
            }
            self.events.push_back(UiEvent::ColorChanged { input_id });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Color presets
    // ------------------------------------------------------------------

    /// Saves the input's complete look under `name` (same name overwrites).
    pub fn color_preset_save(&mut self, input_id: &str, name: &str) -> Result<ColorPresetInfo, ShellError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ShellError::Preset("preset name is empty".into()));
        }
        let grade = self.grade_snapshot(input_id)?;
        let slots = self
            .stack(&FilterTarget::Input(input_id.to_string()))?
            .slots()
            .to_vec();
        let file = ColorPresetFile {
            version: 1,
            name: name.to_string(),
            grade,
            slots,
        };
        let dir = self.preset_dir();
        std::fs::create_dir_all(&dir).map_err(|e| ShellError::Preset(e.to_string()))?;
        let id = slug(name);
        let json = serde_json::to_string_pretty(&file).map_err(|e| ShellError::Preset(e.to_string()))?;
        std::fs::write(dir.join(format!("{id}.sfpreset")), json)
            .map_err(|e| ShellError::Preset(e.to_string()))?;
        Ok(ColorPresetInfo {
            id,
            name: name.to_string(),
        })
    }

    /// Saved presets, sorted by name.
    pub fn color_preset_list(&self) -> Vec<ColorPresetInfo> {
        let mut out: Vec<ColorPresetInfo> = std::fs::read_dir(self.preset_dir())
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sfpreset"))
                    .filter_map(|p| {
                        let id = p.file_stem()?.to_str()?.to_string();
                        let text = std::fs::read_to_string(&p).ok()?;
                        let f: ColorPresetFile = serde_json::from_str(&text).ok()?;
                        Some(ColorPresetInfo { id, name: f.name })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        out
    }

    /// Applies a saved preset to an input: replaces its grade and filter
    /// stack (fresh slot ids). Non-destructive — the camera image is never
    /// modified; everything stays editable/resettable afterwards.
    pub fn color_preset_apply(&mut self, input_id: &str, id: &str) -> Result<ColorGrade, ShellError> {
        if id.contains('/') || id.contains('\\') || id.contains("..") {
            return Err(ShellError::Preset("invalid preset id".into()));
        }
        let text = std::fs::read_to_string(self.preset_dir().join(format!("{id}.sfpreset")))
            .map_err(|e| ShellError::Preset(format!("cannot read preset: {e}")))?;
        let file: ColorPresetFile =
            serde_json::from_str(&text).map_err(|e| ShellError::Preset(format!("corrupt preset: {e}")))?;
        let mut stack = FilterStack::new();
        for mut slot in file.slots {
            slot.id = Uuid::new_v4();
            let _ = stack.push(slot); // skip anything over the stack limits
        }
        if let Some(l) = &file.grade.lut {
            self.engine.invalidate_lut(&l.file);
        }
        *self.grade_mut(input_id)? = file.grade.clone();
        *self.stack_mut(&FilterTarget::Input(input_id.to_string()))? = stack;
        self.events.push_back(UiEvent::ColorChanged {
            input_id: input_id.to_string(),
        });
        self.events.push_back(UiEvent::FilterStackChanged {
            target: FilterTarget::Input(input_id.to_string()),
        });
        Ok(file.grade)
    }

    pub fn color_preset_delete(&mut self, id: &str) -> Result<(), ShellError> {
        if id.contains('/') || id.contains('\\') || id.contains("..") {
            return Err(ShellError::Preset("invalid preset id".into()));
        }
        std::fs::remove_file(self.preset_dir().join(format!("{id}.sfpreset")))
            .map_err(|e| ShellError::Preset(format!("cannot delete preset: {e}")))
    }

    // ------------------------------------------------------------------
    // Live preview tap
    // ------------------------------------------------------------------

    /// True while the Color Studio is polling `input_id` — the tick then
    /// keeps the frames it already produced instead of dropping them.
    pub(crate) fn studio_watching(&self, input_id: &str) -> Option<PreviewMode> {
        match &self.studio_watch {
            Some((id, at, mode)) if id == input_id && at.elapsed() < WATCH_TIMEOUT => Some(*mode),
            _ => None,
        }
    }

    /// Called by `render_scene_frame` with frames it owns and was about to
    /// drop: zero extra GPU work and zero pixel copies.
    pub(crate) fn studio_tap_store(&mut self, input_id: &str, graded: Frame, raw: Option<Frame>) {
        self.studio_tap = Some(StudioTap {
            input_id: input_id.to_string(),
            graded,
            raw,
            at: Instant::now(),
        });
    }

    /// `preview.get_frame` — JPEG of the input as the Color Studio shows it.
    ///
    /// Serves the live tap (the exact frame going to preview/encoder) when
    /// the input is on air or in the preview scene; otherwise grades the
    /// latest frame on demand (throttled like before).
    pub fn preview_frame_jpeg(
        &mut self,
        input_id: &str,
        width: u32,
        height: u32,
        mode: PreviewMode,
    ) -> Result<Vec<u8>, ShellError> {
        let frame = self.preview_frame(input_id, width, height, mode)?;
        encode_preview_jpeg(&frame)
    }

    /// The preview frame itself (RGBA). Hosts should call this under the
    /// shell lock and JPEG-encode with [`encode_preview_jpeg`] *after*
    /// releasing it, so encoding never stalls the media tick.
    pub fn preview_frame(
        &mut self,
        input_id: &str,
        width: u32,
        height: u32,
        mode: PreviewMode,
    ) -> Result<Frame, ShellError> {
        let (w, h) = (width.clamp(16, 1920), height.clamp(16, 1080));
        self.studio_watch = Some((input_id.to_string(), Instant::now(), mode));

        let fresh_tap = self
            .studio_tap
            .as_ref()
            .filter(|t| t.input_id == input_id && t.at.elapsed() < TAP_FRESH);
        let (graded, raw): (Frame, Option<Frame>) = match fresh_tap {
            Some(t) => (
                scale_to(&t.graded, w, h),
                t.raw.as_ref().map(|r| scale_to(r, w, h)),
            ),
            None => {
                // Not on air: grade on demand at preview size.
                if self.engine.state().input_color(input_id).is_none() {
                    return Err(ShellError::UnknownInput(input_id.to_string()));
                }
                let pixels = self
                    .real_preview_frame(input_id, w, h)
                    .unwrap_or_else(|| super::synthetic_frame(w, h));
                self.ingest_frame(input_id, w, h, pixels.clone());
                let graded = self.engine.grade_input_frame(input_id, w, h, &pixels)?;
                (
                    graded,
                    Some(Frame {
                        width: w,
                        height: h,
                        pixels,
                    }),
                )
            }
        };
        Ok(match (mode, raw) {
            (PreviewMode::Before, Some(r)) => r,
            (PreviewMode::Split, Some(r)) => split_frame(&r, &graded),
            _ => graded,
        })
    }
}

/// JPEG for a preview frame (call outside the shell lock).
pub fn encode_preview_jpeg(frame: &Frame) -> Result<Vec<u8>, ShellError> {
    encode_jpeg(frame, 85)
}

fn scale_to(f: &Frame, w: u32, h: u32) -> Frame {
    if f.width == w && f.height == h {
        return Frame {
            width: w,
            height: h,
            pixels: f.pixels.clone(),
        };
    }
    Frame {
        width: w,
        height: h,
        pixels: downscale_bilinear(&f.pixels, f.width, f.height, w, h),
    }
}

/// Left half `before`, right half `after`, with a thin white divider.
fn split_frame(before: &Frame, after: &Frame) -> Frame {
    let (w, h) = (after.width as usize, after.height as usize);
    let mut px = after.pixels.clone();
    if before.width as usize != w || before.height as usize != h {
        return Frame {
            width: after.width,
            height: after.height,
            pixels: px,
        };
    }
    let mid = w / 2;
    for y in 0..h {
        let row = y * w * 4;
        px[row..row + mid * 4].copy_from_slice(&before.pixels[row..row + mid * 4]);
        for x in mid.saturating_sub(1)..(mid + 1).min(w) {
            let i = row + x * 4;
            px[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    Frame {
        width: after.width,
        height: after.height,
        pixels: px,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_filesystem_safe() {
        assert_eq!(slug("My Cinematic LUT!"), "my-cinematic-lut");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("  "), "untitled");
    }

    #[test]
    fn preview_mode_parses() {
        assert_eq!(PreviewMode::parse(Some("Split")), PreviewMode::Split);
        assert_eq!(PreviewMode::parse(Some("before")), PreviewMode::Before);
        assert_eq!(PreviewMode::parse(None), PreviewMode::After);
    }

    #[test]
    fn split_puts_before_on_the_left() {
        let before = Frame { width: 4, height: 1, pixels: vec![10; 16] };
        let after = Frame { width: 4, height: 1, pixels: vec![200; 16] };
        let s = split_frame(&before, &after);
        assert_eq!(s.pixels[0], 10);
        assert_eq!(s.pixels[15], 200);
    }

    #[test]
    fn unsupported_lut_extensions_are_explained() {
        let e = validate_lut_file("C:/luts/film.3dl").unwrap_err().to_string();
        assert!(e.contains(".3dl") && e.contains(".cube"), "{e}");
        let e = validate_lut_file("look.look").unwrap_err().to_string();
        assert!(e.contains("Export"), "{e}");
    }
}
