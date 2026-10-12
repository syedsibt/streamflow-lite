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

struct Shared {
    buf: Mutex<VecDeque<f32>>,
    status: Mutex<LiveStatus>,
    stop: AtomicBool,
    dropped: AtomicU64,
    cap_samples: usize,
}

/// A live audio source running on its own thread. Dropping it stops and
/// joins the thread (bounded by one device read timeout).
pub struct LiveInput {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    label: String,
}

/// FIFO cap: 250 ms of 48 kHz stereo.
const DEFAULT_CAP_SAMPLES: usize = 48_000 / 4 * 2;

impl LiveInput {
    /// Spawn a capture thread around `read`. `read` is called in a loop; it
    /// should block briefly (≤ ~25 ms) and return interleaved stereo 48 kHz
    /// samples, `Ok(None)` when nothing is ready, or `Err` on device failure.
    pub fn spawn<F>(label: &str, mut read: F) -> std::io::Result<Self>
    where
        F: FnMut() -> Result<Option<Vec<f32>>, String> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            buf: Mutex::new(VecDeque::with_capacity(DEFAULT_CAP_SAMPLES)),
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
                        Ok(Some(samples)) => {
                            consecutive_errors = 0;
                            {
                                let mut b = sh.buf.lock().unwrap_or_else(|e| e.into_inner());
                                b.extend(samples.iter().copied());
                                let over = b.len().saturating_sub(sh.cap_samples);
                                if over > 0 {
                                    let over = over + over % 2; // keep L/R aligned
                                    let n = over.min(b.len());
                                    b.drain(..n);
                                    sh.dropped.fetch_add(over as u64 / 2, Ordering::Relaxed);
                                }
                            }
                            set_status(&sh, LiveStatus::Running);
                        }
                        Ok(None) => {
                            // Nothing ready; the read itself usually blocked
                            // briefly — yield a little to avoid spinning on
                            // backends that return immediately.
                            std::thread::sleep(Duration::from_millis(1));
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
        let mut b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
        out.extend(b.drain(..));
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
        if let Some(h) = self.handle.take() {
            // The read loop re-checks `stop` after each (≤25 ms) read.
            let _ = h.join();
        }
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
    fn errors_are_reported_and_drop_joins() {
        let input = LiveInput::spawn("bad", || Err("device unplugged".into())).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(input.status(), LiveStatus::Error("device unplugged".into()));
        let t = Instant::now();
        drop(input);
        assert!(t.elapsed() < Duration::from_millis(500));
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
