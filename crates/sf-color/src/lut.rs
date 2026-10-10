//! Adobe `.cube` LUT parser (LUT tab).
//!
//! Supports 1D and 3D LUTs. Everything is normalized to a 33^3 RGBA lattice
//! on load, so the GPU always uploads the same 33^3 `rgba16float` texture:
//!
//! * 3D sizes in {17, 25, 33, 65} are resampled to 33^3 silently (33 is kept
//!   as-is); any other size is trilinearly resampled to 33^3 **with** a
//!   warning (`"unsupported LUT size {n}, resampled to 33"`).
//! * 1D LUTs are expanded to 33^3 by per-channel linear lookup over the 1D
//!   table on the 33-step lattice.
//! * Output values are normalized to [0, 1] via the `DOMAIN_MIN`/`DOMAIN_MAX`
//!   mapping: `out = (v - dmin) / (dmax - dmin)` per channel.
//!
//! 3D data order in the file is red-fastest: `index = r + n * (g + n * b)`,
//! which matches the storage order of [`CubeFile::data`].

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
const NATIVE_SIZES: [usize; 4] = [17, 25, 33, 65];
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

    let mut title: Option<String> = None;
    let mut size_1d: Option<(usize, usize)> = None; // (size, lineno)
    let mut size_3d: Option<(usize, usize)> = None;
    let mut dmin = [0.0f32; 3];
    let mut dmax = [1.0f32; 3];
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

    // A 3D size takes precedence when both are (incorrectly) present.
    if size_1d.is_some() && size_3d.is_some() {
        warnings.push("both LUT_1D_SIZE and LUT_3D_SIZE present; using the 3D table".to_string());
    }
    enum Kind {
        D1(usize),
        D3(usize),
    }
    let kind = match (size_3d, size_1d) {
        (Some((n, _)), _) => Kind::D3(n),
        (None, Some((n, _))) => Kind::D1(n),
        (None, None) => return Err(LutParseError::MissingSize),
    };

    let expected: usize = match kind {
        Kind::D3(n) => n * n * n,
        Kind::D1(n) => n,
    };
    if raw.len() != expected {
        return Err(LutParseError::WrongDataCount {
            expected,
            got: raw.len(),
        });
    }

    // Normalize output values through the domain mapping, per channel.
    // (A zero-width domain is degenerate; pass values through unscaled.)
    let mut norm: Vec<[f32; 3]> = Vec::with_capacity(raw.len());
    for [r, g, b] in raw {
        let vs = [r, g, b];
        let mut o = [0.0f32; 3];
        for c in 0..3 {
            let range = dmax[c] - dmin[c];
            o[c] = if range.abs() < 1e-9 {
                vs[c] - dmin[c]
            } else {
                (vs[c] - dmin[c]) / range
            };
        }
        norm.push(o);
    }

    let n3 = NORMALIZED_SIZE;
    let mut data: Vec<[f32; 4]> = Vec::with_capacity(n3 * n3 * n3);
    match kind {
        Kind::D3(n) if n == n3 => {
            // Already at the target lattice; keep values as-is.
            data.extend(norm.into_iter().map(|[r, g, b]| [r, g, b, 1.0]));
        }
        Kind::D3(n) => {
            if !NATIVE_SIZES.contains(&n) {
                warnings.push(format!("unsupported LUT size {n}, resampled to 33"));
            }
            let src: Vec<[f32; 4]> = norm.into_iter().map(|[r, g, b]| [r, g, b, 1.0]).collect();
            for b in 0..n3 {
                for g in 0..n3 {
                    for r in 0..n3 {
                        let rgb = [
                            r as f32 / (n3 as f32 - 1.0),
                            g as f32 / (n3 as f32 - 1.0),
                            b as f32 / (n3 as f32 - 1.0),
                        ];
                        let s = trilinear_sample(&src, n, rgb);
                        data.push([s[0], s[1], s[2], 1.0]);
                    }
                }
            }
        }
        Kind::D1(n) => {
            // Expand the 1D table onto the 33^3 lattice by per-channel linear
            // lookup. The lattice coordinate *is* the normalized domain
            // position, so table index = x * (n - 1).
            for b in 0..n3 {
                for g in 0..n3 {
                    for r in 0..n3 {
                        let coords = [
                            r as f32 / (n3 as f32 - 1.0),
                            g as f32 / (n3 as f32 - 1.0),
                            b as f32 / (n3 as f32 - 1.0),
                        ];
                        let mut o = [0.0f32; 3];
                        for c in 0..3 {
                            let pos = coords[c] * (n as f32 - 1.0);
                            let i0 = (pos.floor() as usize).min(n - 2);
                            let f = pos - i0 as f32;
                            o[c] = norm[i0][c] + (norm[i0 + 1][c] - norm[i0][c]) * f;
                        }
                        data.push([o[0], o[1], o[2], 1.0]);
                    }
                }
            }
        }
    }

    Ok(CubeFile {
        title,
        size: n3,
        data,
        warnings,
    })
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
    fn domain_mapping_normalizes_output() {
        // DOMAIN 0..255: output 128 -> ~0.50196.
        let mut s = String::from("LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 255 255 255\n");
        for _ in 0..8 {
            s.push_str("128 128 128\n");
        }
        let f = parse_cube(&s).unwrap();
        // n=2 is unsupported -> resample warning, but parse succeeds.
        assert!(f
            .warnings
            .iter()
            .any(|w| w.contains("unsupported LUT size 2")));
        let samp = trilinear_sample(&f.data, f.size, [0.5, 0.5, 0.5]);
        for c in 0..3 {
            assert!((samp[c] - 128.0 / 255.0).abs() < 1e-4, "{samp:?}");
        }
    }
}
