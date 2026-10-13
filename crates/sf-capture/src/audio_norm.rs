//! Audio normalisation to 48 kHz interleaved stereo f32.
//!
//! Built from each decoded frame's *actual* format / channel count / rate,
//! not from the stream header. A DirectShow microphone reports only a
//! channel count (layout order "unspecified"); the old code then forced a
//! stereo layout on the decoder, built swresample for stereo input, and
//! `swr_convert_frame` rejected every real (mono) frame with
//! `AVERROR_INPUT_CHANGED` ("swresample run: Input changed"), so no audio
//! ever reached the mixer. Here an unspecified layout becomes the default
//! layout for its channel count, and the converter is rebuilt whenever the
//! device changes format mid-stream.

use ffmpeg_next as ff;
use ff::ffi;

use super::media::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use super::CaptureError;

pub(crate) struct AudioNormalizer {
    swr: *mut ffi::SwrContext,
    /// (sample format, rate, channels, layout mask or 0) the context was built for.
    key: Option<(i32, i32, i32, u64)>,
    /// Times the input format changed after the first frame.
    pub(crate) reconfigured: u32,
}

// SAFETY: the context is owned by one producer, used from one thread at a time.
unsafe impl Send for AudioNormalizer {}

impl Default for AudioNormalizer {
    fn default() -> Self {
        Self { swr: std::ptr::null_mut(), key: None, reconfigured: 0 }
    }
}

impl Drop for AudioNormalizer {
    fn drop(&mut self) {
        if !self.swr.is_null() {
            // SAFETY: allocated by swr_alloc_set_opts2.
            unsafe { ffi::swr_free(&mut self.swr) };
        }
    }
}

impl AudioNormalizer {
    /// Converts one decoded frame to interleaved stereo f32 at 48 kHz.
    pub(crate) fn convert(&mut self, frame: &ff::util::frame::audio::Audio) -> Result<Vec<f32>, CaptureError> {
        // SAFETY: `frame` wraps a valid decoded AVFrame.
        let f = unsafe { &*frame.as_ptr() };
        let channels = f.ch_layout.nb_channels;
        let nb = f.nb_samples;
        if channels <= 0 || nb <= 0 || f.sample_rate <= 0 || f.format < 0 {
            return Ok(Vec::new());
        }
        let native = f.ch_layout.order == ffi::AVChannelOrder::AV_CHANNEL_ORDER_NATIVE;
        // SAFETY: reading the mask union member only for native order.
        let mask = if native { unsafe { f.ch_layout.u.mask } } else { 0 };
        let key = (f.format, f.sample_rate, channels, mask);
        if self.key != Some(key) {
            if self.key.is_some() {
                self.reconfigured += 1;
            }
            self.rebuild(f, key)?;
        }
        // SAFETY: valid, initialised context.
        let cap = unsafe { ffi::swr_get_out_samples(self.swr, nb) }.max(nb * 2) + 64;
        let mut out = vec![0f32; cap as usize * AUDIO_CHANNELS as usize];
        let mut out_planes = [out.as_mut_ptr() as *mut u8];
        // SAFETY: `extended_data` holds the frame's planes (or the single
        // packed buffer); `out` has room for `cap` interleaved frames.
        let n = unsafe {
            ffi::swr_convert(
                self.swr,
                out_planes.as_mut_ptr(),
                cap,
                f.extended_data as *mut *const u8,
                nb,
            )
        };
        if n < 0 {
            // Drop the context so the next frame rebuilds it cleanly.
            self.key = None;
            return Err(CaptureError::ReadFailed(format!("audio resample failed ({n})")));
        }
        out.truncate(n as usize * AUDIO_CHANNELS as usize);
        Ok(out)
    }

    fn rebuild(&mut self, f: &ffi::AVFrame, key: (i32, i32, i32, u64)) -> Result<(), CaptureError> {
        if !self.swr.is_null() {
            // SAFETY: previously allocated context.
            unsafe { ffi::swr_free(&mut self.swr) };
        }
        // SAFETY: zeroed AVChannelLayout is a valid "uninitialised" layout
        // for the constructor functions; every layout is uninit'ed below.
        unsafe {
            let mut in_layout: ffi::AVChannelLayout = std::mem::zeroed();
            if f.ch_layout.order == ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC
                || ffi::av_channel_layout_copy(&mut in_layout, &f.ch_layout) < 0
            {
                ffi::av_channel_layout_uninit(&mut in_layout);
                ffi::av_channel_layout_default(&mut in_layout, f.ch_layout.nb_channels);
            }
            let mut out_layout: ffi::AVChannelLayout = std::mem::zeroed();
            ffi::av_channel_layout_default(&mut out_layout, AUDIO_CHANNELS as i32);
            let mut swr: *mut ffi::SwrContext = std::ptr::null_mut();
            let r = ffi::swr_alloc_set_opts2(
                &mut swr,
                &out_layout,
                ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                AUDIO_SAMPLE_RATE as i32,
                &in_layout,
                std::mem::transmute::<i32, ffi::AVSampleFormat>(f.format),
                f.sample_rate,
                0,
                std::ptr::null_mut(),
            );
            ffi::av_channel_layout_uninit(&mut in_layout);
            ffi::av_channel_layout_uninit(&mut out_layout);
            if r < 0 || swr.is_null() {
                return Err(CaptureError::ReadFailed(format!("audio resampler setup failed ({r})")));
            }
            let r = ffi::swr_init(swr);
            if r < 0 {
                ffi::swr_free(&mut swr);
                return Err(CaptureError::ReadFailed(format!(
                    "audio resampler init failed ({r}) for {} ch {} Hz",
                    f.ch_layout.nb_channels, f.sample_rate
                )));
            }
            self.swr = swr;
        }
        self.key = Some(key);
        Ok(())
    }
}
