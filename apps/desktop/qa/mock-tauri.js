// QA only: a fake Tauri backend injected before the app loads (Playwright
// `addInitScript`). It answers the app's commands with a realistic studio
// (colour + image + hidden screen/test pattern, a camera with audio, a mic)
// and keeps edits (delete, reorder, effects, video settings) so the UI can be
// exercised end to end. Every call is logged in window.__qaCalls.
(() => {
  const FX_OFF = {
    enabled: true, invert_polarity: false, mono: false,
    highpass_enabled: false, highpass_hz: 80, lowpass_enabled: false, lowpass_hz: 16000,
    denoise_enabled: false, denoise_strength: 1,
    gate_enabled: false, gate_threshold_db: -40, gate_release_ms: 120,
    eq_enabled: false, eq_low_db: 0, eq_mid_db: 0, eq_high_db: 0, eq_mid_hz: 1000,
    deesser_enabled: false, deesser_hz: 6000, deesser_threshold_db: -30, deesser_max_db: 6,
    compressor_enabled: false, comp_threshold_db: -18, comp_ratio: 3, comp_attack_ms: 10, comp_release_ms: 100, comp_makeup_db: 0,
    limiter_enabled: false, limiter_ceiling_db: -1, limiter_release_ms: 50,
    delay_enabled: false, delay_ms: 0,
  };
  const FX_VOICE = { ...FX_OFF, denoise_enabled: true, gate_enabled: true, eq_enabled: true, eq_mid_db: 2, eq_high_db: 2, compressor_enabled: true };
  const strip = (fx) => ({ gain: 1, mute: false, pan: 0, solo: false, trim_db: 0, fx: { ...fx } });
  const T = (x, y, w, h) => ({ x, y, w, h, crop_left: 0, crop_top: 0, crop_right: 0, crop_bottom: 0, fit: "fit", opacity: 1, rotation: 0 });

  const S = {
    scenes: [
      {
        id: "sc1",
        name: "Scene 1",
        filters: [],
        sources: [
          { id: "s-mic", name: "Microphone (USB)", kind: { AudioInput: { device: "Microphone (USB Audio)" } }, visible: true, order: 0, status: "ready", input_id: null, transform: null, locked: false },
          { id: "s-tp", name: "Test Pattern", kind: "TestPattern", visible: false, order: 1, status: "ready", input_id: "in-tp", transform: null, locked: false },
          { id: "s-screen", name: "Screen", kind: "Screen", visible: false, order: 2, status: "ready", input_id: "in-screen", transform: null, locked: false },
          { id: "s-cam", name: "Camera", kind: { Camera: { device: "HD Webcam", width: 1280, height: 720, fps: 30, audio_device: "Microphone (HD Webcam)", format: null } }, visible: true, order: 3, status: "ready", input_id: "in-cam", transform: T(0.68, 0.62, 0.3, 0.33), locked: false },
          { id: "s-img", name: "download.png", kind: { Image: { path: "C:\\Users\\me\\Pictures\\download.png" } }, visible: true, order: 4, status: "ready", input_id: "in-img", transform: T(0.05, 0.08, 0.3, 0.3), locked: false },
          { id: "s-color", name: "Color #0073ff", kind: { Color: { r: 0, g: 115, b: 255 } }, visible: true, order: 5, status: "ready", input_id: "in-color", transform: null, locked: true },
        ],
      },
      { id: "sc2", name: "Scene 2", filters: [], sources: [] },
    ],
    preview: "sc1",
    program: "sc1",
    mixer: {
      master: strip({ ...FX_OFF, limiter_enabled: true }),
      channels: {
        "s-mic": strip(FX_VOICE),
        "s-cam": strip(FX_VOICE),
        "s-tp": strip(FX_OFF),
        "s-screen": strip(FX_OFF),
        "s-img": strip(FX_OFF),
        "s-color": strip(FX_OFF),
      },
    },
    video: { output_width: 1280, output_height: 720, output_fps: 30, encoder: "auto", bitrate_kbps: 8000, camera_device: null, camera_width: 1280, camera_height: 720, camera_fps: 30 },
    cam: { device: "HD Webcam", width: 1280, height: 720, fps: 30, format: null, negotiated: "mjpeg 1280×720@30fps [via mjpeg]" },
  };
  window.__qaState = S;
  window.__qaCalls = [];

  const stats = (o) => ({ vad: 0, comp_reduction_db: 0, limiter_reduction_db: 0, deesser_reduction_db: 0, gate_closed: false, latency_ms: 0, process_us: 0, ...o });

  function productionState() {
    return {
      scenes: S.scenes,
      preview_scene_id: S.preview,
      program_scene_id: S.program,
      transition: { active: false, progress: 0 },
      mixer: {
        master: S.mixer.master,
        channels: S.mixer.channels,
        meters_live: true,
        meters: {},
        monitor_enabled: false,
        enhancer: { enabled: true, gate_enabled: true, gate_threshold_db: -40, eq_low_db: 0, eq_mid_db: 2, eq_high_db: 2, compressor_enabled: true, ai_denoise: true, ai_strength: 1 },
        voice: {
          "s-mic": { vad: 0.8, gain_reduction_db: 2, gate_closed: false, latency_ms: 20, process_us: 41, buffered_ms: 20, underruns: 0, dropped_frames: 0, device: "running", audio_age_ms: 64, target_age_ms: 66, corrections: 1 },
          "s-cam": { vad: 0.1, gain_reduction_db: 0, gate_closed: true, latency_ms: 20, process_us: 38, buffered_ms: 20, underruns: 0, dropped_frames: 0, device: "running", audio_age_ms: 90, target_age_ms: 92, corrections: 0 },
        },
        av_sync: { auto: true, offset_ms: 0 },
        fx_stats: {},
        master_fx_stats: stats({}),
      },
      stream_settings: { url: "rtmp://a.rtmp.youtube.com/live2", platform: "youtube" },
      stream_lifecycle: { state: "idle" },
      record_settings: { path: "", format: "mp4", quality: "high" },
      record_lifecycle: { state: "idle" },
      video_settings: S.video,
      macros: [],
    };
  }

  const diagnostics = () => ({
    render_fps: 30, target_fps: 30, tick_ms: 1.2, capture_ms: 0.1, render_ms: 0, gpu_ms: 0, audio_ms: 0.3, encode_ms: 0,
    lagged_ticks: 0, skipped_render: 0, dropped_network: 0,
    sources: [
      { id: "s-cam", name: "Camera", kind: "camera", fps: 30, device_fps: 30, device_packet_fps: 30, decode_ms: 3.1, convert_ms: 1.2, capture_latency_ms: 8, max_gap_ms: 40, decode_errors: 0, capture_errors: 0, paused: false, dropped: 0, age_ms: 52, width: 1280, height: 720, format: "mjpeg 1280×720@30fps [via mjpeg]", error: null },
      { id: "s-screen", name: "Screen", kind: "screen", fps: 0, device_fps: 0, device_packet_fps: 0, decode_ms: null, convert_ms: null, capture_latency_ms: null, max_gap_ms: null, decode_errors: 0, capture_errors: 0, paused: true, dropped: 0, age_ms: 0, width: 0, height: 0, format: null, error: null },
    ],
    audio: [],
    encoder: null,
    stream_status: "idle", stream_error: null, stream_kbps: 0, stream_uptime_s: 0, recording: false,
    latency: { app_total_ms: null },
    color_active: false, av_auto: true, av_offset_ms: 0,
    system: { cpu_percent: 4.2, ram_mb: 136, cores: 8, system_cpu_percent: 9, mem_used_mb: 6100, mem_total_mb: 16000, gpu_percent: 3, gpu_app_percent: 1, gpu_encode_percent: 0, gpu_note: null },
    internal_errors: 0, last_internal_error: null, gpu_errors: 0, last_gpu_error: null, gpu_adapter: "Mock GPU", caches: [],
  });

  // One JPEG of the programme (blue + image box + camera box), made once.
  let jpeg = null;
  let seq = 0;
  function makeJpeg() {
    const c = document.createElement("canvas");
    c.width = 640;
    c.height = 360;
    const g = c.getContext("2d");
    g.fillStyle = "#0073ff";
    g.fillRect(0, 0, 640, 360);
    g.fillStyle = "#f2f2f2";
    g.fillRect(32, 29, 192, 108);
    g.fillStyle = "#e9a23b";
    g.fillRect(70, 60, 116, 46);
    g.fillStyle = "#2b2f36";
    g.fillRect(435, 223, 192, 119);
    g.fillStyle = "#c9ccd1";
    g.beginPath();
    g.arc(531, 270, 26, 0, Math.PI * 2);
    g.fill();
    return new Promise((res) => c.toBlob((b) => b.arrayBuffer().then(res), "image/jpeg", 0.85));
  }

  const scene = (id) => S.scenes.find((s) => s.id === id);
  const findSrc = (id) => S.scenes.flatMap((s) => s.sources).find((s) => s.id === id);

  async function handle(cmd, a) {
    switch (cmd) {
      case "plugin:event|listen":
        return 1;
      case "plugin:event|unlisten":
        return null;
      case "production_state":
        return productionState();
      case "diagnostics":
        return diagnostics();
      case "stream_platforms":
        return [
          { id: "youtube", name: "YouTube", default_server: "rtmp://a.rtmp.youtube.com/live2", key_help: "YouTube Studio → Go live → Stream key", latency_note: "" },
          { id: "custom_rtmp", name: "Custom", default_server: null, key_help: "rtmp:// server and key", latency_note: "" },
        ];
      case "audio_meters":
        return {
          channels: [
            { id: "s-mic", peak: 0.31, rms: 0.12, has_signal_path: true, fx: stats({ vad: 0.82, comp_reduction_db: 3.4, latency_ms: 20, process_us: 41 }) },
            { id: "s-cam", peak: 0.08, rms: 0.03, has_signal_path: true, fx: stats({ vad: 0.05, gate_closed: true, latency_ms: 20, process_us: 37 }) },
          ],
          master_peak: 0.33,
          master_rms: 0.13,
          solo_active: false,
          master_fx: stats({ limiter_reduction_db: 0.4, process_us: 6 }),
        };
      case "monitor_frame_if_new": {
        if (!jpeg) jpeg = await makeJpeg();
        if (a.after_seq >= 1) return new ArrayBuffer(0);
        seq = 1;
        const out = new Uint8Array(8 + jpeg.byteLength);
        new DataView(out.buffer).setBigUint64(0, BigInt(seq), true);
        out.set(new Uint8Array(jpeg), 8);
        return out.buffer;
      }
      case "monitor_set_display_width":
        return [640, 360];
      case "source_remove": {
        const sc = scene(a.scene_id);
        sc.sources = sc.sources.filter((s) => s.id !== a.source_id);
        delete S.mixer.channels[a.source_id];
        return null;
      }
      case "source_reorder": {
        const sc = scene(a.scene_id);
        if (a.order.length !== sc.sources.length) throw "reorder list does not match scene sources";
        sc.sources = a.order.map((id, i) => ({ ...sc.sources.find((s) => s.id === id), order: i }));
        return null;
      }
      case "source_set_visible":
        findSrc(a.source_id).visible = a.visible;
        return findSrc(a.source_id);
      case "source_set_locked":
        findSrc(a.source_id).locked = a.locked;
        return findSrc(a.source_id);
      case "mixer_set_fx":
        S.mixer.channels[a.source_id].fx = { ...a.fx };
        return S.mixer.channels[a.source_id];
      case "mixer_set_master_fx":
        S.mixer.master.fx = { ...a.fx };
        return S.mixer.master;
      case "video_settings_get":
        return S.video;
      case "video_settings_set":
        S.video = { ...a.settings };
        return null;
      case "camera_sources":
        return [{ id: "s-cam", name: "Camera", ...S.cam }];
      case "list_cameras":
        return ["HD Webcam", "USB Capture HDMI"];
      case "list_audio_devices":
        return ["Microphone (USB Audio)", "Microphone (HD Webcam)"];
      case "source_audio_device":
        return a.source_id === "s-cam" ? "Microphone (HD Webcam)" : null;
      case "camera_modes":
        return a.device === "USB Capture HDMI"
          ? [
              { format: "nv12", min_width: 640, min_height: 360, max_width: 1920, max_height: 1080, min_fps: 5, max_fps: 60 },
              { format: "yuyv422", min_width: 640, min_height: 360, max_width: 1920, max_height: 1080, min_fps: 5, max_fps: 30 },
            ]
          : [
              { format: "mjpeg", min_width: 1920, min_height: 1080, max_width: 1920, max_height: 1080, min_fps: 5, max_fps: 30 },
              { format: "mjpeg", min_width: 1280, min_height: 720, max_width: 1280, max_height: 720, min_fps: 5, max_fps: 60 },
              { format: "mjpeg", min_width: 640, min_height: 360, max_width: 640, max_height: 360, min_fps: 5, max_fps: 30 },
              { format: "yuyv422", min_width: 640, min_height: 480, max_width: 640, max_height: 480, min_fps: 5, max_fps: 30 },
            ];
      case "camera_configure": {
        // A real camera gives its nearest mode: 1080p only at 30 fps here.
        const fps = a.width >= 1920 ? Math.min(a.fps, 30) : a.fps;
        S.cam = { device: a.device, width: a.width, height: a.height, fps: a.fps, format: a.format, negotiated: `${a.format ?? "mjpeg"} ${a.width}×${a.height}@${fps}fps [via ${a.format ?? "mjpeg"}]` };
        return S.cam.negotiated;
      }
      default:
        if (/^list_|_list$|library$|^lut_library|^color_preset_list/.test(cmd)) return [];
        return null;
    }
  }

  let cb = 1;
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
    transformCallback: () => cb++,
    invoke: async (cmd, args) => {
      window.__qaCalls.push([cmd, args]);
      return handle(cmd, args ?? {});
    },
    convertFileSrc: (p) => p,
  };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
})();
