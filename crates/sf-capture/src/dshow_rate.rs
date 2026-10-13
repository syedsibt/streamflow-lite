//! DirectShow frame-rate requests.
//!
//! FFmpeg's dshow demuxer (libavdevice/dshow.c, `dshow_cycle_formats`)
//! turns the `framerate` option into a 100 ns frame interval as
//! `den * 10_000_000 / num` (integer division) and only accepts a pin format
//! when that interval lies in `[MinFrameInterval, MaxFrameInterval]`.
//! Drivers usually advertise 60 fps as 166667 (rounded) while "60" yields
//! 166666, so a plain integer request is rejected for those cameras.
//!
//! The option string goes through `av_parse_video_rate`, which accepts
//! `num:den` and reduces it with a 1 001 000 bound, so we need a *coprime*
//! pair within that bound whose integer interval equals the target exactly.

/// `ceil(a / b)` for positive values.
pub(crate) fn div_ceil_i64(a: i64, b: i64) -> i64 {
    (a + b - 1) / b
}

/// The interval FFmpeg's dshow computes for `num/den` fps.
pub(crate) fn dshow_interval(num: i64, den: i64) -> i64 {
    den * 10_000_000 / num
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// A `num:den` rate string for which dshow computes exactly `interval`
/// (100 ns units), or `None` if no pair fits `av_parse_video_rate`'s bound.
pub(crate) fn dshow_rate_for_interval(interval: i64) -> Option<String> {
    const MAX: i64 = 1_001_000;
    if interval <= 0 {
        return None;
    }
    for den in 1..=MAX {
        // den*1e7/num == interval  ⇔  den*1e7/(interval+1) < num <= den*1e7/interval
        let hi = den * 10_000_000 / interval;
        let lo = den * 10_000_000 / (interval + 1) + 1;
        if hi > MAX {
            break;
        }
        for num in lo..=hi {
            if gcd(num, den) == 1 && dshow_interval(num, den) == interval {
                return Some(format!("{num}:{den}"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_60_misses_rounded_interval() {
        // Documents the FFmpeg behaviour this module works around.
        assert_eq!(dshow_interval(60, 1), 166_666);
        assert_eq!(div_ceil_i64(10_000_000, 60), 166_667);
    }

    #[test]
    fn rates_hit_exact_intervals() {
        for fps in [5, 10, 15, 24, 25, 30, 50, 60, 120] {
            for interval in [10_000_000 / fps, div_ceil_i64(10_000_000, fps)] {
                let s = dshow_rate_for_interval(interval).expect("rate");
                let (n, d) = s.split_once(':').unwrap();
                let (n, d): (i64, i64) = (n.parse().unwrap(), d.parse().unwrap());
                assert_eq!(gcd(n, d), 1, "{s} must already be reduced");
                assert!(n <= 1_001_000 && d <= 1_001_000);
                assert_eq!(dshow_interval(n, d), interval, "fps {fps} → {s}");
            }
        }
    }
}
