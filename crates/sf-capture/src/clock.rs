//! One monotonic clock for the whole media pipeline, plus device-clock
//! mapping.
//!
//! Every capture timestamp in StreamFlow is expressed in microseconds on
//! [`now_us`] — a monotonic clock (never jumps with NTP / DST, unlike
//! `SystemTime`). Capture, the media tick, the encoders and the latency
//! diagnostics all read this one clock, so "how old is this frame" is a
//! plain subtraction everywhere.
//!
//! Devices stamp packets on *their own* clocks (DirectShow reference clock,
//! audio-device clock, …). [`ClockMapper`] converts a device timestamp to
//! [`now_us`] the way OBS does: the offset between the two clocks is the
//! smallest `(arrival − device_ts)` ever observed (the packet that waited
//! the least), allowed to creep upward slowly so device-vs-system clock
//! drift is tracked. A packet that sat in a buffer therefore maps to its
//! true (older) capture time — exactly what latency measurement and A/V
//! alignment need.

use std::sync::OnceLock;
use std::time::Instant;

fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}

/// Monotonic microseconds since the first call in this process.
pub fn now_us() -> i64 {
    epoch().elapsed().as_micros() as i64
}

/// Converts an [`Instant`] to the [`now_us`] timeline.
pub fn instant_us(t: Instant) -> i64 {
    let e = *epoch();
    if t >= e {
        t.duration_since(e).as_micros() as i64
    } else {
        -(e.duration_since(t).as_micros() as i64)
    }
}

/// Maps device timestamps onto [`now_us`] (see module docs).
#[derive(Debug, Clone, Default)]
pub struct ClockMapper {
    offset_us: Option<i64>,
    last_local_us: i64,
}

/// Allowed upward creep of the offset: 200 ppm of elapsed time (device
/// crystals are typically within ±100 ppm of the system clock).
const MAX_DRIFT_PPM: i64 = 200;
/// A jump larger than this means the device clock restarted: re-anchor.
const RESYNC_US: i64 = 2_000_000;

impl ClockMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps `device_us` (a device timestamp in µs) observed at `local_us`
    /// (now, on [`now_us`]) to the capture time on [`now_us`]. The result
    /// is never later than `local_us`.
    pub fn map(&mut self, device_us: i64, local_us: i64) -> i64 {
        let sample = local_us - device_us;
        let offset = match self.offset_us {
            None => sample,
            Some(o) if (sample - o).abs() > RESYNC_US => sample,
            Some(o) if sample < o => sample,
            Some(o) => {
                let elapsed = (local_us - self.last_local_us).max(0);
                let creep = elapsed * MAX_DRIFT_PPM / 1_000_000;
                o + (sample - o).min(creep)
            }
        };
        self.offset_us = Some(offset);
        self.last_local_us = local_us;
        (device_us + offset).min(local_us)
    }

    /// Forget the anchor (e.g. after a device reopen).
    pub fn reset(&mut self) {
        self.offset_us = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
    }

    #[test]
    fn queued_packets_map_to_their_true_age() {
        let mut m = ClockMapper::new();
        // Device clock is 1_000_000 µs behind; first packet arrives with
        // 5 ms transport delay, next ones arrive promptly.
        assert_eq!(m.map(10_000, 1_015_000), 1_015_000); // anchor (worst case first)
        assert_eq!(m.map(43_333, 1_043_400), 1_043_400); // smaller lag → re-anchor
        // A packet captured at device 76_666 but read 400 ms late (backlog):
        let mapped = m.map(76_666, 1_476_666);
        assert!((mapped - 1_076_733).abs() < 2_000, "mapped {mapped}");
    }

    #[test]
    fn tracks_slow_drift_and_restarts() {
        let mut m = ClockMapper::new();
        let mut dev = 0i64;
        let mut local = 1_000_000i64;
        // Device runs 100 ppm slow for 100 s: mapping keeps up (no growing lag).
        for _ in 0..3000 {
            dev += 33_333;
            local += 33_336; // +100 ppm
            let mapped = m.map(dev, local);
            assert!(local - mapped < 1_000, "lag {}", local - mapped);
        }
        // Device restarts at 0: re-anchors instead of mapping 100 s into the past.
        let mapped = m.map(0, local + 33_333);
        assert_eq!(mapped, local + 33_333);
    }
}
