//! Local audio monitoring: play the master mix through the default
//! speakers/headphones.
//!
//! # OBS parity
//! OBS offers per-source audio monitoring ("Monitor Off" / "Monitor Only" /
//! "Monitor and Output") through WASAPI. We monitor the **master mix** to
//! the default output device via `cpal` (WASAPI on Windows, ALSA on Linux).
//!
//! # Realtime rules
//! * The tick pushes 48 kHz stereo f32 chunks into a bounded ring buffer
//!   (2 s cap, oldest dropped). The cpal callback drains it; underrun →
//!   silence, never a stall.
//! * When the device runs at a rate other than 48 kHz, the callback
//!   linear-resamples on the fly (most Windows devices are 48 kHz, so
//!   this is usually a straight copy).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Tick rate we produce (matches `sf-capture`'s normalized audio).
const TICK_RATE: u32 = 48_000;
/// Ring capacity in stereo frames (2 s at 48 kHz).
const RING_FRAMES: usize = 96_000;

struct Shared {
    /// Interleaved stereo f32 at [`TICK_RATE`].
    buf: VecDeque<f32>,
    device_rate: u32,
}

/// Local monitor: master mix → default output device.
pub struct AudioMonitor {
    // Held to keep the stream alive.
    _stream: cpal::Stream,
    shared: Arc<Mutex<Shared>>,
    /// Device sample rate (for diagnostics).
    pub device_rate: u32,
}

// SAFETY: `cpal::Stream` is `Send`; all shared state goes through the
// `Mutex`. The backend touches the monitor only through the shell `Mutex`.
unsafe impl Send for AudioMonitor {}

impl AudioMonitor {
    /// Open the default output device and start playing. Prefers a
    /// 48 kHz stereo f32 config (zero-resample path); otherwise takes the
    /// default config and resamples in the callback.
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default audio output device".to_string())?;
        let supported = device
            .supported_output_configs()
            .map_err(|e| format!("output configs: {e}"))?;
        // Prefer 48 kHz / stereo / f32.
        let mut chosen: Option<cpal::SupportedStreamConfig> = None;
        let mut fallback: Option<cpal::SupportedStreamConfigRange> = None;
        for cfg in supported {
            if cfg.channels() == 2 && cfg.sample_format() == cpal::SampleFormat::F32 {
                if fallback.is_none() {
                    fallback = Some(cfg.clone());
                }
                if cfg.min_sample_rate().0 <= TICK_RATE && TICK_RATE <= cfg.max_sample_rate().0 {
                    chosen = Some(cfg.with_sample_rate(cpal::SampleRate(TICK_RATE)));
                    break;
                }
            }
        }
        let (stream_config, sample_format): (cpal::StreamConfig, cpal::SampleFormat) =
            match chosen {
                Some(c) => {
                    let sf = c.sample_format();
                    (c.into(), sf)
                }
                None => match fallback {
                    Some(c) => {
                        let rate = c.max_sample_rate();
                        let sf = c.sample_format();
                        (c.with_sample_rate(rate).into(), sf)
                    }
                    None => {
                        let dc = device
                            .default_output_config()
                            .map_err(|e| format!("default output config: {e}"))?;
                        let sf = dc.sample_format();
                        (dc.into(), sf)
                    }
                },
            };
        let device_rate = stream_config.sample_rate.0;
        let shared = Arc::new(Mutex::new(Shared {
            buf: VecDeque::with_capacity(RING_FRAMES * 2),
            device_rate,
        }));
        let shared_cb = Arc::clone(&shared);
        let err_fn = |err| eprintln!("audio monitor stream error: {err}");
        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _| write_output(data, &shared_cb),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                &stream_config,
                move |data: &mut [i16], _| {
                    let mut tmp = vec![0.0f32; data.len()];
                    write_output(&mut tmp, &shared_cb);
                    for (d, s) in data.iter_mut().zip(tmp.iter()) {
                        *d = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                &stream_config,
                move |data: &mut [u16], _| {
                    let mut tmp = vec![0.0f32; data.len()];
                    write_output(&mut tmp, &shared_cb);
                    for (d, s) in data.iter_mut().zip(tmp.iter()) {
                        *d = ((s.clamp(-1.0, 1.0) * 32767.0) as i32 + 32768) as u16;
                    }
                },
                err_fn,
                None,
            ),
            _ => return Err("unsupported sample format".into()),
        }
        .map_err(|e| format!("build output stream: {e}"))?;
        stream.play().map_err(|e| format!("play: {e}"))?;
        Ok(Self {
            _stream: stream,
            shared,
            device_rate,
        })
    }

    /// Push a 48 kHz stereo f32 chunk from the media tick. Bounded:
    /// oldest audio is dropped when the ring is full (a slow device must
    /// never accumulate delay).
    pub fn push(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let Ok(mut s) = self.shared.lock() else {
            return;
        };
        for &v in samples {
            if s.buf.len() >= RING_FRAMES * 2 {
                s.buf.pop_front();
            }
            s.buf.push_back(v);
        }
    }
}

/// Fill `out` (interleaved stereo f32) from the ring, resampling
/// 48 kHz → device rate with linear interpolation. Underrun → silence.
fn write_output(out: &mut [f32], shared: &Arc<Mutex<Shared>>) {
    let Ok(mut s) = shared.lock() else {
        out.fill(0.0);
        return;
    };
    let frames_needed = out.len() / 2;
    if s.buf.len() < frames_needed * 2 {
        // Underrun: play what we have, pad silence. (Don't drain
        // partially — keep it simple and predictable.)
        out.fill(0.0);
        return;
    }
    if s.device_rate == TICK_RATE {
        for (d, v) in out.iter_mut().zip(s.buf.drain(..frames_needed * 2)) {
            *d = v;
        }
        return;
    }
    // Linear resample 48 kHz → device_rate.
    let ratio = TICK_RATE as f64 / s.device_rate as f64;
    let have_frames = s.buf.len() / 2;
    let frames_out = ((have_frames as f64 / ratio) as usize).min(frames_needed);
    // Drain everything we have (leftovers are cleared below anyway).
    let src: Vec<f32> = s.buf.drain(..).collect();
    let src_frames = src.len() / 2;
    for i in 0..frames_out {
        let pos = i as f64 * ratio;
        let i0 = pos as usize;
        let i1 = (i0 + 1).min(src_frames.saturating_sub(1));
        let t = (pos - i0 as f64) as f32;
        for ch in 0..2 {
            let a = src.get(i0 * 2 + ch).copied().unwrap_or(0.0);
            let b = src.get(i1 * 2 + ch).copied().unwrap_or(0.0);
            out[i * 2 + ch] = a + (b - a) * t;
        }
    }
    for v in out.iter_mut().skip(frames_out * 2) {
        *v = 0.0;
    }
    // Drop any leftover source frames we didn't consume (newest-wins:
    // the monitor must never fall behind realtime).
    s.buf.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_on_empty_buffer() {
        let shared = Arc::new(Mutex::new(Shared {
            buf: VecDeque::new(),
            device_rate: 48_000,
        }));
        let mut out = vec![9.9f32; 8];
        write_output(&mut out, &shared);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn passthrough_at_48k() {
        let shared = Arc::new(Mutex::new(Shared {
            buf: VecDeque::from(vec![0.5f32, -0.5, 0.25, -0.25]),
            device_rate: 48_000,
        }));
        let mut out = vec![0.0f32; 4];
        write_output(&mut out, &shared);
        assert_eq!(out, vec![0.5, -0.5, 0.25, -0.25]);
    }
}
