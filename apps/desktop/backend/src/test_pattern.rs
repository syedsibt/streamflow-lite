//! Diagnostic test signal (the `TestPattern` source).
//!
//! Every element answers one question about the pipeline:
//!
//! | Element | What it shows |
//! |---|---|
//! | 75 % colour bars | colour/LUT/grade path: hues and saturation survive |
//! | near-black steps 0…10 | black crush (a grade/encoder that clips shadows merges them) |
//! | near-white steps 245…255 | highlight clipping |
//! | 11-step grey scale | gamma / contrast / range (full vs limited) mismatch |
//! | smooth ramp | banding from 8-bit grading or low bitrate |
//! | 1-px checker / line pairs | scaling: a resize blurs them to flat grey; moiré = bad scaler |
//! | 1-px border + circle | crop/overscan (border must be visible on all 4 sides); aspect (circle must stay round) |
//! | moving bar (2 s sweep) | judder / dropped frames: smooth motion, any jump = a missed frame |
//! | 10 frame boxes | one box lights per output frame; a skipped box in a recording = a dropped frame |
//! | timecode HH:MM:SS.mmm + frame no. | latency: photograph the app preview next to the stream player — the timecode difference is the end-to-end delay |
//!
//! The static part is generated once per size and cached; each frame only
//! copies it and redraws the small dynamic regions.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub(crate) struct TestPatternCache {
    base: Option<(u32, u32, Arc<Vec<u8>>)>,
}

impl TestPatternCache {
    pub(crate) fn entries(&self) -> usize {
        self.base.is_some() as usize
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.base.as_ref().map(|(_, _, p)| p.len() as u64).unwrap_or(0)
    }

    /// The test signal at `w`×`h` for wall-clock time `now` and output
    /// frame rate `fps`.
    pub(crate) fn frame(&mut self, w: u32, h: u32, fps: u32, now: SystemTime) -> Vec<u8> {
        let (w, h) = (w.max(1), h.max(1));
        let base = match &self.base {
            Some((bw, bh, px)) if *bw == w && *bh == h => px.clone(),
            _ => {
                let px = Arc::new(static_pattern(w, h));
                self.base = Some((w, h, px.clone()));
                px
            }
        };
        let mut px = (*base).clone();
        draw_dynamic(&mut px, w, h, fps.max(1), now);
        px
    }
}

const GREY_BG: [u8; 3] = [24, 24, 24];

struct Canvas<'a> {
    px: &'a mut [u8],
    w: i64,
    h: i64,
}

impl Canvas<'_> {
    fn rect(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, c: [u8; 3]) {
        let (x0, x1) = (x0.clamp(0, self.w), x1.clamp(0, self.w));
        let (y0, y1) = (y0.clamp(0, self.h), y1.clamp(0, self.h));
        for y in y0..y1 {
            let row = (y * self.w) as usize * 4;
            for x in x0..x1 {
                let i = row + x as usize * 4;
                self.px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
    }
    fn put(&mut self, x: i64, y: i64, c: [u8; 3]) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            let i = (y * self.w + x) as usize * 4;
            self.px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
}

/// Section boundaries as fractions of the height.
const BARS_END: f32 = 0.48;
const STEPS_END: f32 = 0.56;
const GREY_END: f32 = 0.66;
const RAMP_END: f32 = 0.72;

fn static_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut px = vec![0u8; w as usize * h as usize * 4];
    let mut c = Canvas {
        px: &mut px,
        w: w as i64,
        h: h as i64,
    };
    let (wf, hf) = (w as f32, h as f32);
    let y = |f: f32| (f * hf).round() as i64;
    let x = |f: f32| (f * wf).round() as i64;

    // 75 % SMPTE-order bars: grey, yellow, cyan, green, magenta, red, blue.
    const V: u8 = 191;
    let bars = [
        [V, V, V],
        [V, V, 0],
        [0, V, V],
        [0, V, 0],
        [V, 0, V],
        [V, 0, 0],
        [0, 0, V],
    ];
    for (i, col) in bars.iter().enumerate() {
        c.rect(
            x(i as f32 / 7.0),
            0,
            x((i + 1) as f32 / 7.0),
            y(BARS_END),
            *col,
        );
    }

    // Near-black (left half) and near-white (right half) steps.
    let blacks = [0u8, 2, 4, 6, 8, 10];
    let whites = [245u8, 247, 249, 251, 253, 255];
    for (i, v) in blacks.iter().enumerate() {
        c.rect(
            x(i as f32 / 12.0),
            y(BARS_END),
            x((i + 1) as f32 / 12.0),
            y(STEPS_END),
            [*v; 3],
        );
    }
    for (i, v) in whites.iter().enumerate() {
        let i = i + 6;
        c.rect(
            x(i as f32 / 12.0),
            y(BARS_END),
            x((i + 1) as f32 / 12.0),
            y(STEPS_END),
            [*v; 3],
        );
    }

    // 11-step grey scale 0 … 100 %.
    for i in 0..11 {
        let v = (i as f32 * 25.5).round() as u8;
        c.rect(
            x(i as f32 / 11.0),
            y(STEPS_END),
            x((i + 1) as f32 / 11.0),
            y(GREY_END),
            [v; 3],
        );
    }

    // Smooth ramp (banding).
    for xx in 0..w as i64 {
        let v = (xx as f32 / (wf - 1.0) * 255.0).round() as u8;
        c.rect(xx, y(GREY_END), xx + 1, y(RAMP_END), [v; 3]);
    }

    // Bottom area.
    c.rect(0, y(RAMP_END), w as i64, h as i64, GREY_BG);

    // Resolution patches (left of the bottom area): 1-px checker, 1-px and
    // 2-px vertical line pairs, 1-px horizontal lines.
    let ph = ((hf * 0.09) as i64).max(4);
    let pw = ph;
    let py0 = y(RAMP_END) + (hf * 0.025) as i64;
    let px0 = (wf * 0.02) as i64;
    for (k, kind) in [0, 1, 2, 3].iter().enumerate() {
        let ox = px0 + k as i64 * (pw + pw / 4);
        for yy in 0..ph {
            for xx in 0..pw {
                let on = match kind {
                    0 => (xx + yy) % 2 == 0,
                    1 => xx % 2 == 0,
                    2 => (xx / 2) % 2 == 0,
                    _ => yy % 2 == 0,
                };
                c.put(ox + xx, py0 + yy, if on { [255; 3] } else { [0; 3] });
            }
        }
    }

    // Circle (aspect) centred on the frame, 2 px, white.
    let (cx, cy) = (wf / 2.0, hf / 2.0);
    let r = hf * 0.42;
    let steps = (r * 8.0) as i64;
    for s in 0..steps {
        let a = s as f32 / steps as f32 * std::f32::consts::TAU;
        for dr in [0.0, 1.0] {
            c.put(
                (cx + (r - dr) * a.cos()) as i64,
                (cy + (r - dr) * a.sin()) as i64,
                [255; 3],
            );
        }
    }
    // Centre cross.
    let arm = (hf * 0.04) as i64;
    c.rect(
        cx as i64 - arm,
        cy as i64,
        cx as i64 + arm + 1,
        cy as i64 + 1,
        [255; 3],
    );
    c.rect(
        cx as i64,
        cy as i64 - arm,
        cx as i64 + 1,
        cy as i64 + arm + 1,
        [255; 3],
    );

    // 1-px border (crop / overscan check).
    let (wi, hi) = (w as i64, h as i64);
    c.rect(0, 0, wi, 1, [255; 3]);
    c.rect(0, hi - 1, wi, hi, [255; 3]);
    c.rect(0, 0, 1, hi, [255; 3]);
    c.rect(wi - 1, 0, wi, hi, [255; 3]);

    // Static resolution readout under the patches.
    let dh = ((hf * 0.035) as i64).max(7);
    let text = format!("{w}x{h}");
    draw_text(&mut c, px0, py0 + ph + dh / 2, dh, &text, [200; 3]);
    px
}

fn draw_dynamic(px: &mut [u8], w: u32, h: u32, fps: u32, now: SystemTime) {
    let mut c = Canvas {
        px,
        w: w as i64,
        h: h as i64,
    };
    let (wf, hf) = (w as f32, h as f32);
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let ms_total = since.as_millis() as u64;
    let day_ms = ms_total % 86_400_000;
    let frame_no = (since.as_micros() as u64 * fps as u64 / 1_000_000) as i64;

    // Timecode + frame number (centre-right of the bottom area).
    let dh = ((hf * 0.07) as i64).max(9);
    let tx = (wf * 0.36) as i64;
    let ty = (hf * (RAMP_END + 0.03)) as i64;
    let tc = format!(
        "{:02}:{:02}:{:02}.{:03}",
        day_ms / 3_600_000,
        day_ms / 60_000 % 60,
        day_ms / 1000 % 60,
        day_ms % 1000
    );
    draw_text(&mut c, tx, ty, dh, &tc, [255; 3]);
    let fh = dh * 6 / 10;
    let fnum = format!("{:06}", frame_no.rem_euclid(1_000_000));
    draw_text(&mut c, tx, ty + dh + dh / 3, fh, &fnum, [180; 3]);

    // Ten frame boxes: box (frame mod 10) is lit.
    let bs = ((hf * 0.035) as i64).max(4);
    let bx0 = tx + text_width(fh, &fnum) + bs;
    let by = ty + dh + dh / 3 + (fh - bs) / 2;
    for i in 0..10 {
        let lit = frame_no.rem_euclid(10) == i;
        let x0 = bx0 + i * (bs + bs / 3);
        c.rect(
            x0,
            by,
            x0 + bs,
            by + bs,
            if lit { [255, 255, 255] } else { [60; 3] },
        );
    }

    // Sweep bar: crosses the bottom strip every 2 s at constant speed.
    let sy0 = (hf * 0.93) as i64;
    let sy1 = h as i64 - 2;
    let period_ms = 2000u64;
    let bw = ((wf / 60.0) as i64).max(2);
    let pos = (ms_total % period_ms) as f32 / period_ms as f32;
    let bx = 1 + (pos * (wf - 2.0 - bw as f32)) as i64;
    c.rect(1, sy0, w as i64 - 1, sy1, [40; 3]);
    c.rect(bx, sy0, bx + bw, sy1, [255, 210, 0]);
}

// ---------------------------------------------------------------------------
// Seven-segment digits (no font dependency; crisp at any size)
// ---------------------------------------------------------------------------

/// Segment bits: a b c d e f g (top, top-right, bottom-right, bottom,
/// bottom-left, top-left, middle).
fn segments(ch: char) -> u8 {
    match ch {
        '0' => 0b1111110,
        '1' => 0b0110000,
        '2' => 0b1101101,
        '3' => 0b1111001,
        '4' => 0b0110011,
        '5' => 0b1011011,
        '6' => 0b1011111,
        '7' => 0b1110000,
        '8' => 0b1111111,
        '9' => 0b1111011,
        _ => 0,
    }
}

fn char_width(h: i64, ch: char) -> i64 {
    match ch {
        ':' | '.' => h / 4,
        'x' => h * 4 / 10,
        _ => h * 6 / 10,
    }
}

fn text_width(h: i64, s: &str) -> i64 {
    s.chars().map(|ch| char_width(h, ch) + h / 8).sum()
}

fn draw_text(c: &mut Canvas<'_>, mut x: i64, y: i64, h: i64, s: &str, col: [u8; 3]) {
    for ch in s.chars() {
        draw_char(c, x, y, h, ch, col);
        x += char_width(h, ch) + h / 8;
    }
}

fn draw_char(c: &mut Canvas<'_>, x: i64, y: i64, h: i64, ch: char, col: [u8; 3]) {
    let t = (h / 9).max(1);
    match ch {
        ':' => {
            let cx = x + h / 8 - t / 2;
            c.rect(cx, y + h / 3 - t, cx + t * 2, y + h / 3 + t, col);
            c.rect(cx, y + 2 * h / 3 - t, cx + t * 2, y + 2 * h / 3 + t, col);
            return;
        }
        '.' => {
            let cx = x + h / 8 - t / 2;
            c.rect(cx, y + h - t * 2, cx + t * 2, y + h, col);
            return;
        }
        'x' => {
            let wdt = char_width(h, ch);
            let top = y + h / 3;
            let n = h - h / 3;
            for i in 0..n {
                let dx = i * wdt / n.max(1);
                c.rect(x + dx, top + i, x + dx + t, top + i + 1, col);
                c.rect(x + wdt - dx - t, top + i, x + wdt - dx, top + i + 1, col);
            }
            return;
        }
        _ => {}
    }
    let s = segments(ch);
    let wdt = char_width(h, ch);
    let mid = y + h / 2;
    let on = |bit: u8| s & (1 << (6 - bit)) != 0;
    if on(0) {
        c.rect(x, y, x + wdt, y + t, col);
    }
    if on(1) {
        c.rect(x + wdt - t, y, x + wdt, mid, col);
    }
    if on(2) {
        c.rect(x + wdt - t, mid, x + wdt, y + h, col);
    }
    if on(3) {
        c.rect(x, y + h - t, x + wdt, y + h, col);
    }
    if on(4) {
        c.rect(x, mid, x + t, y + h, col);
    }
    if on(5) {
        c.rect(x, y, x + t, mid, col);
    }
    if on(6) {
        c.rect(x, mid - t / 2, x + wdt, mid - t / 2 + t, col);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let i = (y * w + x) as usize * 4;
        [px[i], px[i + 1], px[i + 2]]
    }

    #[test]
    fn static_elements_are_exact() {
        let (w, h) = (1280, 720);
        let mut c = TestPatternCache::default();
        let px = c.frame(w, h, 30, UNIX_EPOCH);
        assert_eq!(px.len(), (w * h * 4) as usize);
        // Border.
        assert_eq!(at(&px, w, 0, 300), [255; 3]);
        assert_eq!(at(&px, w, w - 1, 300), [255; 3]);
        // 75 % yellow bar (2nd of 7), away from circle/cross.
        assert_eq!(at(&px, w, w * 3 / 14, 20), [191, 191, 0]);
        // 2 % and 4 % near-black steps differ (crush check is meaningful).
        let yb = (h as f32 * 0.52) as u32;
        assert_eq!(at(&px, w, w / 12 + w / 24, yb), [2; 3]);
        assert_eq!(at(&px, w, 2 * w / 12 + w / 24, yb), [4; 3]);
        // Ramp is monotonic.
        let yr = (h as f32 * 0.69) as u32;
        assert!(at(&px, w, 10, yr)[0] < at(&px, w, w - 10, yr)[0]);
    }

    #[test]
    fn sweep_and_frame_boxes_move_with_time() {
        let (w, h) = (640, 360);
        let mut c = TestPatternCache::default();
        let a = c.frame(w, h, 30, UNIX_EPOCH + Duration::from_millis(100));
        let b = c.frame(w, h, 30, UNIX_EPOCH + Duration::from_millis(1100));
        assert_ne!(a, b, "dynamic elements must change over time");
        if let Ok(path) = std::env::var("SF_PATTERN_PNG") {
            let f = c.frame(
                1920,
                1080,
                30,
                UNIX_EPOCH + Duration::from_millis(45_296_789),
            );
            image::save_buffer(&path, &f, 1920, 1080, image::ExtendedColorType::Rgba8).unwrap();
        }
        // Same time → identical frame (deterministic, no hidden state).
        let a2 = c.frame(w, h, 30, UNIX_EPOCH + Duration::from_millis(100));
        assert_eq!(a, a2);
    }

    #[test]
    fn any_size_returns_exact_buffer() {
        let mut c = TestPatternCache::default();
        for (w, h) in [(1, 1), (3, 2), (15, 9), (17, 1), (1, 40)] {
            assert_eq!(c.frame(w, h, 30, SystemTime::now()).len(), (w * h * 4) as usize);
        }
    }

    #[test]
    fn per_frame_cost_is_small() {
        let mut c = TestPatternCache::default();
        let _ = c.frame(1920, 1080, 60, SystemTime::now());
        let t = std::time::Instant::now();
        for _ in 0..10 {
            let _ = c.frame(1920, 1080, 60, SystemTime::now());
        }
        let per = t.elapsed() / 10;
        eprintln!("test pattern 1080p per frame: {per:?}");
        // Generous bound for debug builds on slow CI.
        assert!(per < Duration::from_millis(250), "{per:?}");
    }
}
