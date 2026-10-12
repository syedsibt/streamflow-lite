//! Windows microphone / line-in capture via FFmpeg's `dshow` audio device.
//!
//! # OBS parity
//! OBS captures audio through WASAPI (`plugins/win-wasapi`): an
//! event-driven `IAudioClient` capture thread, with microphones on
//! `eCapture` and desktop audio on `eRender` +
//! `AUDCLNT_STREAMFLAGS_LOOPBACK`, pushing timestamped buffers through
//! `obs_source_output_audio`. We reach the same physical devices through
//! FFmpeg's dshow audio input (`audio="Device Name"`) and normalize to the
//! engine's 48 kHz stereo f32 format with the same decode+resample
//! machinery media files use.
//!
//! # Realtime rules (the blueprint's core: never let frames accumulate)
//! * The chunk queue is bounded (8) and newest-wins: a slow consumer drops
//!   backlog instead of growing memory.
//! * Every read is bounded by a 25 ms interrupt deadline — a stalled
//!   device reports "no audio yet", never hangs the media tick.
//!
//! # Status
//! Code-complete; **hardware-unverified** — no Windows microphone exists on
//! the CI/dev machines. The open path follows FFmpeg's documented `dshow`
//! audio API; decode/resample is the tested machinery from `demux.rs`.

use std::collections::VecDeque;
use std::ffi::CString;
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ffmpeg_next as ff;

use super::demux::{ensure_init, pts_to_us};
use super::media::{DecodedAudio, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use super::CaptureError;

/// Max queued audio chunks. dshow delivers small packets; 8 chunks is
/// ~170 ms of 48 kHz audio — plenty for a 33 ms tick, bounded forever.
const MAX_QUEUE: usize = 8;

/// A live device that stops delivering must not hang the tick.
const READ_TIMEOUT_US: u64 = 25_000;
/// Opening a missing/unplugged device must fail fast, not hang.
const OPEN_TIMEOUT_US: u64 = 5_000_000;

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

struct ReadDeadline {
    deadline_us: AtomicU64,
}

unsafe extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let rd = unsafe { &*(opaque as *const ReadDeadline) };
    let dl = rd.deadline_us.load(Ordering::Relaxed);
    if dl != 0 && now_us() >= dl {
        1 // abort the blocking read
    } else {
        0
    }
}

/// Live microphone / line-in audio producer (Windows dshow).
pub struct AudioInputProducer {
    ictx: ff::format::context::Input,
    deadline: Arc<ReadDeadline>,
    audio_stream: usize,
    decoder: ff::decoder::Audio,
    resampler: ff::software::resampling::Context,
    audio_tb: (i32, i32),
    queue: VecDeque<DecodedAudio>,
    samples_out: u64,
    device_name: String,
}

// SAFETY: FFmpeg contexts have no thread affinity; the backend serializes
// all producer access through the shell `Mutex`.
unsafe impl Send for AudioInputProducer {}

impl AudioInputProducer {
    /// Open the named dshow audio device (`list_audio_devices` names).
    /// `None` picks the first available device (OBS "default" equivalent).
    pub fn open(device: Option<&str>) -> Result<Self, CaptureError> {
        ensure_init();
        let name = match device {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => super::dshow::list_audio_devices()?
                .into_iter()
                .next()
                .ok_or_else(|| CaptureError::OpenFailed("no audio input devices found".into()))?,
        };
        let fmt_name = CString::new("dshow").unwrap();
        let fmt = unsafe { ff::ffi::av_find_input_format(fmt_name.as_ptr()) };
        if fmt.is_null() {
            return Err(CaptureError::OpenFailed(
                "FFmpeg was built without dshow support".into(),
            ));
        }
        let url = CString::new(format!("audio={name}"))
            .map_err(|_| CaptureError::OpenFailed("device name contains NUL".into()))?;

        let deadline = Arc::new(ReadDeadline {
            deadline_us: AtomicU64::new(now_us() + OPEN_TIMEOUT_US),
        });
        let mut ctx: *mut ff::ffi::AVFormatContext =
            unsafe { ff::ffi::avformat_alloc_context() };
        if ctx.is_null() {
            return Err(CaptureError::OpenFailed("avformat_alloc_context failed".into()));
        }
        unsafe {
            (*ctx).interrupt_callback.callback = Some(interrupt_cb);
            (*ctx).interrupt_callback.opaque = Arc::as_ptr(&deadline) as *mut c_void;
        }
        let mut dict: *mut ff::ffi::AVDictionary = ptr::null_mut();
        let mut set_opt = |k: &str, v: &str| unsafe {
            let key = CString::new(k).unwrap();
            let val = CString::new(v).unwrap();
            ff::ffi::av_dict_set(&mut dict, key.as_ptr(), val.as_ptr(), 0);
        };
        // DirectShow's default capture buffer is ~500 ms: the mic is then
        // delivered in half-second lumps, i.e. every word arrives 0.5 s
        // late and in bursts. 20 ms packets (dshow `audio_buffer_size`, in
        // ms) is what OBS-class low latency needs and is supported by all
        // WDM audio drivers.
        set_opt("audio_buffer_size", "20");
        // Live: no internal buffering (the lag lesson from cameras), tiny probe.
        set_opt("fflags", "nobuffer");
        set_opt("probesize", "32");
        set_opt("analyzeduration", "0");
        let ret = unsafe {
            ff::ffi::avformat_open_input(
                &mut ctx,
                url.as_ptr(),
                fmt as *mut ff::ffi::AVInputFormat,
                &mut dict,
            )
        };
        unsafe { ff::ffi::av_dict_free(&mut dict) };
        if ret < 0 || ctx.is_null() {
            // `avformat_open_input` frees `ctx` on failure; do not touch it.
            return Err(CaptureError::OpenFailed(format!(
                "cannot open audio device '{name}'"
            )));
        }
        let si_ret = unsafe { ff::ffi::avformat_find_stream_info(ctx, ptr::null_mut()) };
        // Open phase done — disarm; per-read timeouts are armed in `next_audio`.
        deadline.deadline_us.store(0, Ordering::Relaxed);
        if si_ret < 0 {
            unsafe { ff::ffi::avformat_close_input(&mut ctx) };
            return Err(CaptureError::OpenFailed(
                "avformat_find_stream_info failed".into(),
            ));
        }
        // The interrupt callback stays installed for the context's lifetime;
        // `deadline` (Arc) is owned by `Self`, so the opaque pointer stays
        // valid until drop.
        //
        // SAFETY: `ctx` is a valid open input context owned by us now;
        // `Input::wrap` takes ownership and closes it on drop.
        let ictx = unsafe { ff::format::context::Input::wrap(ctx) };
        let audio_stream = ictx
            .streams()
            .best(ff::media::Type::Audio)
            .ok_or_else(|| CaptureError::OpenFailed("audio device has no audio stream".into()))?;
        let audio_index = audio_stream.index();
        let tb = audio_stream.time_base();
        let mut decoder = ff::codec::context::Context::from_parameters(audio_stream.parameters())
            .map_err(|e| CaptureError::OpenFailed(format!("audio codec params: {e}")))?
            .decoder()
            .audio()
            .map_err(|e| CaptureError::OpenFailed(format!("audio decoder open: {e}")))?;
        if decoder.channel_layout().channels() == 0 {
            let _ = decoder.set_channel_layout(ff::channel_layout::ChannelLayout::STEREO);
        }
        let resampler = ff::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            ff::util::format::sample::Sample::F32(ff::util::format::sample::Type::Packed),
            ff::channel_layout::ChannelLayout::STEREO,
            AUDIO_SAMPLE_RATE,
        )
        .map_err(|e| CaptureError::OpenFailed(format!("swresample setup: {e}")))?;

        Ok(Self {
            ictx,
            deadline,
            audio_stream: audio_index,
            decoder,
            resampler,
            audio_tb: (tb.numerator(), tb.denominator()),
            queue: VecDeque::with_capacity(MAX_QUEUE),
            samples_out: 0,
            device_name: name,
        })
    }

    /// Device name, for diagnostics.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Pull the next normalized audio chunk (48 kHz stereo f32).
    /// `Ok(None)` = no chunk available within the read timeout.
    pub fn next_audio(&mut self) -> Result<Option<DecodedAudio>, CaptureError> {
        if let Some(c) = self.queue.pop_front() {
            return Ok(Some(c));
        }
        // Bounded read: the interrupt callback aborts a stalled device.
        // Collect a small burst of packets first (ending the `&self.ictx`
        // borrow) so decoding can take `&mut self`.
        self.deadline
            .deadline_us
            .store(now_us() + READ_TIMEOUT_US, Ordering::Relaxed);
        let mut burst: Vec<ff::Packet> = Vec::new();
        for (stream, packet) in self.ictx.packets() {
            if stream.index() == self.audio_stream {
                burst.push(packet);
                if burst.len() >= 4 {
                    break;
                }
            }
        }
        self.deadline.deadline_us.store(0, Ordering::Relaxed);
        for packet in &burst {
            self.decoder
                .send_packet(packet)
                .map_err(|e| CaptureError::ReadFailed(format!("audio send_packet: {e}")))?;
            self.drain_decoder()?;
        }
        Ok(self.queue.pop_front())
    }

    fn drain_decoder(&mut self) -> Result<(), CaptureError> {
        loop {
            let mut decoded = ff::util::frame::audio::Audio::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(ff::util::error::Error::Eof) => break,
                Err(ff::util::error::Error::Other { errno: 11 }) => break,
                Err(e) => {
                    return Err(CaptureError::ReadFailed(format!(
                        "audio receive_frame: {e}"
                    )))
                }
            }
            let max_out = decoded.samples() as usize * 2 + 1024;
            let mut out = ff::util::frame::audio::Audio::new(
                ff::util::format::sample::Sample::F32(ff::util::format::sample::Type::Packed),
                max_out,
                ff::channel_layout::ChannelLayout::STEREO,
            );
            self.resampler
                .run(&decoded, &mut out)
                .map_err(|e| CaptureError::ReadFailed(format!("swresample run: {e}")))?;
            let n = out.samples() as usize * AUDIO_CHANNELS as usize;
            let bytes = out.data(0);
            let floats: &[f32] = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr() as *const f32, n.min(bytes.len() / 4))
            };
            let pts_us = pts_to_us(
                decoded.pts(),
                self.audio_tb.0,
                self.audio_tb.1,
                (self.samples_out * 1_000_000 / AUDIO_SAMPLE_RATE as u64) as i64,
            );
            self.samples_out += out.samples() as u64;
            // Newest-wins: never accumulate backlog.
            if self.queue.len() >= MAX_QUEUE {
                self.queue.pop_front();
            }
            self.queue.push_back(DecodedAudio {
                samples: floats.to_vec(),
                channels: AUDIO_CHANNELS,
                sample_rate: AUDIO_SAMPLE_RATE,
                pts_us,
            });
        }
        Ok(())
    }
}
