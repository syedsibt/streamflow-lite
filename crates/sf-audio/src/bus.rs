//! Clocked audio bus: per-source jitter FIFOs + a sample-exact block clock.
//!
//! # Why this exists (the bugs it replaces)
//!
//! The media tick used to collect "whatever chunks arrived" per source and
//! hand them all to [`crate::Mixer::mix`] — which looks up **one** frame per
//! source id, so every chunk after the first was silently discarded, and the
//! block length was whatever size the first chunk happened to be. Result:
//! choppy/garbled audio, wrong playback speed for files, and ticks with no
//! chunk produced *no* audio at all, so the AAC timeline shrank relative to
//! the video timeline (A/V drift that grows over a stream).
//!
//! # Model (OBS-style fixed audio ticks)
//!
//! * Every media tick asks [`AudioBus::next_block_frames`] how many sample
//!   frames this tick represents. The [`SampleClock`] is exact (rational fps,
//!   no rounding drift): at 30 fps it yields 1600 every tick; at 29.97 it
//!   alternates so that the long-run rate is exactly 48 000 Hz.
//! * Producers push whatever they have into their source FIFO.
//! * [`AudioBus::pull`] returns **exactly** the requested frames for every
//!   source, every tick — real samples when available, silence on underrun.
//!   So the mixer always sees equal-length blocks and the encoder's audio
//!   timeline always advances in lock-step with video PTS.
//!
//! # Latency rules (live first)
//!
//! * A source starts (or restarts after an underrun) only once it holds one
//!   block plus a small prebuffer (default 10 ms) — absorbs capture jitter
//!   without chopping.
//! * If a FIFO grows past `max_latency` (default 100 ms: capture clock faster
//!   than the tick clock, or ticks were skipped under load), the oldest audio
//!   is dropped back to `trim_to` (default 30 ms). Audio never builds a
//!   backlog; it snaps back to live, like the video path's newest-wins rule.

use std::collections::{HashMap, VecDeque};

use crate::{TARGET_CHANNELS, TARGET_SAMPLE_RATE};

const CH: usize = TARGET_CHANNELS as usize;

/// Sample-exact block clock: frames per tick for a rational frame rate.
#[derive(Clone, Debug)]
pub struct SampleClock {
    rate: u64,
    fps_num: u64,
    fps_den: u64,
    ticks: u64,
}

impl SampleClock {
    pub fn new(rate: u32, fps_num: u32, fps_den: u32) -> Self {
        Self {
            rate: rate as u64,
            fps_num: fps_num.max(1) as u64,
            fps_den: fps_den.max(1) as u64,
            ticks: 0,
        }
    }

    /// Change the frame rate. Restarts the fractional accumulator (a single
    /// tick of rounding, never a drift).
    pub fn set_fps(&mut self, fps_num: u32, fps_den: u32) {
        let (n, d) = (fps_num.max(1) as u64, fps_den.max(1) as u64);
        if n != self.fps_num || d != self.fps_den {
            self.fps_num = n;
            self.fps_den = d;
            self.ticks = 0;
        }
    }

    fn frames_at(&self, t: u64) -> u64 {
        // floor(t · rate · den / num) — computed in u128 to never overflow.
        ((t as u128 * self.rate as u128 * self.fps_den as u128) / self.fps_num as u128) as u64
    }

    /// Advance one tick; returns the sample frames this tick covers.
    pub fn next_frames(&mut self) -> usize {
        let a = self.frames_at(self.ticks);
        self.ticks += 1;
        let b = self.frames_at(self.ticks);
        (b - a) as usize
    }
}

/// Bus tuning (sample frames at 48 kHz).
#[derive(Clone, Copy, Debug)]
pub struct BusConfig {
    /// Extra frames (beyond one block) a source must hold before it starts.
    pub prebuffer_frames: usize,
    /// FIFO depth beyond one block that triggers a trim.
    pub max_latency_frames: usize,
    /// Depth (beyond one block) a trim cuts back to.
    pub trim_to_frames: usize,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            prebuffer_frames: 480,     // 10 ms
            max_latency_frames: 4_800, // 100 ms
            trim_to_frames: 1_440,     // 30 ms
        }
    }
}

/// Per-source bus diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SourceBusStats {
    /// Audio currently queued for this source, in milliseconds.
    pub buffered_ms: f32,
    /// Times the source ran dry and silence was inserted.
    pub underruns: u64,
    /// Sample frames dropped to stay live (backlog trims + hard cap).
    pub dropped_frames: u64,
    /// True while the source is delivering (primed).
    pub active: bool,
}

#[derive(Default)]
struct Fifo {
    buf: VecDeque<f32>,
    primed: bool,
    underruns: u64,
    dropped_frames: u64,
}

impl Fifo {
    fn frames(&self) -> usize {
        self.buf.len() / CH
    }
}

/// See the module docs.
pub struct AudioBus {
    fifos: HashMap<String, Fifo>,
    clock: SampleClock,
    cfg: BusConfig,
}

/// Hard memory cap per source regardless of pull cadence (1 s).
const HARD_CAP_FRAMES: usize = TARGET_SAMPLE_RATE as usize;

impl AudioBus {
    /// New bus clocked at `fps_num/fps_den` ticks per second.
    pub fn new(fps_num: u32, fps_den: u32) -> Self {
        Self::with_config(fps_num, fps_den, BusConfig::default())
    }

    pub fn with_config(fps_num: u32, fps_den: u32, cfg: BusConfig) -> Self {
        Self {
            fifos: HashMap::new(),
            clock: SampleClock::new(TARGET_SAMPLE_RATE, fps_num, fps_den),
            cfg,
        }
    }

    pub fn config(&self) -> BusConfig {
        self.cfg
    }

    /// Follow a frame-rate change of the media tick.
    pub fn set_fps(&mut self, fps_num: u32, fps_den: u32) {
        self.clock.set_fps(fps_num, fps_den);
    }

    /// Advance the clock one tick: sample frames to pull this tick.
    pub fn next_block_frames(&mut self) -> usize {
        self.clock.next_frames()
    }

    /// Queue interleaved stereo 48 kHz samples for `id` (registers the
    /// source on first use). Odd trailing samples are ignored.
    pub fn push(&mut self, id: &str, samples: &[f32]) {
        let fifo = self.fifos.entry(id.to_string()).or_default();
        let usable = samples.len() - samples.len() % CH;
        fifo.buf.extend(&samples[..usable]);
        let over = fifo.frames().saturating_sub(HARD_CAP_FRAMES);
        if over > 0 {
            fifo.buf.drain(..over * CH);
            fifo.dropped_frames += over as u64;
        }
    }

    /// Frames currently queued for `id` (0 if unknown).
    pub fn buffered_frames(&self, id: &str) -> usize {
        self.fifos.get(id).map(Fifo::frames).unwrap_or(0)
    }

    /// Frames a paced (file) producer should keep queued so the next pull
    /// is satisfied without underrun: one block + prebuffer.
    pub fn refill_target_frames(&self, block_frames: usize) -> usize {
        block_frames + self.cfg.prebuffer_frames
    }

    /// Write exactly `frames` stereo frames for `id` into `out` (cleared
    /// first): queued audio, padded with silence on underrun.
    pub fn pull(&mut self, id: &str, frames: usize, out: &mut Vec<f32>) {
        out.clear();
        out.resize(frames * CH, 0.0);
        let cfg = self.cfg;
        let Some(fifo) = self.fifos.get_mut(id) else {
            return;
        };
        if !fifo.primed {
            if fifo.frames() >= frames + cfg.prebuffer_frames {
                fifo.primed = true;
            } else {
                return; // still buffering: silence, keep what we have
            }
        }
        let avail = fifo.frames().min(frames);
        for (o, s) in out.iter_mut().zip(fifo.buf.drain(..avail * CH)) {
            *o = s;
        }
        if avail < frames {
            fifo.primed = false;
            fifo.underruns += 1;
        }
        // Stay live: never let a backlog build.
        if fifo.frames() > cfg.max_latency_frames {
            let drop = fifo.frames() - cfg.trim_to_frames;
            fifo.buf.drain(..drop * CH);
            fifo.dropped_frames += drop as u64;
        }
    }

    /// Drop sources not in `ids`.
    pub fn retain(&mut self, ids: &[String]) {
        self.fifos.retain(|k, _| ids.iter().any(|i| i == k));
    }

    pub fn remove(&mut self, id: &str) {
        self.fifos.remove(id);
    }

    pub fn stats(&self, id: &str) -> SourceBusStats {
        match self.fifos.get(id) {
            Some(f) => SourceBusStats {
                buffered_ms: f.frames() as f32 * 1000.0 / TARGET_SAMPLE_RATE as f32,
                underruns: f.underruns,
                dropped_frames: f.dropped_frames,
                active: f.primed,
            },
            None => SourceBusStats::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_30fps_is_1600_per_tick() {
        let mut c = SampleClock::new(48_000, 30, 1);
        for _ in 0..100 {
            assert_eq!(c.next_frames(), 1600);
        }
    }

    #[test]
    fn clock_ntsc_has_no_long_run_drift() {
        let mut c = SampleClock::new(48_000, 30_000, 1001);
        let total: usize = (0..30_000).map(|_| c.next_frames()).sum();
        // 30000 ticks at 29.97 fps = 1001 s exactly = 48_048_000 frames.
        assert_eq!(total, 48_048_000);
    }

    #[test]
    fn every_chunk_is_used_in_order() {
        // The old path mixed only the first chunk per tick; the bus must
        // deliver every sample, in order, across ticks.
        let mut bus = AudioBus::with_config(
            30,
            1,
            BusConfig {
                prebuffer_frames: 0,
                ..BusConfig::default()
            },
        );
        let mut next = 0.0f32;
        let mut produce = |n: usize| -> Vec<f32> {
            (0..n)
                .flat_map(|_| {
                    next += 1.0;
                    [next, next]
                })
                .collect()
        };
        // Three 1024-frame chunks (typical AAC decode size) arrive.
        for _ in 0..3 {
            let c = produce(1024);
            bus.push("a", &c);
        }
        let mut out = Vec::new();
        let mut got = Vec::new();
        for _ in 0..2 {
            let n = bus.next_block_frames();
            bus.pull("a", n, &mut out);
            assert_eq!(out.len(), n * 2);
            got.extend(out.iter().step_by(2).copied());
        }
        let want: Vec<f32> = (1..=3072).map(|v| v as f32).collect();
        assert_eq!(&got[..3072], &want[..]);
    }

    #[test]
    fn underrun_pads_silence_and_counts() {
        let mut bus = AudioBus::new(30, 1);
        bus.push("mic", &vec![0.5; 2000 * 2]); // block 1600 + prebuffer 480 > 2000? no
        let mut out = Vec::new();
        bus.pull("mic", 1600, &mut out);
        // 2000 < 1600 + 480: still buffering → silence, nothing consumed.
        assert!(out.iter().all(|&v| v == 0.0));
        assert_eq!(bus.buffered_frames("mic"), 2000);
        bus.push("mic", &vec![0.5; 200 * 2]); // now 2200 ≥ 2080 → primes
        bus.pull("mic", 1600, &mut out);
        assert!(out.iter().all(|&v| v == 0.5));
        bus.pull("mic", 1600, &mut out); // only 600 left → underrun
        assert_eq!(out.iter().filter(|&&v| v == 0.5).count(), 600 * 2);
        assert_eq!(bus.stats("mic").underruns, 1);
        assert!(!bus.stats("mic").active);
    }

    #[test]
    fn backlog_is_trimmed_to_stay_live() {
        let mut bus = AudioBus::new(30, 1);
        // Capture delivered 500 ms while the tick stalled.
        bus.push("mic", &vec![0.1; 24_000 * 2]);
        let mut out = Vec::new();
        bus.pull("mic", 1600, &mut out);
        let cfg = bus.config();
        assert_eq!(bus.buffered_frames("mic"), cfg.trim_to_frames);
        assert!(bus.stats("mic").dropped_frames > 0);
        assert!(bus.stats("mic").buffered_ms <= 31.0);
    }

    #[test]
    fn unknown_source_yields_silence_of_exact_length() {
        let mut bus = AudioBus::new(60, 1);
        let mut out = vec![9.0; 3];
        let n = bus.next_block_frames();
        bus.pull("nope", n, &mut out);
        assert_eq!(out.len(), 800 * 2);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn hard_cap_bounds_memory() {
        let mut bus = AudioBus::new(30, 1);
        for _ in 0..10 {
            bus.push("x", &vec![0.0; 48_000 * 2]);
        }
        assert!(bus.buffered_frames("x") <= HARD_CAP_FRAMES);
    }
}
