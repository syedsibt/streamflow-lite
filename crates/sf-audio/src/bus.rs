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

//!
//! # A/V alignment (capture timestamps)
//!
//! Live sources push audio with the capture time of its first sample
//! ([`AudioBus::push_timed`], pipeline clock µs). [`AudioBus::pull_aligned`]
//! then plays each source with a requested *age*: the age of the video frame
//! rendered in the same tick (+ the user's manual offset). Audio that is
//! fresher than the picture is delayed by holding it in the FIFO; audio that
//! is older is dropped. Corrections only happen outside a ±15 ms dead band,
//! so a steady source is never touched. This is how the camera's own
//! latency (often 60–200 ms more than a USB mic's) stops showing up as
//! audio-ahead-of-lips.
//!
//! # Time-based block length
//!
//! [`AudioTimeline`] turns the media tick's *time* into a block length:
//! samples since the session origin minus samples already produced. A tick
//! that ran late (or several that were skipped) produces exactly the audio
//! for the elapsed wall time, so the audio timeline never falls behind real
//! time and live FIFOs never accumulate.

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

/// Wall-clock audio timeline: how many sample frames a tick at time `t`
/// must produce (see module docs).
#[derive(Clone, Debug, Default)]
pub struct AudioTimeline {
    origin_us: Option<i64>,
    done: u64,
}

impl AudioTimeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames to produce for a tick at `t_us` (µs, any monotonic clock).
    /// The first call starts the timeline and returns one block of
    /// `first_block` frames.
    pub fn frames_until(&mut self, t_us: i64, first_block: usize) -> usize {
        let origin = match self.origin_us {
            Some(o) => o,
            None => {
                let rate = TARGET_SAMPLE_RATE as i64;
                let o = t_us - first_block as i64 * 1_000_000 / rate;
                self.origin_us = Some(o);
                o
            }
        };
        let elapsed = (t_us - origin).max(0) as u128;
        let target = ((elapsed * TARGET_SAMPLE_RATE as u128 + 500_000) / 1_000_000) as u64;
        let n = target.saturating_sub(self.done);
        self.done = self.done.max(target);
        n as usize
    }

    /// Sample frames produced so far.
    pub fn produced(&self) -> u64 {
        self.done
    }

    pub fn reset(&mut self) {
        *self = Self::default();
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
    /// Age of the audio played last tick (capture → now), ms. `None` for
    /// sources without capture timestamps.
    pub audio_age_ms: Option<f32>,
    /// Requested age (video age + manual offset), ms, when aligning.
    pub target_age_ms: Option<f32>,
    /// Alignment corrections applied (drops + inserted delays).
    pub corrections: u64,
}

#[derive(Default)]
struct Fifo {
    buf: VecDeque<f32>,
    /// `(absolute frame index, capture µs)` of pushed chunks, oldest first.
    marks: VecDeque<(u64, i64)>,
    /// Absolute frame index of `buf[0]`.
    head_frame: u64,
    primed: bool,
    underruns: u64,
    dropped_frames: u64,
    audio_age_us: Option<i64>,
    target_age_us: Option<i64>,
    corrections: u64,
}

impl Fifo {
    fn frames(&self) -> usize {
        self.buf.len() / CH
    }

    /// Remove `n` frames from the head (they were played or dropped).
    fn consume(&mut self, n: usize) -> std::collections::vec_deque::Drain<'_, f32> {
        let n = n.min(self.frames());
        self.head_frame += n as u64;
        while self.marks.len() >= 2 && self.marks[1].0 <= self.head_frame {
            self.marks.pop_front();
        }
        if n == self.frames() {
            self.marks.clear();
        }
        self.buf.drain(..n * CH)
    }

    /// Capture time of the head sample, when known.
    fn head_time(&self) -> Option<i64> {
        if self.buf.is_empty() {
            return None;
        }
        let (f, at) = *self.marks.front()?;
        let off = self.head_frame.saturating_sub(f) as i64;
        Some(at + off * 1_000_000 / TARGET_SAMPLE_RATE as i64)
    }
}

/// Alignment dead band: corrections smaller than this are never applied.
pub const ALIGN_TOLERANCE_US: i64 = 15_000;
/// Longest delay the bus will add to align audio to video.
pub const MAX_ALIGN_DELAY_US: i64 = 800_000;

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
        self.push_timed(id, samples, None);
    }

    /// [`Self::push`] with the capture time (pipeline clock, µs) of the
    /// first sample — enables [`Self::pull_aligned`].
    pub fn push_timed(&mut self, id: &str, samples: &[f32], captured_at_us: Option<i64>) {
        let fifo = self.fifos.entry(id.to_string()).or_default();
        let usable = samples.len() - samples.len() % CH;
        if usable == 0 {
            return;
        }
        if let Some(at) = captured_at_us {
            let tail = fifo.head_frame + fifo.frames() as u64;
            fifo.marks.push_back((tail, at));
        }
        fifo.buf.extend(&samples[..usable]);
        let over = fifo.frames().saturating_sub(HARD_CAP_FRAMES);
        if over > 0 {
            fifo.consume(over);
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
        for (o, s) in out.iter_mut().zip(fifo.consume(avail)) {
            *o = s;
        }
        if avail < frames {
            fifo.primed = false;
            fifo.underruns += 1;
        }
        // Stay live: never let a backlog build.
        if fifo.frames() > cfg.max_latency_frames {
            let drop = fifo.frames() - cfg.trim_to_frames;
            fifo.consume(drop);
            fifo.dropped_frames += drop as u64;
        }
    }

    /// Like [`Self::pull`], but plays the source at a requested age so it
    /// lines up with the video rendered in the same tick.
    ///
    /// * `tick_us` — the tick's time on the pipeline clock; the block ends
    ///   at `tick_us` (it covers the `frames` before it).
    /// * `target_age_us` — how old the audio should be when played: the
    ///   age of the video frame rendered this tick plus the manual offset.
    ///   `None` (or a source without capture timestamps) → plain
    ///   lowest-latency [`Self::pull`].
    pub fn pull_aligned(
        &mut self,
        id: &str,
        frames: usize,
        out: &mut Vec<f32>,
        tick_us: i64,
        target_age_us: Option<i64>,
    ) {
        let rate = TARGET_SAMPLE_RATE as i64;
        let head = self.fifos.get(id).and_then(Fifo::head_time);
        let (Some(target_age), Some(head)) = (target_age_us, head) else {
            if let Some(f) = self.fifos.get_mut(id) {
                f.target_age_us = None;
                f.audio_age_us = f.head_time().map(|h| tick_us - h);
            }
            return self.pull(id, frames, out);
        };
        let target_age = target_age.clamp(0, MAX_ALIGN_DELAY_US);
        out.clear();
        out.resize(frames * CH, 0.0);
        let fifo = self.fifos.get_mut(id).expect("head exists");
        fifo.primed = true;
        fifo.target_age_us = Some(target_age);
        let block_us = frames as i64 * 1_000_000 / rate;
        // Where the head *should* be: captured `target_age` before the
        // start of this block.
        let want_head = tick_us - block_us - target_age;
        let err = head - want_head; // > 0: audio too fresh; < 0: too old
        let mut lead = 0usize;
        if err < -ALIGN_TOLERANCE_US {
            // Too old: drop the stale part (stay live).
            let n = ((-err) * rate / 1_000_000) as usize;
            let n = n.min(fifo.frames());
            fifo.consume(n);
            fifo.dropped_frames += n as u64;
            fifo.corrections += 1;
        } else if err > ALIGN_TOLERANCE_US {
            // Too fresh: delay it — silence now, the audio stays queued.
            lead = ((err * rate / 1_000_000) as usize).min(frames);
            fifo.corrections += 1;
        }
        let avail = fifo.frames().min(frames - lead);
        fifo.audio_age_us = fifo
            .head_time()
            .map(|h| tick_us - block_us + (lead as i64 * 1_000_000 / rate) - h);
        for (o, s) in out[lead * CH..].iter_mut().zip(fifo.consume(avail)) {
            *o = s;
        }
        if lead == 0 && avail < frames {
            fifo.underruns += 1;
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
                audio_age_ms: f.audio_age_us.map(|v| v as f32 / 1000.0),
                target_age_ms: f.target_age_us.map(|v| v as f32 / 1000.0),
                corrections: f.corrections,
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
    fn timeline_covers_wall_time_even_when_ticks_are_skipped() {
        let mut tl = AudioTimeline::new();
        assert_eq!(tl.frames_until(1_000_000, 1600), 1600);
        assert_eq!(tl.frames_until(1_033_333, 1600), 1600);
        // Three ticks missed: the next one covers all elapsed time.
        assert_eq!(tl.frames_until(1_166_667, 1600), 6400);
        // Long run: exactly 48 kHz of wall time, no drift.
        let mut t = 1_166_667i64;
        for _ in 0..30_000 {
            t += 33_333;
            tl.frames_until(t, 1600);
        }
        let expect = ((t - (1_000_000 - 33_333)) as f64 * 0.048).round() as u64;
        assert!((tl.produced() as i64 - expect as i64).abs() <= 1);
    }

    /// A mic that is 80 ms fresher than the camera picture gets delayed
    /// by 80 ms (± tolerance); a stale backlog gets dropped.
    #[test]
    fn aligns_audio_to_video_age() {
        let mut bus = AudioBus::new(30, 1);
        let mut out = Vec::new();
        let mut cap_t = 0i64; // capture time of next pushed chunk
        let mut tick = 20_000i64;
        let mut ages = Vec::new();
        for _ in 0..90 {
            // Mic delivers 10 ms chunks arriving 5 ms after capture.
            while cap_t + 10_000 + 5_000 <= tick {
                let chunk = vec![0.3f32; 480 * 2];
                bus.push_timed("mic", &chunk, Some(cap_t));
                cap_t += 10_000;
            }
            bus.pull_aligned("mic", 1600, &mut out, tick, Some(100_000));
            ages.push(bus.stats("mic").audio_age_ms);
            tick += 33_333;
        }
        let last = ages.last().unwrap().unwrap();
        assert!((last - 100.0).abs() <= 16.0, "settled age {last} ms, want ~100");
        assert!(bus.stats("mic").buffered_ms < 130.0);

        // A 500 ms backlog (stalled consumer) is dropped back to the
        // target age instead of being played late.
        let mut bus = AudioBus::new(30, 1);
        bus.push_timed("mic", &vec![0.3; 24_000 * 2], Some(500_000));
        bus.pull_aligned("mic", 1600, &mut out, 1_000_000, Some(100_000));
        let age = bus.stats("mic").audio_age_ms.unwrap();
        assert!((age - 100.0).abs() <= 16.0, "age after drop {age}");
    }

    #[test]
    fn untimed_sources_fall_back_to_lowest_latency() {
        let mut bus = AudioBus::new(30, 1);
        bus.push("file", &vec![0.2; 4000 * 2]);
        let mut out = Vec::new();
        bus.pull_aligned("file", 1600, &mut out, 5_000_000, Some(150_000));
        assert!(out.iter().all(|&v| v == 0.2));
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
