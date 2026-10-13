//! `.cube` LUT parser (LUT tab) — Adobe Cube LUT Specification 1.0 plus the
//! DaVinci Resolve dialect, i.e. the files Premiere Pro / Lumetri, Resolve,
//! Final Cut and most LUT vendors export.
//!
//! Supported:
//! * 3D LUTs (`LUT_3D_SIZE` 2..=256) and 1D LUTs (`LUT_1D_SIZE` 2..=65536).
//! * **Input domain**: `DOMAIN_MIN`/`DOMAIN_MAX` (Adobe) and
//!   `LUT_1D_INPUT_RANGE`/`LUT_3D_INPUT_RANGE` (Resolve). These map the
//!   *input* signal onto the table; output values are used as written.
//!   (An earlier version wrongly rescaled the output values.)
//! * **Shaper + 3D** (Resolve): a file carrying both a 1D and a 3D table
//!   applies the 1D shaper first, then the 3D cube — both are honored.
//!
//! Everything is baked onto a 33^3 RGBA lattice on load, so the GPU always
//! uploads the same 33^3 `rgba16float` texture: 33 is kept as-is; 17/25/65
//! (the common export sizes) are resampled silently; other sizes are
//! resampled with a warning.
//!
//! Not supported (clear error): `.3dl` (Autodesk/Lustre), `.look` (Adobe
//! SpeedGrade XML), `.csp`, `.lut`, binary files — re-export those as
//! `.cube` from Premiere/Resolve.
//!
//! 3D data order in the file is red-fastest: `index = r + n * (g + n * b)`,
//! which matches the storage order of [`CubeFile::data`].
//!
//! Color space note: `.cube` creative LUTs (Lumetri "Creative" looks, film
//! emulations) expect **display-referred** (Rec.709 gamma-encoded) input.
//! The grade pass therefore samples the LUT on gamma-encoded values, not on
//! the linear working space.

/// Error from [`parse_cube`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LutParseError {
    /// The input had no content at all.
    #[error("empty file")]
    Empty,
    /// Neither `LUT_1D_SIZE` nor `LUT_3D_SIZE` was found.
    #[error("missing LUT_1D_SIZE/LUT_3D_SIZE")]
    MissingSize,
    /// A data line on the given 1-based line number was not three numbers.
    #[error("line {0}: bad number")]
    BadNumber(usize),
    /// The data section did not contain exactly the expected value count.
    #[error("expected {expected} data values, got {got}")]
    WrongDataCount { expected: usize, got: usize },
    /// A known keyword line on the given 1-based line number was malformed.
    #[error("line {0}: {1}")]
    BadKeyword(usize, String),
    /// The file is a LUT format other than `.cube`.
    #[error("{0}")]
    UnsupportedFormat(String),
}

/// A parsed `.cube` file, normalized and ready for GPU upload.
///
/// `data` always holds 33^3 RGBA entries (`a = 1.0`) in red-fastest order.
#[derive(Clone, Debug)]
pub struct CubeFile {
    /// `TITLE "..."` value, if present.
    pub title: Option<String>,
    /// Lattice size per axis. Always 33 after normalization.
    pub size: usize,
    /// `size^3` entries, RGBA with `a = 1.0`; RGB are output values
    /// normalized to [0, 1] via the domain mapping.
    pub data: Vec<[f32; 4]>,
    /// Non-fatal issues (unknown keywords, resampled sizes).
    pub warnings: Vec<String>,
}

/// 3D sizes the LUT tab accepts natively (resampled to 33^3 without warning).
const NATIVE_SIZES: [usize; 6] = [16, 17, 25, 32, 33, 65];
/// Largest accepted 3D size (256^3 ≈ 16.7 M entries).
const MAX_3D_SIZE: usize = 256;
/// Largest accepted 1D size.
const MAX_1D_SIZE: usize = 65_536;
/// Normalized lattice size everything is converted to.
const NORMALIZED_SIZE: usize = 33;

fn parse_title(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('"') {
            return after[..end].to_string();
        }
        return after.to_string();
    }
    rest.to_string()
}

fn parse_size_arg(
    parts: &mut std::str::SplitWhitespace<'_>,
    lineno: usize,
    kw: &str,
) -> Result<usize, LutParseError> {
    let bad = || LutParseError::BadKeyword(lineno, format!("{kw} expects an integer size"));
    let n: usize = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if n < 2 {
        return Err(LutParseError::BadKeyword(
            lineno,
            format!("{kw}: LUT size must be >= 2, got {n}"),
        ));
    }
    Ok(n)
}

fn parse_domain(
    parts: &mut std::str::SplitWhitespace<'_>,
    lineno: usize,
    kw: &str,
) -> Result<[f32; 3], LutParseError> {
    let bad = || LutParseError::BadKeyword(lineno, format!("{kw} expects 3 numbers"));
    let mut v = [0.0f32; 3];
    for c in 0..3 {
        v[c] = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    }
    Ok(v)
}

/// Parse `.cube` file text into a normalized [`CubeFile`].
///
/// `#` starts a comment (whole-line, or trailing on non-`TITLE` lines).
/// Unknown keywords are ignored with a warning pushed to
/// [`CubeFile::warnings`].
pub fn parse_cube(text: &str) -> Result<CubeFile, LutParseError> {
    if text.trim().is_empty() {
        return Err(LutParseError::Empty);
    }
    detect_foreign_format(text)?;

    let mut title: Option<String> = None;
    let mut size_1d: Option<(usize, usize)> = None; // (size, lineno)
    let mut size_3d: Option<(usize, usize)> = None;
    // Input domains: shared (Adobe DOMAIN_*) unless a per-table Resolve
    // INPUT_RANGE overrides it.
    let mut dmin = [0.0f32; 3];
    let mut dmax = [1.0f32; 3];
    let mut range_1d: Option<(f32, f32)> = None;
    let mut range_3d: Option<(f32, f32)> = None;
    let mut warnings: Vec<String> = Vec::new();
    let mut raw: Vec<[f32; 3]> = Vec::new();

    for (idx, raw_line) in text.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // TITLE is handled before comment-stripping so `#` inside quotes survives.
        if line.starts_with("TITLE") {
            title = Some(parse_title(&line["TITLE".len()..]));
            continue;
        }
        // Strip trailing `#` comments on all other lines.
        let line = match line.find('#') {
            Some(i) => line[..i].trim(),
            None => line,
        };
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let first = parts.next().expect("non-empty line");
        match first {
            "LUT_1D_SIZE" => {
                let n = parse_size_arg(&mut parts, lineno, "LUT_1D_SIZE")?;
                size_1d = Some((n, lineno));
            }
            "LUT_3D_SIZE" => {
                let n = parse_size_arg(&mut parts, lineno, "LUT_3D_SIZE")?;
                size_3d = Some((n, lineno));
            }
            "DOMAIN_MIN" => dmin = parse_domain(&mut parts, lineno, "DOMAIN_MIN")?,
            "DOMAIN_MAX" => dmax = parse_domain(&mut parts, lineno, "DOMAIN_MAX")?,
            "LUT_1D_INPUT_RANGE" => {
                range_1d = Some(parse_range(&mut parts, lineno, "LUT_1D_INPUT_RANGE")?)
            }
            "LUT_3D_INPUT_RANGE" => {
                range_3d = Some(parse_range(&mut parts, lineno, "LUT_3D_INPUT_RANGE")?)
            }
            _ if first
                .chars()
                .next()
                .map_or(false, |c| c.is_alphabetic() || c == '_') =>
            {
                warnings.push(format!("unknown keyword '{first}' ignored (line {lineno})"));
            }
            _ => {
                // Data line: exactly three floats.
                let mut nums = [0.0f32; 3];
                let mut count = 0;
                for tok in line.split_whitespace() {
                    if count >= 3 {
                        return Err(LutParseError::BadNumber(lineno));
                    }
                    nums[count] = tok
                        .parse::<f32>()
                        .map_err(|_| LutParseError::BadNumber(lineno))?;
                    count += 1;
                }
                if count != 3 {
                    return Err(LutParseError::BadNumber(lineno));
                }
                raw.push(nums);
            }
        }
    }

    let n1 = size_1d.map(|(n, _)| n);
    let n3 = size_3d.map(|(n, _)| n);
    if n1.is_none() && n3.is_none() {
        return Err(LutParseError::MissingSize);
    }
    if let Some((n, line)) = size_3d {
        if n > MAX_3D_SIZE {
            return Err(LutParseError::BadKeyword(
                line,
                format!("LUT_3D_SIZE {n} is too large (max {MAX_3D_SIZE})"),
            ));
        }
    }
    if let Some((n, line)) = size_1d {
        if n > MAX_1D_SIZE {
            return Err(LutParseError::BadKeyword(
                line,
                format!("LUT_1D_SIZE {n} is too large (max {MAX_1D_SIZE})"),
            ));
        }
    }
    // Resolve shaper files list the 1D entries first, then the 3D cube.
    let expected = n1.unwrap_or(0) + n3.map_or(0, |n| n * n * n);
    if raw.len() != expected {
        return Err(LutParseError::WrongDataCount {
            expected,
            got: raw.len(),
        });
    }
    let (shaper, cube): (Option<&[[f32; 3]]>, Option<&[[f32; 3]]>) = match (n1, n3) {
        (Some(a), Some(_)) => {
            warnings.push("1D shaper + 3D LUT: shaper applied before the cube".to_string());
            (Some(&raw[..a]), Some(&raw[a..]))
        }
        (Some(_), None) => (Some(&raw[..]), None),
        (None, Some(_)) => (None, Some(&raw[..])),
        (None, None) => unreachable!(),
    };
    if let Some(n) = n3 {
        if !NATIVE_SIZES.contains(&n) {
            warnings.push(format!("unsupported LUT size {n}, resampled to 33"));
        }
    }
    let dom = |r: Option<(f32, f32)>| -> ([f32; 3], [f32; 3]) {
        match r {
            Some((lo, hi)) => ([lo; 3], [hi; 3]),
            None => (dmin, dmax),
        }
    };
    let (d1_lo, d1_hi) = dom(range_1d);
    let (d3_lo, d3_hi) = dom(range_3d);
    // Signal value -> normalized 0..1 table coordinate for a domain.
    let to_unit = |v: f32, lo: f32, hi: f32| -> f32 {
        let span = hi - lo;
        if span.abs() < 1e-9 {
            0.0
        } else {
            ((v - lo) / span).clamp(0.0, 1.0)
        }
    };
    let lerp1d = |table: &[[f32; 3]], c: usize, u: f32| -> f32 {
        let n = table.len();
        let pos = u.clamp(0.0, 1.0) * (n as f32 - 1.0);
        let i0 = (pos.floor() as usize).min(n - 2);
        let f = pos - i0 as f32;
        table[i0][c] + (table[i0 + 1][c] - table[i0][c]) * f
    };
    let cube4: Option<Vec<[f32; 4]>> =
        cube.map(|c| c.iter().map(|&[r, g, b]| [r, g, b, 1.0]).collect());

    let ns = NORMALIZED_SIZE;
    let mut data: Vec<[f32; 4]> = Vec::with_capacity(ns * ns * ns);
    for b in 0..ns {
        for g in 0..ns {
            for r in 0..ns {
                let x = [
                    r as f32 / (ns as f32 - 1.0),
                    g as f32 / (ns as f32 - 1.0),
                    b as f32 / (ns as f32 - 1.0),
                ];
                // 1D shaper (or the whole LUT when 1D-only).
                let mut v = x;
                if let Some(t) = shaper {
                    for c in 0..3 {
                        v[c] = lerp1d(t, c, to_unit(x[c], d1_lo[c], d1_hi[c]));
                    }
                }
                let out = match (&cube4, n3) {
                    (Some(c4), Some(n)) => {
                        let u = [
                            to_unit(v[0], d3_lo[0], d3_hi[0]),
                            to_unit(v[1], d3_lo[1], d3_hi[1]),
                            to_unit(v[2], d3_lo[2], d3_hi[2]),
                        ];
                        trilinear_sample(c4, n, u)
                    }
                    _ => v,
                };
                data.push([out[0], out[1], out[2], 1.0]);
            }
        }
    }

    Ok(CubeFile {
        title,
        size: ns,
        data,
        warnings,
    })
}

fn parse_range(
    parts: &mut std::str::SplitWhitespace<'_>,
    lineno: usize,
    kw: &str,
) -> Result<(f32, f32), LutParseError> {
    let bad = || LutParseError::BadKeyword(lineno, format!("{kw} expects 2 numbers"));
    let lo: f32 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let hi: f32 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    Ok((lo, hi))
}

/// Rejects other LUT formats with an actionable message instead of a
/// confusing parse error.
fn detect_foreign_format(text: &str) -> Result<(), LutParseError> {
    let head: String = text.chars().take(4096).collect();
    if head.contains('\0') {
        return Err(LutParseError::UnsupportedFormat(
            "this is a binary file, not a text .cube LUT".into(),
        ));
    }
    let lower = head.to_ascii_lowercase();
    let has_size = lower.contains("lut_3d_size") || lower.contains("lut_1d_size");
    if !has_size {
        if lower.trim_start().starts_with("<?xml") || lower.contains("<look") {
            return Err(LutParseError::UnsupportedFormat(
                "Adobe SpeedGrade .look (XML) LUTs are not supported — export the look as .cube (Premiere: Lumetri panel ▸ ⋯ ▸ Export .cube)".into(),
            ));
        }
        if lower.contains("csplutv100") {
            return Err(LutParseError::UnsupportedFormat(
                ".csp LUTs are not supported — convert to .cube".into(),
            ));
        }
        if lower.contains("3dmesh")
            || lower
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .map_or(false, |l| {
                    let toks: Vec<&str> = l.split_whitespace().collect();
                    toks.len() > 3 && toks.iter().all(|t| t.parse::<u32>().is_ok())
                })
        {
            return Err(LutParseError::UnsupportedFormat(
                ".3dl LUTs are not supported — re-export the LUT as .cube".into(),
            ));
        }
    }
    Ok(())
}

/// Trilinear sample of a 3D LUT lattice.
///
/// `data` holds `size^3` RGBA entries in red-fastest order
/// (`index = r + size * (g + size * b)`); `rgb` is clamped to 0..1.
/// Returns the interpolated RGB (alpha is ignored).
pub fn trilinear_sample(data: &[[f32; 4]], size: usize, rgb: [f32; 3]) -> [f32; 3] {
    debug_assert!(size >= 2, "LUT lattice size must be >= 2");
    let max_i = (size - 1) as f32;
    let mut p = [0.0f32; 3];
    let mut i0 = [0usize; 3];
    let mut f = [0.0f32; 3];
    for c in 0..3 {
        p[c] = (rgb[c].clamp(0.0, 1.0) * max_i).clamp(0.0, max_i);
        i0[c] = (p[c].floor() as usize).min(size - 2);
        f[c] = p[c] - i0[c] as f32;
    }
    let at = |ix: usize, iy: usize, iz: usize| -> [f32; 3] {
        let e = data[ix + size * (iy + size * iz)];
        [e[0], e[1], e[2]]
    };
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let mut out = [0.0f32; 3];
    for c in 0..3 {
        // 8 corners, lerped r -> g -> b.
        let c000 = at(i0[0], i0[1], i0[2])[c];
        let c100 = at(i0[0] + 1, i0[1], i0[2])[c];
        let c010 = at(i0[0], i0[1] + 1, i0[2])[c];
        let c110 = at(i0[0] + 1, i0[1] + 1, i0[2])[c];
        let c001 = at(i0[0], i0[1], i0[2] + 1)[c];
        let c101 = at(i0[0] + 1, i0[1], i0[2] + 1)[c];
        let c011 = at(i0[0], i0[1] + 1, i0[2] + 1)[c];
        let c111 = at(i0[0] + 1, i0[1] + 1, i0[2] + 1)[c];
        let x00 = lerp(c000, c100, f[0]);
        let x10 = lerp(c010, c110, f[0]);
        let x01 = lerp(c001, c101, f[0]);
        let x11 = lerp(c011, c111, f[0]);
        let y0 = lerp(x00, x10, f[1]);
        let y1 = lerp(x01, x11, f[1]);
        out[c] = lerp(y0, y1, f[2]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a 3D identity cube: output == input lattice position.
    fn identity_cube_3d(n: usize) -> String {
        let mut s = format!("# identity test cube\nTITLE \"identity {n}\"\nLUT_3D_SIZE {n}\n");
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    s.push_str(&format!(
                        "{} {} {}\n",
                        r as f32 / (n - 1) as f32,
                        g as f32 / (n - 1) as f32,
                        b as f32 / (n - 1) as f32
                    ));
                }
            }
        }
        s
    }

    #[test]
    fn parse_17_identity_cube() {
        let f = parse_cube(&identity_cube_3d(17)).unwrap();
        assert_eq!(f.title.as_deref(), Some("identity 17"));
        assert_eq!(f.size, 33);
        assert!(f.warnings.is_empty(), "warnings: {:?}", f.warnings);
        assert_eq!(f.data.len(), 33 * 33 * 33);
        // Identity is linear, so the 17 -> 33 resample is (near-)exact.
        for rgb in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.25, 0.5, 0.75],
            [0.9, 0.1, 0.33],
        ] {
            let s = trilinear_sample(&f.data, f.size, rgb);
            for c in 0..3 {
                assert!((s[c] - rgb[c]).abs() < 1e-4, "rgb={rgb:?} sampled={s:?}");
            }
            // Alpha is always 1.0.
            let e = f.data[16 + 33 * (16 + 33 * 16)];
            assert_eq!(e[3], 1.0);
        }
    }

    #[test]
    fn parse_1d_cube() {
        let n = 8;
        let mut s = String::from("TITLE \"1d identity\"\nLUT_1D_SIZE 8\n");
        for i in 0..n {
            let v = i as f32 / (n - 1) as f32;
            s.push_str(&format!("{v} {v} {v}\n"));
        }
        let f = parse_cube(&s).unwrap();
        assert_eq!(f.size, 33);
        assert!(f.warnings.is_empty());
        for rgb in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.3, 0.6, 0.9]] {
            let samp = trilinear_sample(&f.data, f.size, rgb);
            for c in 0..3 {
                assert!((samp[c] - rgb[c]).abs() < 1e-3, "rgb={rgb:?} -> {samp:?}");
            }
        }
    }

    #[test]
    fn size_9_resampled_with_warning() {
        let f = parse_cube(&identity_cube_3d(9)).unwrap();
        assert_eq!(f.size, 33);
        assert!(
            f.warnings
                .iter()
                .any(|w| w.contains("unsupported LUT size 9")),
            "warnings: {:?}",
            f.warnings
        );
        let s = trilinear_sample(&f.data, f.size, [0.5, 0.25, 0.75]);
        for c in 0..3 {
            assert!((s[c] - [0.5, 0.25, 0.75][c]).abs() < 1e-3);
        }
    }

    #[test]
    fn malformed_files_error() {
        assert!(matches!(parse_cube(""), Err(LutParseError::Empty)));
        assert!(matches!(parse_cube("   \n"), Err(LutParseError::Empty)));
        assert!(matches!(
            parse_cube("# just a comment\nTITLE \"no size\"\n"),
            Err(LutParseError::MissingSize)
        ));
        // Truncated data.
        let mut s = String::from("LUT_3D_SIZE 17\n");
        for _ in 0..100 {
            s.push_str("0 0 0\n");
        }
        assert!(matches!(
            parse_cube(&s),
            Err(LutParseError::WrongDataCount {
                expected: 4913,
                got: 100
            })
        ));
        // Too much data.
        let mut s = identity_cube_3d(17);
        s.push_str("0 0 0\n");
        assert!(matches!(
            parse_cube(&s),
            Err(LutParseError::WrongDataCount { .. })
        ));
        // Bad number on line 2.
        assert!(matches!(
            parse_cube("LUT_3D_SIZE 2\n0 0 oops\n"),
            Err(LutParseError::BadNumber(2))
        ));
        // Malformed keyword.
        assert!(matches!(
            parse_cube("LUT_3D_SIZE\n"),
            Err(LutParseError::BadKeyword(1, _))
        ));
        assert!(matches!(
            parse_cube("DOMAIN_MIN 0 0\nLUT_3D_SIZE 2\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n"),
            Err(LutParseError::BadKeyword(1, _))
        ));
    }

    #[test]
    fn unknown_keyword_warns_but_parses() {
        let mut text = String::from("LUT_3D_SIZE 17\nMYSTERY_KEYWORD hello\n");
        for b in 0..17 {
            for g in 0..17 {
                for r in 0..17 {
                    text.push_str(&format!(
                        "{} {} {}\n",
                        r as f32 / 16.0,
                        g as f32 / 16.0,
                        b as f32 / 16.0
                    ));
                }
            }
        }
        let f = parse_cube(&text).unwrap();
        assert_eq!(f.warnings.len(), 1);
        assert!(f.warnings[0].contains("MYSTERY_KEYWORD"));
    }

    #[test]
    fn domain_maps_input_not_output() {
        // DOMAIN 0..2 on an identity-shaped 2-point cube whose outputs are
        // the input-domain lattice (0 or 2). Input 0.5 sits at 1/4 of the
        // domain -> table coordinate 0.25 -> output 0.5 (identity). Outputs
        // are NOT rescaled by the domain.
        let mut s = String::from("LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 2 2 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    s.push_str(&format!("{} {} {}\n", r * 2, g * 2, b * 2));
                }
            }
        }
        let f = parse_cube(&s).unwrap();
        let samp = trilinear_sample(&f.data, f.size, [0.5, 0.5, 0.5]);
        for c in 0..3 {
            assert!((samp[c] - 0.5).abs() < 1e-4, "{samp:?}");
        }
    }

    #[test]
    fn resolve_input_range_is_honored() {
        // 1D LUT over input range 0..0.5 mapping to 0..1 (a 2x gain).
        let s = "LUT_1D_SIZE 2\nLUT_1D_INPUT_RANGE 0.0 0.5\n0 0 0\n1 1 1\n";
        let f = parse_cube(s).unwrap();
        let samp = trilinear_sample(&f.data, f.size, [0.25, 0.25, 0.25]);
        assert!((samp[0] - 0.5).abs() < 1e-3, "{samp:?}");
        // Inputs above the range clamp to the last entry.
        let samp = trilinear_sample(&f.data, f.size, [0.9, 0.9, 0.9]);
        assert!((samp[0] - 1.0).abs() < 1e-3, "{samp:?}");
    }

    #[test]
    fn shaper_plus_cube_applies_both() {
        // Shaper squares the input (0, .25, 1 at 0, .5, 1); cube inverts.
        let mut s = String::from("LUT_1D_SIZE 3\nLUT_3D_SIZE 2\n0 0 0\n0.25 0.25 0.25\n1 1 1\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    s.push_str(&format!("{} {} {}\n", 1 - r, 1 - g, 1 - b));
                }
            }
        }
        let f = parse_cube(&s).unwrap();
        assert!(f.warnings.iter().any(|w| w.contains("shaper")));
        let samp = trilinear_sample(&f.data, f.size, [0.5, 0.5, 0.5]);
        // shaper(0.5) = 0.25 -> inverted = 0.75.
        assert!((samp[0] - 0.75).abs() < 1e-3, "{samp:?}");
    }

    #[test]
    fn foreign_formats_get_clear_errors() {
        let e = parse_cube("<?xml version=\"1.0\"?><look></look>").unwrap_err();
        assert!(e.to_string().contains(".look"), "{e}");
        let e = parse_cube("0 64 128 192 256 320 384 448 512 576 640 704 768 832 896 960 1023\n0 0 0\n").unwrap_err();
        assert!(e.to_string().contains(".3dl"), "{e}");
        let e = parse_cube("CSPLUTV100\n3D\n").unwrap_err();
        assert!(e.to_string().contains(".csp"), "{e}");
    }

    #[test]
    fn premiere_style_65_cube_parses() {
        let f = parse_cube(&identity_cube_3d(65)).unwrap();
        assert!(f.warnings.is_empty(), "{:?}", f.warnings);
        let s = trilinear_sample(&f.data, f.size, [0.3, 0.6, 0.9]);
        for c in 0..3 {
            assert!((s[c] - [0.3, 0.6, 0.9][c]).abs() < 1e-3);
        }
    }

    #[test]
    fn oversized_lut_is_rejected() {
        assert!(matches!(
            parse_cube("LUT_3D_SIZE 512\n0 0 0\n"),
            Err(LutParseError::BadKeyword(1, _))
        ));
    }
}
