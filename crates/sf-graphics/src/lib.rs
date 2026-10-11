//! `sf-graphics` — 2D graphics helpers for StreamFlow Lite.
//!
//! Currently: text rasterization for `Text` / lower-third sources.
//! Uses `fontdue` (pure Rust, no system dependencies) with the bundled
//! DejaVu Sans (open license, `fonts/DejaVuSans.ttf`).

use std::sync::OnceLock;

static FONT: OnceLock<fontdue::Font> = OnceLock::new();

fn font() -> &'static fontdue::Font {
    FONT.get_or_init(|| {
        let data = include_bytes!("../fonts/DejaVuSans.ttf");
        fontdue::Font::from_bytes(data as &[u8], fontdue::FontSettings::default())
            .expect("bundled DejaVuSans.ttf must parse")
    })
}

/// Render `text` centered on a transparent RGBA canvas (`width` × `height`).
///
/// The font size is derived from the canvas height (`height / 12`, clamped
/// to 12..96 px) so a lower-third stays proportional at any output
/// resolution. Returns white RGBA8 with per-pixel alpha from glyph
/// coverage (straight alpha).
pub fn render_text_rgba(text: &str, width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let mut canvas = vec![0u8; w * h * 4];
    let text = text.trim();
    if text.is_empty() {
        return canvas;
    }
    let font = font();
    let px = (h as f32 / 12.0).clamp(12.0, 96.0);
    // Rasterize each glyph first so we can center the whole line.
    struct G {
        metrics: fontdue::Metrics,
        bitmap: Vec<u8>,
    }
    let mut glyphs: Vec<G> = Vec::new();
    let mut total_w = 0i32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px);
        total_w += metrics.advance_width as i32;
        glyphs.push(G { metrics, bitmap });
    }
    if glyphs.is_empty() {
        return canvas;
    }
    let mut x = (w as i32 - total_w) / 2;
    // Baseline vertically centered (cap-height approximation).
    let y_baseline = h as i32 / 2 + (px * 0.35) as i32;
    for g in &glyphs {
        let m = &g.metrics;
        let gw = m.width as i32;
        let gh = m.height as i32;
        let gx = x + m.bounds.xmin as i32;
        // Bitmap top row sits at (ymin + height) above the baseline.
        let gy = y_baseline - (m.bounds.ymin + m.height as f32) as i32;
        for row in 0..gh {
            for col in 0..gw {
                let dx = gx + col;
                let dy = gy + row;
                if dx < 0 || dy < 0 || dx >= w as i32 || dy >= h as i32 {
                    continue;
                }
                let coverage = g.bitmap[(row * gw + col) as usize];
                if coverage == 0 {
                    continue;
                }
                let d = (dy as usize * w + dx as usize) * 4;
                canvas[d] = 255;
                canvas[d + 1] = 255;
                canvas[d + 2] = 255;
                if coverage > canvas[d + 3] {
                    canvas[d + 3] = coverage;
                }
            }
        }
        x += m.advance_width as i32;
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_renders_nonempty_alpha() {
        let px = render_text_rgba("Hello", 640, 360);
        assert_eq!(px.len(), 640 * 360 * 4);
        let alpha: u32 = px.chunks_exact(4).map(|c| c[3] as u32).sum();
        assert!(
            alpha > 1000,
            "expected visible glyphs, got alpha sum {alpha}"
        );
    }

    #[test]
    fn empty_text_is_transparent() {
        let px = render_text_rgba("   ", 320, 180);
        assert!(px.chunks_exact(4).all(|c| c[3] == 0));
    }
}
