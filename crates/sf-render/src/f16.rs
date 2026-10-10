//! IEEE 754 binary16 (half float) conversions.
//!
//! `rgba16float` textures are the linear working-space pixel format
//! everywhere in the render layer, so f32↔f16 conversion is shared here
//! (moved out of `grade_pass`, which used to own `f32_to_f16` privately).
//! Both directions are round-to-nearest-even on the f32→f16 side and exact
//! on the f16→f32 side; used by texture uploads, test helpers, and anywhere
//! else a half-float bit pattern is needed (`half` is not a dependency, so
//! these are hand-rolled).

/// Round-to-nearest-even conversion of an `f32` to an IEEE 754 binary16 bit
/// pattern.
///
/// NaN inputs produce a quiet NaN pattern (`sign | 0x7e00`); infinities map
/// to `sign | 0x7c00`; values too large for f16 saturate to infinity; f32
/// subnormals either become f16 subnormals or round to zero.
pub fn f32_to_f16(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let mant = x & 0x7f_ffff;
    if exp == 0xff {
        // Inf / NaN.
        return sign | 0x7c00 | if mant != 0 { 0x0200 } else { 0 };
    }
    let e16 = exp - 127 + 15;
    if e16 >= 31 {
        return sign | 0x7c00; // Overflow -> infinity.
    }
    if e16 <= 0 {
        // Subnormal or underflow to zero.
        if e16 < -10 {
            return sign;
        }
        let m = mant | 0x80_0000;
        let shift = (14 - e16) as u32;
        let mut half = m >> shift;
        let rest = m & ((1u32 << shift) - 1);
        let halfway = 1u32 << (shift - 1);
        if rest > halfway || (rest == halfway && (half & 1) == 1) {
            half += 1;
        }
        return sign | half as u16;
    }
    let mut half = ((e16 as u32) << 10) | (mant >> 13);
    let rest = mant & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && (half & 1) == 1) {
        half += 1;
    }
    sign | half as u16
}

/// Exact conversion of an IEEE 754 binary16 bit pattern to `f32`.
///
/// f16 subnormals, infinities, and NaNs round-trip correctly (NaN payload
/// bits are preserved in the f32 mantissa). Every f16 value converts exactly;
/// combined with [`f32_to_f16`], arbitrary f32 values round-trip to the
/// nearest f16.
pub fn f16_to_f32(h: u16) -> f32 {
    let h = h as u32;
    let sign = (h & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let mant = h & 0x3ff;
    let bits = if exp == 0 {
        if mant == 0 {
            // ±0.
            sign
        } else {
            // Subnormal: renormalize the mantissa into a normal f32.
            let mut e: u32 = 127 - 14;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3ff; // Drop the implicit leading 1.
            sign | (e << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        // Inf / NaN (payload preserved).
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp + (127 - 15)) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_f16_known_values() {
        assert_eq!(f32_to_f16(0.0), 0x0000);
        assert_eq!(f32_to_f16(-0.0), 0x8000);
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(0.5), 0x3800);
        assert_eq!(f32_to_f16(-2.0), 0xc000);
        assert_eq!(f32_to_f16(1.5), 0x3e00);
        assert_eq!(f32_to_f16(65504.0), 0x7bff);
        assert_eq!(f32_to_f16(100000.0), 0x7c00);
        assert_eq!(f32_to_f16(f32::INFINITY), 0x7c00);
        assert_eq!(f32_to_f16(f32::NEG_INFINITY), 0xfc00);
        assert!(f32_to_f16(f32::NAN) & 0x7c00 == 0x7c00);
        // Smallest positive subnormal -> rounds to zero; min normal survives.
        assert_eq!(f32_to_f16(1e-8), 0x0000);
        assert_ne!(f32_to_f16(6.10352e-5), 0x0000);
        // Round-to-nearest-even: 0.333251953125 is exactly between two f16
        // values; the even one (0x3555) must win.
        assert_eq!(f32_to_f16(0.333251953125), 0x3555);
    }

    #[test]
    fn f16_to_f32_known_values() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x8000), -0.0);
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0x3800), 0.5);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x7bff), 65504.0);
        assert_eq!(f16_to_f32(0x7c00), f32::INFINITY);
        assert_eq!(f16_to_f32(0xfc00), f32::NEG_INFINITY);
        assert!(f16_to_f32(0x7e00).is_nan());
        // Subnormals: smallest is 2^-24, largest is just under 2^-14.
        assert_eq!(f16_to_f32(0x0001), 5.9604645e-8);
        assert_eq!(f16_to_f32(0x03ff), 1023.0 * 2.0f32.powi(-24));
        // Min normal.
        assert_eq!(f16_to_f32(0x0400), 6.1035156e-5);
        // Sign of zero is preserved.
        assert!(f16_to_f32(0x8000).is_sign_negative());
    }

    #[test]
    fn f16_roundtrip_all_bit_patterns() {
        // Every f16 value converts to f32 and back to itself exactly.
        for h in 0..=u16::MAX {
            let f = f16_to_f32(h);
            let back = f32_to_f16(f);
            if h & 0x7c00 == 0x7c00 && h & 0x03ff != 0 {
                // NaN: payload may be canonicalized, but must stay NaN.
                assert!(f.is_nan(), "f16 0x{h:04x} should be NaN");
                assert!(back & 0x7c00 == 0x7c00, "f16 0x{h:04x} lost NaN");
                assert_eq!(back & 0x8000, h & 0x8000, "NaN sign flipped");
            } else {
                assert_eq!(back, h, "f16 0x{h:04x} did not round-trip");
            }
        }
    }

    #[test]
    fn f32_roundtrip_is_nearest_f16() {
        // A sweep of normal-range values: f32 -> f16 -> f32 is idempotent
        // and within half an f16 ulp of the original.
        let mut v = -100.0f32;
        while v <= 100.0 {
            let q = f16_to_f32(f32_to_f16(v));
            assert_eq!(f16_to_f32(f32_to_f16(q)), q, "not idempotent at {v}");
            let ulp = (q.abs() * 2.0f32.powi(-10)).max(6.0e-8);
            assert!(
                (q - v).abs() <= ulp,
                "roundtrip of {v} off by more than half an ulp"
            );
            v += 0.37;
        }
    }
}
