//! Live diagnostics: every number here is *measured* in the running
//! pipeline (no estimates presented as measurements).
//!
//! Latency is split the way it really accrues:
//!
//! * **App side (measured):** picture age at render (device timestamp →
//!   tick; includes the device's own buffering as far as its timestamps
//!   show it) + processing (render + encode of the last tick) + send
//!   buffer (encoded video not yet written to the socket).
//! * **Platform side (not measurable from the app):** ingest, transcoding
//!   and player buffering at YouTube / Facebook / TikTok. Shown as guidance
//!   for the platform's own latency setting.

use serde::Serialize;

use crate::production::{SourceKind, StreamLifecycle};
use crate::Shell;

/// Real process CPU / memory sampling.
pub(crate) struct SysSampler {
    last_wall: std::time::Instant,
    last_cpu_s: f64,
    last: SysStats,
    cores: usize,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct SysStats {
    /// Process CPU, % of the whole machine (all cores = 100 %).
    pub cpu_percent: f32,
    /// Resident memory, MB.
    pub ram_mb: f32,
    /// Logical cores.
    pub cores: usize,
}

impl SysSampler {
    pub(crate) fn new() -> Self {
        Self {
            last_wall: std::time::Instant::now(),
            last_cpu_s: process_cpu_seconds().unwrap_or(0.0),
            last: SysStats::default(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    }

    /// Current stats; re-measured at most every 500 ms.
    pub(crate) fn sample(&mut self) -> SysStats {
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_wall).as_secs_f64();
        if dt >= 0.5 {
            if let Some(cpu) = process_cpu_seconds() {
                let pct = (cpu - self.last_cpu_s) / (dt * self.cores as f64) * 100.0;
                self.last.cpu_percent = pct.clamp(0.0, 100.0) as f32;
                self.last_cpu_s = cpu;
            }
            self.last.ram_mb = process_rss_bytes().unwrap_or(0) as f32 / (1024.0 * 1024.0);
            self.last.cores = self.cores;
            self.last_wall = now;
        }
        self.last
    }
}

#[cfg(target_os = "linux")]
fn process_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised comm; utime/stime are fields 14/15.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = f.get(11)?.parse().ok()?;
    let stime: f64 = f.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0) // USER_HZ is 100 on Linux
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[cfg(windows)]
mod win {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FileTime {
        pub low: u32,
        pub high: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct ProcessMemoryCounters {
        pub cb: u32,
        pub page_fault_count: u32,
        pub peak_working_set_size: usize,
        pub working_set_size: usize,
        pub quota_peak_paged_pool_usage: usize,
        pub quota_paged_pool_usage: usize,
        pub quota_peak_non_paged_pool_usage: usize,
        pub quota_non_paged_pool_usage: usize,
        pub pagefile_usage: usize,
        pub peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetCurrentProcess() -> isize;
        pub fn GetProcessTimes(
            h: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        pub fn K32GetProcessMemoryInfo(h: isize, c: *mut ProcessMemoryCounters, cb: u32) -> i32;
    }
}

#[cfg(windows)]
fn process_cpu_seconds() -> Option<f64> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    // SAFETY: plain Win32 call on the current-process pseudo handle with
    // valid out-pointers.
    let ok = unsafe { win::GetProcessTimes(win::GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return None;
    }
    let t = |f: win::FileTime| ((f.high as u64) << 32 | f.low as u64) as f64 / 1e7;
    Some(t(k) + t(u))
}

#[cfg(windows)]
fn process_rss_bytes() -> Option<u64> {
    let mut pmc = win::ProcessMemoryCounters {
        cb: std::mem::size_of::<win::ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: `pmc.cb` is set to the struct size as the API requires.
    let ok = unsafe { win::K32GetProcessMemoryInfo(win::GetCurrentProcess(), &mut pmc, pmc.cb) };
    (ok != 0).then_some(pmc.working_set_size as u64)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_cpu_seconds() -> Option<f64> {
    None
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_rss_bytes() -> Option<u64> {
    None
}

/// One video source.
#[derive(Clone, Debug, Serialize)]
pub struct SourceDiag {
    pub id: String,
    pub name: String,
    pub kind: String,
    /// New frames reaching the pipeline per second.
    pub fps: f32,
    /// Frames the capture thread published per second (live devices only).
    pub device_fps: Option<f32>,
    /// Video packets the camera delivered per second (its real output
    /// rate, before stale frames are dropped). Live devices only.
    pub device_packet_fps: Option<f32>,
    /// Decode / RGBA-convert time per frame, ms (live devices only).
    pub decode_ms: Option<f32>,
    pub convert_ms: Option<f32>,
    /// Device timestamp → mailbox, ms (queueing + decode inside capture).
    pub capture_latency_ms: Option<f32>,
    /// Longest gap between two published frames in the last second, ms.
    pub max_gap_ms: Option<f32>,
    pub decode_errors: u64,
    pub capture_errors: u64,
    /// Frames dropped at capture because newer ones arrived (live-first).
    pub dropped: u64,
    /// Picture age when rendered (smoothed), ms.
    pub age_ms: f32,
    pub width: u32,
    pub height: u32,
    pub format: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioDiag {
    pub id: String,
    pub name: String,
    pub device: String,
    pub buffered_ms: f32,
    pub audio_age_ms: Option<f32>,
    pub target_age_ms: Option<f32>,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub corrections: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EncoderDiag {
    pub name: String,
    pub hardware: bool,
    pub fps: f32,
    pub encode_ms: f32,
    pub skipped_slots: u64,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LatencyDiag {
    /// Program picture age at render (capture → tick), ms.
    pub capture_ms: Option<f32>,
    /// Render + encode of the last tick, ms.
    pub processing_ms: f32,
    /// Encoded video waiting for the socket, ms.
    pub send_buffer_ms: f32,
    /// Sum of the above: everything the app adds before the network.
    pub app_total_ms: Option<f32>,
    /// What the platform adds is outside the app's control.
    pub platform_note: String,
}

/// Everything the diagnostics panel and the status bar show.
#[derive(Clone, Debug, Serialize)]
pub struct DiagnosticsView {
    /// Output / render frame rate actually achieved.
    pub render_fps: f32,
    pub target_fps: u32,
    /// Last tick, total ms.
    pub tick_ms: f32,
    /// Last tick by stage, ms: capture, render (incl. GPU), gpu, audio, encode+send.
    pub capture_ms: f32,
    pub render_ms: f32,
    pub gpu_ms: f32,
    pub audio_ms: f32,
    pub encode_ms: f32,
    /// Ticks that missed their deadline (pipeline too slow).
    pub lagged_ticks: u64,
    /// Frames not delivered (render/encode failures).
    pub skipped_render: u64,
    /// Packets dropped because the network could not keep up.
    pub dropped_network: u64,
    pub sources: Vec<SourceDiag>,
    pub audio: Vec<AudioDiag>,
    pub encoder: Option<EncoderDiag>,
    /// "idle" | "connecting" | "live" | "reconnecting" | "error".
    pub stream_status: String,
    pub stream_error: Option<String>,
    pub stream_kbps: f32,
    pub stream_uptime_s: u64,
    pub recording: bool,
    pub latency: LatencyDiag,
    /// A grade / LUT / filter is active on a program source.
    pub color_active: bool,
    /// A/V sync: auto on, manual offset.
    pub av_auto: bool,
    pub av_offset_ms: i32,
    pub system: SysStats,
}

impl Shell {
    /// Full diagnostics snapshot (cheap: reads counters, renders nothing).
    pub fn diagnostics(&mut self) -> DiagnosticsView {
        let now = std::time::Instant::now();
        let (render_fps, ..) = {
            let cutoff = now - std::time::Duration::from_secs(1);
            while self
                .perf_frame_times
                .front()
                .is_some_and(|t| *t < cutoff)
            {
                self.perf_frame_times.pop_front();
            }
            (self.perf_frame_times.len() as f32, 0)
        };
        let ms = |us: u64| us as f32 / 1000.0;
        let [cap, ren, aud, enc] = self.perf_stages_us;

        let all_sources: Vec<_> = self
            .scenes
            .iter()
            .flat_map(|s| s.sources.iter())
            .cloned()
            .collect();
        let mut seen = std::collections::HashSet::new();
        let mut sources = Vec::new();
        for s in &all_sources {
            if !seen.insert(s.id.clone()) {
                continue;
            }
            let kind = match &s.kind {
                SourceKind::Camera { .. } => "camera",
                SourceKind::Screen => "screen",
                SourceKind::Media { .. } => "media",
                _ => continue,
            };
            let f = self.source_frames.get(&s.id);
            let live = self.media_producers.get(&s.id).and_then(|p| p.live_stats());
            sources.push(SourceDiag {
                id: s.id.clone(),
                name: s.name.clone(),
                kind: kind.to_string(),
                fps: f.map(|f| f.fps()).unwrap_or(0.0),
                device_fps: live.as_ref().map(|l| l.capture_fps),
                device_packet_fps: live.as_ref().map(|l| l.device_fps),
                decode_ms: live.as_ref().map(|l| l.decode_ms),
                convert_ms: live.as_ref().map(|l| l.convert_ms),
                capture_latency_ms: live.as_ref().map(|l| l.capture_latency_us as f32 / 1000.0),
                max_gap_ms: live.as_ref().map(|l| l.max_gap_ms),
                decode_errors: live.as_ref().map(|l| l.decode_errors).unwrap_or(0),
                capture_errors: live.as_ref().map(|l| l.capture_errors).unwrap_or(0),
                dropped: live.as_ref().map(|l| l.frames_dropped).unwrap_or(0),
                age_ms: f.map(|f| f.age_ema_us as f32 / 1000.0).unwrap_or(0.0),
                width: f.map(|f| f.width).unwrap_or(0),
                height: f.map(|f| f.height).unwrap_or(0),
                format: self.camera_formats.get(&s.id).cloned(),
                error: f
                    .and_then(|f| f.last_error.clone())
                    .or_else(|| live.and_then(|l| l.last_error)),
            });
        }

        let voice = self.voice_stats();
        let audio = voice
            .into_iter()
            .map(|(id, v)| AudioDiag {
                name: all_sources
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| id.clone()),
                id,
                device: v.device,
                buffered_ms: v.buffered_ms,
                audio_age_ms: v.audio_age_ms,
                target_age_ms: v.target_age_ms,
                underruns: v.underruns,
                dropped_frames: v.dropped_frames,
                corrections: v.corrections,
            })
            .collect();

        let encoder = self.output.as_ref().map(|o| EncoderDiag {
            name: o.encoder_name().to_string(),
            hardware: o.encoder_is_hardware(),
            fps: o.encoder_fps(),
            encode_ms: ms(o.last_encode_us()),
            skipped_slots: o.skipped_slots,
            note: self.encoder_note.clone(),
        });
        let send_buffer_ms = self
            .output
            .as_ref()
            .and_then(|o| o.stream.as_ref())
            .map(|s| ms(s.buffered_video_us().max(0) as u64))
            .unwrap_or(0.0);
        let (stream_status, stream_error) = match &self.stream_lifecycle {
            StreamLifecycle::Idle => ("idle", None),
            StreamLifecycle::Connecting => ("connecting", None),
            StreamLifecycle::Live => ("live", None),
            StreamLifecycle::Reconnecting => ("reconnecting", None),
            StreamLifecycle::Stopping => ("stopping", None),
            StreamLifecycle::Error { message } => ("error", Some(message.clone())),
        };
        let capture_ms = self.last_program_video_age_us.map(|a| a as f32 / 1000.0);
        let processing_ms = ms(ren + enc);
        let platform_note =
            crate::platforms::platform_info(self.stream_settings.platform).latency_note.to_string();
        let latency = LatencyDiag {
            capture_ms,
            processing_ms,
            send_buffer_ms,
            app_total_ms: capture_ms.map(|c| c + processing_ms + send_buffer_ms),
            platform_note,
        };
        let stream_kbps = self.upload_kbps(now);

        DiagnosticsView {
            render_fps,
            target_fps: self.video_settings.output_fps,
            tick_ms: ms(self.perf_last_tick_us),
            capture_ms: ms(cap),
            render_ms: ms(ren),
            gpu_ms: ms(self.perf_gpu_us_accum),
            audio_ms: ms(aud),
            encode_ms: ms(enc),
            lagged_ticks: self.perf_lagged_ticks,
            skipped_render: self.perf_skipped_render,
            dropped_network: self.perf_dropped_network,
            sources,
            audio,
            encoder,
            stream_status: stream_status.to_string(),
            stream_error,
            stream_kbps,
            stream_uptime_s: self
                .stream_started_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0),
            recording: self.record_runtime.is_some(),
            latency,
            color_active: self.program_color_active(),
            av_auto: self.av_sync.auto,
            av_offset_ms: self.av_sync.offset_ms,
            system: self.sys.sample(),
        }
    }

    /// True when any visible program source has a non-neutral grade or an
    /// enabled filter / look.
    fn program_color_active(&self) -> bool {
        let Some(scene) = self.scenes.iter().find(|s| s.id == self.program_scene_id) else {
            return false;
        };
        let neutral = sf_color::ColorGrade::default();
        scene
            .sources
            .iter()
            .filter(|s| s.visible)
            .filter_map(|s| s.input_id.as_ref())
            .any(|input| {
                let graded = self
                    .engine
                    .state()
                    .input_color(input)
                    .is_some_and(|c| c.grade != neutral);
                let filtered = self
                    .stack(&sf_ipc::FilterTarget::Input(input.clone()))
                    .is_ok_and(|st| st.slots().iter().any(|sl| sl.enabled));
                graded || filtered
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_and_memory_are_measured() {
        let mut s = SysSampler::new();
        let t = std::time::Instant::now();
        let mut x = 0u64;
        while t.elapsed() < std::time::Duration::from_millis(700) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let st = s.sample();
        eprintln!("{st:?} raw={:?}", process_cpu_seconds());
        assert!(st.cpu_percent > 5.0, "{st:?}");
        assert!(st.ram_mb > 1.0);
    }
}
