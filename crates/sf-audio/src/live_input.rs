//! Dedicated capture thread for live audio devices (OBS's per-source audio
//! thread).
//!
//! # Why
//!
//! Live device reads block: the dshow microphone producer waits up to 25 ms
//! per read for the next packet. The media tick used to call it directly
//! (twice per tick), so an idle or slow mic stalled the **whole** 33 ms
//! render/encode tick by up to 50 ms — the tick then missed deadlines,
//! skipped frames, and audio underran. Reads now happen on their own thread;
//! the tick only drains a mutex-guarded FIFO, which never blocks for longer
//! than a memcpy.
//!
//! The FIFO is bounded (oldest dropped) so a stalled consumer can never grow
//! memory or latency; the [`crate::bus::AudioBus`] applies the precise live
//! latency policy on top.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Device health as seen by the capture thread.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveStatus {
    /// Thread started, nothing delivered yet.
    Starting,
    /// Delivering audio.
    Running,
    /// Reads are failing (unplugged device, driver error). Keeps retrying.
    Error(String),
    /// Thread exited.
    Stopped,
}

struct Fifo {
    samples: VecDeque<f32>,
    /// `(first frame index, capture time µs)` of each pushed chunk, in
    /// order. Lets the consumer know the capture time of the oldest queued
    /// sample (A/V alignment + latency diagnostics).
    marks: VecDeque<(u64, i64)>,
    /// Absolute frame index of `samples[0]`.
    head_frame: u64,
}

impl Fifo {
    fn tail_frame(&self) -> u64 {
        self.head_frame + (self.samples.len() / 2) as u64
    }

    /// Drop `frames` stereo frames from the head.
    fn pop_frames(&mut self, frames: usize) {
        let n = (frames * 2).min(self.samples.len());
        self.samples.drain(..n);
        self.head_frame += (n / 2) as u64;
        // Keep the last mark at or before the new head (it still dates it).
        while self.marks.len() >= 2 && self.marks[1].0 <= self.head_frame {
            self.marks.pop_front();
        }
        if self.samples.is_empty() {
            self.marks.clear();
        }
    }

    fn head_capture_us(&self) -> Option<i64> {
        if self.samples.is_empty() {
            return None;
        }
        let (f, at) = *self.marks.front()?;
        if at == UNKNOWN_TIME {
            return None;
        }
        let off = self.head_frame.saturating_sub(f) as i64;
        Some(at + off * 1_000_000 / SAMPLE_RATE)
    }
}

struct Shared {
    fifo: Mutex<Fifo>,
    status: Mutex<LiveStatus>,
    stop: AtomicBool,
    dropped: AtomicU64,
    cap_samples: usize,
}

/// A live audio source running on its own thread. Dropping it signals the
/// thread to stop and **does not join**: the thread exits after its current
/// read on its own, so a wedged driver (unplugged device) can never hang
/// the caller (UI command / media tick).
pub struct LiveInput {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    label: String,
}

/// FIFO cap: 250 ms of 48 kHz stereo.
const DEFAULT_CAP_SAMPLES: usize = 48_000 / 4 * 2;
const SAMPLE_RATE: i64 = 48_000;
/// Marker for chunks without a capture timestamp.
const UNKNOWN_TIME: i64 = i64::MIN;

/// One chunk of captured audio: interleaved stereo 48 kHz plus the capture
/// time of its first sample on the pipeline clock (`sf_capture::clock`),
/// in µs.
pub struct TimedChunk {
    pub samples: Vec<f32>,
    pub captured_at_us: i64,
}

impl LiveInput {
    /// Spawn a capture thread around `read` for a backend without capture
    /// timestamps ([`Self::head_capture_us`] then reports `None`).
    pub fn spawn<F>(label: &str, mut read: F) -> std::io::Result<Self>
    where
        F: FnMut() -> Result<Option<Vec<f32>>, String> + Send + 'static,
    {
        Self::spawn_timed(label, move || {
            Ok(read()?.map(|samples| TimedChunk {
                samples,
                captured_at_us: UNKNOWN_TIME,
            }))
        })
    }

    /// Spawn a capture thread around `read`. `read` is called in a loop; it
    /// should return promptly with a chunk, `Ok(None)` when nothing is
    /// ready, or `Err` on device failure.
    pub fn spawn_timed<F>(label: &str, mut read: F) -> std::io::Result<Self>
    where
        F: FnMut() -> Result<Option<TimedChunk>, String> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            fifo: Mutex::new(Fifo {
                samples: VecDeque::with_capacity(DEFAULT_CAP_SAMPLES),
                marks: VecDeque::new(),
                head_frame: 0,
            }),
            status: Mutex::new(LiveStatus::Starting),
            stop: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            cap_samples: DEFAULT_CAP_SAMPLES,
        });
        let sh = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name(format!("sf-audio-in-{label}"))
            .spawn(move || {
                let mut consecutive_errors = 0u32;
                while !sh.stop.load(Ordering::Relaxed) {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut read))
                        .unwrap_or_else(|_| Err("capture backend panicked".to_string()));
                    match r {
                        Ok(Some(chunk)) => {
                            consecutive_errors = 0;
                            if chunk.samples.len() >= 2 {
                                let mut f = sh.fifo.lock().unwrap_or_else(|e| e.into_inner());
                                let at = f.tail_frame();
                                f.marks.push_back((at, chunk.captured_at_us));
                                let even = chunk.samples.len() & !1;
                                f.samples.extend(chunk.samples[..even].iter().copied());
                                let over = f.samples.len().saturating_sub(sh.cap_samples);
                                if over > 0 {
                                    let frames = over.div_ceil(2);
                                    f.pop_frames(frames);
                                    sh.dropped.fetch_add(frames as u64, Ordering::Relaxed);
                                }
                            }
                            set_status(&sh, LiveStatus::Running);
                        }
                        Ok(None) => {
                            // Non-blocking backends: poll every 2 ms.
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => {
                            consecutive_errors += 1;
                            set_status(&sh, LiveStatus::Error(e));
                            // Back off so a dead device doesn't burn a core.
                            let ms = (10u64 << consecutive_errors.min(4)).min(200);
                            std::thread::sleep(Duration::from_millis(ms));
                        }
                    }
                }
                set_status(&sh, LiveStatus::Stopped);
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
            label: label.to_string(),
        })
    }

    /// Move everything captured so far into `out` (appended). Never blocks
    /// beyond the FIFO lock.
    pub fn drain_into(&self, out: &mut Vec<f32>) {
        let mut f = self.shared.fifo.lock().unwrap_or_else(|e| e.into_inner());
        out.extend(f.samples.iter().copied());
        let frames = f.samples.len() / 2;
        f.pop_frames(frames);
    }

    /// Like [`Self::drain_into`], returning the capture time (pipeline
    /// clock, µs) of the first drained sample, if any were drained.
    pub fn drain_into_timed(&self, out: &mut Vec<f32>) -> Option<i64> {
        let mut f = self.shared.fifo.lock().unwrap_or_else(|e| e.into_inner());
        let head = f.head_capture_us();
        out.extend(f.samples.iter().copied());
        let frames = f.samples.len() / 2;
        f.pop_frames(frames);
        head
    }

    /// Capture time of the oldest queued sample, without draining.
    pub fn head_capture_us(&self) -> Option<i64> {
        self.shared
            .fifo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .head_capture_us()
    }

    /// Queued stereo frames.
    pub fn queued_frames(&self) -> usize {
        self.shared.fifo.lock().unwrap_or_else(|e| e.into_inner()).samples.len() / 2
    }

    pub fn status(&self) -> LiveStatus {
        self.shared
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(LiveStatus::Stopped)
    }

    /// Sample frames dropped because the consumer fell behind.
    pub fn dropped_frames(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

fn set_status(sh: &Shared, s: LiveStatus) {
    if let Ok(mut cur) = sh.status.lock() {
        if *cur != s {
            *cur = s;
        }
    }
}

impl Drop for LiveInput {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Detach: the thread sees `stop` after its current read and exits,
        // releasing the device. Never join — a wedged driver read would
        // otherwise hang whoever dropped us (UI thread / media tick).
        drop(self.handle.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn delivers_samples_without_blocking_consumer() {
        let mut n = 0.0f32;
        let input = LiveInput::spawn("test", move || {
            // Simulate a device that blocks 20 ms per 10 ms packet... of 480 frames.
            std::thread::sleep(Duration::from_millis(5));
            n += 1.0;
            Ok(Some(vec![n; 480 * 2]))
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let t = Instant::now();
        let mut out = Vec::new();
        input.drain_into(&mut out);
        assert!(
            t.elapsed() < Duration::from_millis(5),
            "drain must not block"
        );
        assert!(!out.is_empty());
        assert_eq!(out.len() % 2, 0);
        assert_eq!(input.status(), LiveStatus::Running);
    }

    #[test]
    fn fifo_is_bounded() {
        let input = LiveInput::spawn("flood", || Ok(Some(vec![0.0; 4800 * 2]))).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let mut out = Vec::new();
        input.drain_into(&mut out);
        assert!(out.len() <= DEFAULT_CAP_SAMPLES);
        assert!(input.dropped_frames() > 0);
    }

    #[test]
    fn errors_are_reported_and_drop_never_blocks() {
        let input = LiveInput::spawn("bad", || Err("device unplugged".into())).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(input.status(), LiveStatus::Error("device unplugged".into()));
        let t = Instant::now();
        drop(input);
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn drop_does_not_wait_for_a_wedged_read() {
        let input = LiveInput::spawn("wedged", || {
            std::thread::sleep(Duration::from_secs(3));
            Ok(None)
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let t = Instant::now();
        drop(input);
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn head_capture_time_tracks_queue_and_drops() {
        let mut at = 1_000_000i64;
        let input = LiveInput::spawn_timed("timed", move || {
            std::thread::sleep(Duration::from_millis(1));
            let c = TimedChunk { samples: vec![0.0; 480 * 2], captured_at_us: at };
            at += 10_000; // 10 ms chunks
            Ok(Some(c))
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Flooded faster than real time → FIFO capped at 250 ms; the head is
        // dated by the oldest *kept* sample, exactly.
        let head = input.head_capture_us().unwrap();
        assert_eq!((head - 1_000_000) % 10_000, 0, "head {head}");
        let mut out = Vec::new();
        let first = input.drain_into_timed(&mut out).unwrap();
        assert!(first >= head);
        let frames = out.len() as i64 / 2;
        assert!(frames <= 12_000);
        // Last drained sample's time ≈ first + duration.
        assert!(frames * 1_000_000 / 48_000 <= 250_000);
    }

    #[test]
    fn panic_in_backend_does_not_kill_thread() {
        let mut first = true;
        let input = LiveInput::spawn("panicky", move || {
            if first {
                first = false;
                panic!("boom");
            }
            Ok(Some(vec![0.25; 2]))
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let mut out = Vec::new();
        input.drain_into(&mut out);
        assert!(out.contains(&0.25));
    }
}
