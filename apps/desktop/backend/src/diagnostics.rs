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

pub use crate::sysmon::SysStats;
#[cfg(test)]
use crate::sysmon::{process_cpu_seconds, SysSampler};

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
    /// Not on Preview/Program: capture paused (no decoding) until shown.
    pub paused: bool,
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
    /// Contained panics / recovered failures since start, and the last one.
    pub internal_errors: u64,
    pub last_internal_error: Option<String>,
    /// GPU validation/runtime errors since start, the last one, and the
    /// adapter in use (a `Cpu` type = software rendering).
    pub gpu_errors: u64,
    pub last_gpu_error: Option<String>,
    pub gpu_adapter: Option<String>,
    /// App-side caches and buffers with their current size.
    pub caches: Vec<CacheStat>,
}

impl Shell {
    /// Full diagnostics snapshot (cheap: reads counters, renders nothing).
    pub fn diagnostics(&mut self) -> DiagnosticsView {
        let (internal_errors, last_internal_error) = crate::health::status();
        let (gpu_errors, last_gpu_error, gpu_adapter) = sf_engine::gpu_status();
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
            if !seen.insert(self.frame_owner(&s.id).to_string()) {
                continue;
            }
            let kind = match &s.kind {
                SourceKind::Camera { .. } => "camera",
                SourceKind::Screen => "screen",
                SourceKind::Media { .. } => "media",
                _ => continue,
            };
            let owner = self.frame_owner(&s.id).to_string();
            let f = self.source_frames.get(&owner);
            let live = self.media_producers.get(&owner).and_then(|p| p.live_stats());
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
                paused: self.paused_sources.contains(&owner),
                dropped: live.as_ref().map(|l| l.frames_dropped).unwrap_or(0),
                age_ms: f.map(|f| f.age_ema_us as f32 / 1000.0).unwrap_or(0.0),
                width: f.map(|f| f.width).unwrap_or(0),
                height: f.map(|f| f.height).unwrap_or(0),
                format: self.camera_formats.get(&owner).cloned(),
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
            system: self.sys.latest(),
            internal_errors,
            last_internal_error,
            gpu_errors,
            last_gpu_error,
            gpu_adapter,
            caches: self.resource_report(),
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
        #[cfg(target_os = "linux")]
        {
            assert!(st.system_cpu_percent.is_some(), "{st:?}");
            assert!(st.mem_total_mb.unwrap_or(0.0) > st.mem_used_mb.unwrap_or(0.0));
            assert!(st.gpu_percent.is_none() && st.gpu_note.is_some(), "no fake GPU value");
        }
    }
}

/// One in-memory cache or buffer and what it holds right now.
#[derive(Clone, Debug, Serialize)]
pub struct CacheStat {
    pub name: &'static str,
    pub entries: usize,
    pub bytes: u64,
}

impl Shell {
    /// Every app-side cache/buffer with its current size, so abnormal growth
    /// is visible (Diagnostics) and testable (long-run benchmark). GPU-side
    /// pools inside the engine are not included.
    pub fn resource_report(&self) -> Vec<CacheStat> {
        let b = |v: &Vec<u8>| v.len() as u64;
        let mut out = vec![
            CacheStat {
                name: "source frames (newest per source)",
                entries: self.source_frames.len(),
                bytes: self.source_frames.values().map(|f| f.data.len() as u64).sum(),
            },
            CacheStat {
                name: "resized source frames",
                entries: self.resized_frames.len(),
                bytes: self.resized_frames.values().map(|(_, p)| b(p)).sum(),
            },
            CacheStat {
                name: "decoded images",
                entries: self.image_cache.len(),
                bytes: self.image_cache.values().map(|i| i.data.len() as u64).sum(),
            },
            CacheStat {
                name: "resized images",
                entries: self.image_sized.len(),
                bytes: self.image_sized.values().map(|p| b(p)).sum(),
            },
            CacheStat {
                name: "text rasters",
                entries: self.text_cache.len(),
                bytes: self.text_cache.values().map(b).sum(),
            },
            CacheStat {
                name: "test signal base",
                entries: self.test_pattern.entries(),
                bytes: self.test_pattern.bytes(),
            },
            CacheStat {
                name: "color-tool last frames",
                entries: self.last_frame.len(),
                bytes: self.last_frame.values().map(|f| f.pixels.len() as u64).sum(),
            },
            CacheStat {
                name: "color-tool preview PNGs",
                entries: self.preview_cache.len(),
                bytes: self.preview_cache.values().map(|(_, p)| b(p)).sum(),
            },
            CacheStat {
                name: "program monitor frame",
                entries: self.monitor_frame.is_some() as usize,
                bytes: self.monitor_frame.as_ref().map(|m| m.pixels.len() as u64).unwrap_or(0),
            },
            CacheStat {
                name: "pending UI events",
                entries: self.events.len(),
                bytes: 0,
            },
            CacheStat {
                name: "frame-time history",
                entries: self.perf_frame_times.len(),
                bytes: (self.perf_frame_times.len() * 16) as u64,
            },
        ];
        let bus: u64 = self.mixer.channels.keys().map(|id| self.audio_bus.buffered_frames(id) as u64).sum();
        out.push(CacheStat { name: "audio bus (buffered frames ×8 B)", entries: self.mixer.channels.len(), bytes: bus * 8 });
        out
    }
}
