//! OBS-style asynchronous RTMP send path.
//!
//! In OBS the network send never happens on the video tick: encoder packets
//! flow through an interleaver into a dedicated send thread
//! (`plugins/obs-outputs/rtmp-stream.c:632`), so a stalled network can never
//! stall rendering. Previously `Shell::stream_tick` called
//! `Streamer::send_packet` inline on the 33 ms media tick — one slow socket
//! write jittered the whole pipeline.
//!
//! [`SendWorker`] owns the [`Streamer`] on its own thread. The media tick
//! only does `try_send` (never blocks); the worker owns the connect /
//! reconnect backoff state machine. Tick-side, an OBS-style time-based drop
//! policy sheds non-keyframe video when the worker's buffered span exceeds
//! 900 ms — audio and keyframes are never dropped.

use sf_encode::EncodedPacket;
use sf_output::{redact_url, RealTransport, StreamState, Streamer};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Tick → worker packet queue bound. The time-based drop policy sheds load
/// long before this fills; a full queue means the worker is wedged.
pub const SEND_QUEUE_CAP: usize = 256;

/// OBS `check_to_drop_frames` (rtmp-stream.c): drop B-frames past 700 ms of
/// buffered output, P-frames past 900 ms. Our encoder emits no B-frames, so a
/// single 900 ms threshold covers non-keyframe video. Audio and keyframes are
/// never dropped.
pub const DROP_VIDEO_BUFFER_US: i64 = 900_000;

/// Messages from the media tick to the send thread. `try_send` only — the
/// tick must never block on the network.
pub enum SendMsg {
    Packet(EncodedPacket),
    Shutdown,
}

/// Messages from the send thread back to the media tick.
pub enum SendEvent {
    /// One packet reached the network. `bytes_sent` is the worker's running
    /// total (drives the kbps meter); `dts_us` lets the tick pop its
    /// exact pending-packet accounting.
    Ack { dts_us: i64, bytes_sent: u64 },
    /// Worker status changed (connect → live → reconnecting → failed).
    Status(WorkerStatus),
    /// Fresh (re)connect: tick must forget pending packets (the worker
    /// dropped stale ones) and start the encoder over so the first packet
    /// is a keyframe.
    Resync,
    /// Worker exited after [`SendMsg::Shutdown`].
    Stopped { bytes_sent: u64 },
}

/// Send-thread status, mirrored into [`crate::production::StreamLifecycle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerStatus {
    Connecting,
    Live,
    Reconnecting,
    Failed(String),
}

/// `Streamer<RealTransport>` with an explicit `Send` impl: the FFmpeg output
/// context is `!Send` only by conservatism (raw pointers); it carries no
/// thread affinity and is touched solely by the send thread, so moving it
/// there is sound. Same pattern as the encoder `Send` wrappers.
struct SendStreamer(Streamer<RealTransport>);

// SAFETY: the streamer is only ever touched on the send thread it is moved
// to; no other thread retains a reference.
unsafe impl Send for SendStreamer {}

/// Owns the RTMP [`Streamer`] on a dedicated thread.
pub struct SendWorker {
    tx: SyncSender<SendMsg>,
    rx: Receiver<SendEvent>,
    handle: Option<JoinHandle<()>>,
}

impl SendWorker {
    /// Spawns the send thread, which immediately starts connecting (async —
    /// the tick never blocks on TCP connect).
    pub fn spawn(url: String, key: String, streamer: Streamer<RealTransport>) -> Self {
        let (tx, rx) = mpsc::sync_channel(SEND_QUEUE_CAP);
        let (etx, erx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("sf-rtmp-send".to_string())
            .spawn(move || send_thread_main(SendStreamer(streamer), rx, etx, url, key))
            .expect("RTMP send thread must spawn");
        Self {
            tx: tx,
            rx: erx,
            handle: Some(handle),
        }
    }

    /// Non-blocking packet handoff. `Err` when the queue is full (backstop)
    /// or the worker is gone.
    pub fn try_send(&self, packet: EncodedPacket) -> Result<(), TrySendError<SendMsg>> {
        self.tx.try_send(SendMsg::Packet(packet))
    }

    /// Drain all pending worker events (call once per media tick).
    pub fn drain_events(&self) -> Vec<SendEvent> {
        self.rx.try_iter().collect()
    }

    /// Asks the worker to stop and returns immediately. The worker writes
    /// the FLV trailer and closes the socket on its own thread (bounded by
    /// the transport's I/O timeouts), so stopping a stream on a dead
    /// network never freezes the UI.
    pub fn shutdown_detached(mut self) {
        // `send` (not try_send): the queue may be full of packets; a
        // blocked send would stall, so fall back to dropping the sender —
        // the worker treats a disconnected channel as shutdown.
        let _ = self.tx.try_send(SendMsg::Shutdown);
        drop(self.handle.take());
    }

    /// Asks the worker to stop and waits (bounded) for its final byte count.
    #[allow(dead_code)]
    pub fn shutdown(mut self) -> u64 {
        let _ = self.tx.send(SendMsg::Shutdown);
        let mut bytes = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(SendEvent::Stopped { bytes_sent }) => {
                    bytes = bytes_sent;
                    break;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        bytes
    }
}

/// Redact the key out of a worker-side error message (mirrors
/// `sanitize_stream_error`, which lives on the tick side).
fn worker_error(url: &str, key: &str, what: &str, err: &dyn std::fmt::Display) -> String {
    let mut msg = format!("{what} ({}): {err}", redact_url(url));
    if !key.is_empty() {
        msg = msg.replace(key, "[redacted]");
    }
    msg
}

fn send_thread_main(
    mut streamer: SendStreamer,
    rx: Receiver<SendMsg>,
    tx: mpsc::Sender<SendEvent>,
    url: String,
    key: String,
) {
    let report = |s: WorkerStatus| {
        let _ = tx.send(SendEvent::Status(s));
    };
    let mut last_reported: Option<WorkerStatus> = None;
    let mut report_status = |s: WorkerStatus| {
        if last_reported.as_ref() != Some(&s) {
            last_reported = Some(s.clone());
            report(s);
        }
    };
    let mut last_err = String::new();

    // Initial connect. Blocking TCP happens here, on this thread only —
    // the media tick keeps rendering at full cadence meanwhile.
    report_status(WorkerStatus::Connecting);
    if let Err(e) = streamer.0.start() {
        last_err = worker_error(&url, &key, "RTMP connect failed", &e);
    }

    loop {
        match streamer.0.state() {
            StreamState::Live => {
                report_status(WorkerStatus::Live);
                // Drop anything queued while reconnecting: stale timestamps
                // must never hit the fresh connection — except Shutdown.
                let mut shutdown = false;
                while let Ok(msg) = rx.try_recv() {
                    if matches!(msg, SendMsg::Shutdown) {
                        shutdown = true;
                        break;
                    }
                }
                if shutdown {
                    break;
                }
                let _ = tx.send(SendEvent::Resync);
                // Pump packets until a send fails or shutdown arrives.
                loop {
                    match rx.recv() {
                        Ok(SendMsg::Packet(p)) => {
                            let dts = p.dts_us;
                            match streamer.0.send_packet(&p) {
                                Ok(()) => {
                                    let _ = tx.send(SendEvent::Ack {
                                        dts_us: dts,
                                        bytes_sent: streamer.0.bytes_sent(),
                                    });
                                }
                                Err(e) => {
                                    last_err =
                                        worker_error(&url, &key, "RTMP send failed", &e);
                                    break;
                                }
                            }
                        }
                        Ok(SendMsg::Shutdown) | Err(_) => {
                            shutdown = true;
                            break;
                        }
                    }
                }
                if shutdown {
                    break;
                }
                // send_packet failed → state is Reconnecting or Failed; the
                // outer loop drives the backoff below.
            }
            StreamState::Reconnecting { .. } => {
                report_status(WorkerStatus::Reconnecting);
                let delay = streamer.0.reconnect_delay_ms().unwrap_or(1000);
                match rx.recv_timeout(Duration::from_millis(delay)) {
                    Ok(SendMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(SendMsg::Packet(_)) => {
                        // Not live: drop. The tick-side drop policy already
                        // shed these; anything here is overflow.
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if let Err(e) = streamer.0.on_reconnect_timer() {
                            last_err =
                                worker_error(&url, &key, "reconnect failed", &e);
                        }
                    }
                }
            }
            StreamState::Failed => {
                let msg = if last_err.is_empty() {
                    worker_error(&url, &key, "reconnect attempts exhausted", &"")
                } else {
                    last_err.clone()
                };
                report_status(WorkerStatus::Failed(msg));
                // Park: drop everything until Shutdown.
                for msg in rx {
                    if matches!(msg, SendMsg::Shutdown) {
                        break;
                    }
                }
                break;
            }
            StreamState::Idle | StreamState::Connecting => {
                // Transient (start()/try_connect() are synchronous); nudge
                // forward defensively.
                if let Err(e) = streamer.0.start() {
                    last_err = worker_error(&url, &key, "RTMP connect failed", &e);
                }
            }
        }
    }

    let bytes = streamer.0.bytes_sent();
    streamer.0.stop();
    let _ = tx.send(SendEvent::Stopped { bytes_sent: bytes });
}
