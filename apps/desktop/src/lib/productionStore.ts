/**
 * Production state (zustand): sources, mixer/enhancer, A/V sync, stream,
 * record, the live program monitor and measured diagnostics.
 *
 * One program scene: StreamFlow is a single-output tool (Input → Preview →
 * Color → Audio → Stream), so every source is added to the program scene.
 *
 * Rules:
 * - Never fabricate state: live / recording indicators only reflect what
 *   `production_state()` and `diagnostics()` report.
 * - Never queue IPC: every poll has an in-flight guard, so a slow backend
 *   can't build a request backlog (the monitor shows the newest frame or
 *   nothing — never a stale queue).
 */
import { create } from "zustand";
import { api, isTauri, onSfEvent } from "./ipc";
import { useStudio } from "./store";
import type {
  Diagnostics,
  EnhancerSettings,
  PlatformInfo,
  ProductionStateData,
  SceneLayout,
  SourceKind,
  SourceTransform,
  TransitionKindName,
  VoiceStats,
  PerfSample,
  MixerChannelView,
} from "./types";

interface ProductionStore {
  ready: boolean;
  state: ProductionStateData | null;
  /** Program monitor (JPEG data URL) and its backend frame number. */
  programUrl: string | null;
  programSeq: number;
  diag: Diagnostics | null;
  platforms: PlatformInfo[];
  /** Stream settings being edited (the key never comes back from Rust). */
  streamPlatform: string;
  streamUrl: string;
  streamKey: string;
  streamError: string | null;
  recordPath: string;
  recordFormat: string;
  recordError: string | null;
  mediaPaused: Record<string, boolean>;
  busy: Record<string, boolean>;

  init: () => Promise<void>;
  dispose: () => void;
  refresh: () => Promise<void>;
  pollMonitor: () => Promise<void>;
  pollDiagnostics: () => Promise<void>;
  /** Last ~2 minutes of real diagnostics samples (1 per poll). */
  perf: PerfSample[];

  /** Source selected in the workspace (strip, preview box, properties). */
  selectedSourceId: string | null;
  selectSource: (id: string | null) => void;
  sourceSetLocked: (source_id: string, locked: boolean) => Promise<void>;
  sourceDuplicate: (source_id: string) => Promise<void>;
  sceneMove: (scene_id: string, to: number) => Promise<void>;
  setChannelPan: (source_id: string, pan: number) => void;
  setChannelSolo: (source_id: string, solo: boolean) => Promise<void>;
  setChannelTrim: (source_id: string, trim_db: number) => void;

  /** The scene everything is added to (program). */
  sceneId: () => string | null;
  sourceAdd: (kind: SourceKind, name: string) => Promise<boolean>;
  sourceRemove: (source_id: string) => Promise<void>;
  sourceReorder: (order: string[]) => Promise<void>;
  sourceSetVisible: (source_id: string, visible: boolean) => Promise<void>;
  /** Live placement edit (optimistic; the backend clamps). */
  sourceSetTransform: (source_id: string, t: SourceTransform | null) => Promise<void>;
  sceneApplyLayout: (layout: SceneLayout) => Promise<void>;

  // Studio: scenes, preview → program, transitions.
  /** Preview-scene monitor (data URL), polled only while it differs from program. */
  previewUrl: string | null;
  pollPreviewMonitor: () => Promise<void>;
  /** Eased progress of a running auto transition, null when idle. */
  transitionProgress: number | null;
  transitionKind: TransitionKindName;
  transitionMs: number;
  setTransition: (kind: TransitionKindName, ms: number) => void;
  setPreviewScene: (scene_id: string) => Promise<void>;
  cut: () => Promise<void>;
  auto: () => Promise<void>;
  sceneCreate: () => Promise<void>;
  sceneDuplicate: (scene_id: string) => Promise<void>;
  sceneRename: (scene_id: string, name: string) => Promise<void>;
  sceneDelete: (scene_id: string) => Promise<void>;
  mediaSetPaused: (source_id: string, paused: boolean) => Promise<void>;

  setChannelGain: (source_id: string, gain: number) => void;
  setChannelMute: (source_id: string, mute: boolean) => Promise<void>;
  setMasterGain: (gain: number) => void;
  setMasterMute: (mute: boolean) => Promise<void>;
  setMonitorEnabled: (enabled: boolean) => Promise<void>;
  setEnhancerSettings: (settings: EnhancerSettings) => Promise<void>;
  setAvSync: (auto: boolean, offset_ms: number) => void;

  setStreamPlatform: (v: string) => void;
  setStreamUrl: (v: string) => void;
  setStreamKey: (v: string) => void;
  streamStart: () => Promise<void>;
  streamStop: () => Promise<void>;
  clearStreamError: () => void;

  setRecordPath: (v: string) => void;
  setRecordFormat: (v: string) => void;
  recordStart: () => Promise<void>;
  recordStop: () => Promise<void>;
  recordPause: () => Promise<void>;
  recordResume: () => Promise<void>;
  clearRecordError: () => void;
}

const toast = (text: string) => useStudio.getState().pushToast(text);

/** Human message from an IPC error (Tauri rejects with the Rust string). */
export function errText(e: unknown): string {
  const s = typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
  return s.replace(/^Error: /, "");
}

// --- persisted, non-secret stream prefs (the key is NEVER stored) ---------
const PREFS_KEY = "sf.stream.prefs.v1";
function loadPrefs(): { platform: string; urls: Record<string, string>; recordPath: string } {
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (raw) {
      const p = JSON.parse(raw);
      return {
        platform: typeof p.platform === "string" ? p.platform : "youtube",
        urls: p.urls && typeof p.urls === "object" ? p.urls : {},
        recordPath: typeof p.recordPath === "string" ? p.recordPath : "",
      };
    }
  } catch {
    /* storage unavailable */
  }
  return { platform: "youtube", urls: {}, recordPath: "" };
}
function savePrefs(platform: string, url: string, recordPath: string) {
  try {
    const p = loadPrefs();
    p.platform = platform;
    p.urls[platform] = url;
    p.recordPath = recordPath;
    localStorage.setItem(PREFS_KEY, JSON.stringify(p));
  } catch {
    /* storage unavailable */
  }
}

/** Trailing 60 Hz sync for gain drags. */
let gainTimer: ReturnType<typeof setTimeout> | null = null;
const pendingGains = new Map<string, number>();
function scheduleGainFlush() {
  if (gainTimer !== null) return;
  gainTimer = setTimeout(async () => {
    gainTimer = null;
    const batch = [...pendingGains.entries()];
    pendingGains.clear();
    for (const [key, gain] of batch) {
      try {
        if (key === "master") await api.mixerSetMasterGain(gain);
        else await api.mixerSetGain(key.slice(3), gain);
      } catch (e) {
        toast(`Gain change failed: ${errText(e)}`);
      }
    }
  }, 16);
}

let avTimer: ReturnType<typeof setTimeout> | null = null;
let unlisten: (() => void) | null = null;
let monitorInFlight = false;
let diagInFlight = false;
let refreshInFlight = false;

function kindToString(kind: unknown): string {
  if (typeof kind === "string") return kind.toLowerCase();
  if (kind && typeof kind === "object") {
    const key = Object.keys(kind as Record<string, unknown>)[0];
    return key ? key.toLowerCase() : "unknown";
  }
  return "unknown";
}

/** Device / file / text carried by a source kind, for display. */
function kindDetail(kind: unknown): string | null {
  if (!kind || typeof kind !== "object") return null;
  const v = Object.values(kind as Record<string, any>)[0];
  if (!v || typeof v !== "object") return null;
  return (v.device ?? v.path ?? v.text ?? null) as string | null;
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function patchChannel(set: any, get: () => ProductionStore, id: string, patch: Partial<MixerChannelView>) {
  const st = get().state;
  if (!st) return;
  set({ state: { ...st, mixer: { ...st.mixer, channels: st.mixer.channels.map((c) => (c.source_id === id ? { ...c, ...patch } : c)) } } });
}

/**
 * Monitor pictures as blob URLs (raw JPEG from the engine, no base64).
 * Each slot keeps the shown URL and the one before it (still decoding in
 * the <img>); anything older is revoked so blobs never pile up.
 */
const urlSlots: Record<string, string[]> = {};
function swapUrl(slot: string, jpeg: ArrayBuffer | null): string | null {
  const list = (urlSlots[slot] ??= []);
  const url = jpeg ? URL.createObjectURL(new Blob([jpeg], { type: "image/jpeg" })) : null;
  if (url) list.push(url);
  while (list.length > (url ? 2 : 0)) URL.revokeObjectURL(list.shift()!);
  return url;
}

const num = (v: unknown, d: number) => (typeof v === "number" && Number.isFinite(v) ? v : d);

/** Normalize `production_state()` JSON in ONE place. */
function normalizeProductionState(raw: unknown): ProductionStateData {
  const r = (raw ?? {}) as Record<string, any>;
  const nameBySource = new Map<string, string>();
  const scenes = (r.scenes ?? []).map((sc: any) => ({
    id: String(sc.id ?? ""),
    name: String(sc.name ?? ""),
    sources: (sc.sources ?? []).map((s: any) => {
      nameBySource.set(String(s.id ?? ""), String(s.name ?? ""));
      return {
        id: String(s.id ?? ""),
        name: String(s.name ?? ""),
        kind: kindToString(s.kind),
        visible: !!s.visible,
        order: num(s.order, 0),
        status: String(s.status ?? ""),
        input_id: typeof s.input_id === "string" ? s.input_id : null,
        transform: s.transform && typeof s.transform === "object" ? ({ rotation: 0, ...s.transform } as SourceTransform) : null,
        locked: !!s.locked,
        detail: kindDetail(s.kind),
      };
    }),
  }));
  const channelsObj = (r.mixer?.channels ?? {}) as Record<string, any>;
  const channels = Object.entries(channelsObj).map(([source_id, ch]: [string, any]) => ({
    source_id,
    name: nameBySource.get(source_id) ?? source_id,
    gain: num(ch?.gain, 1),
    mute: !!ch?.mute,
    pan: num(ch?.pan, 0),
    solo: !!ch?.solo,
    trim_db: num(ch?.trim_db, 0),
  }));
  const sl = (r.stream_lifecycle ?? {}) as Record<string, any>;
  const rl = (r.record_lifecycle ?? {}) as Record<string, any>;
  const e = r.mixer?.enhancer ?? {};
  return {
    scenes,
    preview_scene_id: r.preview_scene_id ?? null,
    program_scene_id: r.program_scene_id ?? null,
    mixer: {
      channels,
      master: { gain: num(r.mixer?.master?.gain, 1), mute: !!r.mixer?.master?.mute },
      meters_live: !!r.mixer?.meters_live,
      meters:
        r.mixer?.meters && typeof r.mixer.meters === "object"
          ? (r.mixer.meters as Record<string, number>)
          : {},
      monitor_enabled: !!r.mixer?.monitor_enabled,
      enhancer: {
        enabled: !!e.enabled,
        gate_enabled: e.gate_enabled !== false,
        gate_threshold_db: num(e.gate_threshold_db, -40),
        eq_low_db: num(e.eq_low_db, 0),
        eq_mid_db: num(e.eq_mid_db, 2),
        eq_high_db: num(e.eq_high_db, 2),
        compressor_enabled: e.compressor_enabled !== false,
        ai_denoise: e.ai_denoise !== false,
        ai_strength: num(e.ai_strength, 1),
      },
      voice:
        r.mixer?.voice && typeof r.mixer.voice === "object"
          ? (r.mixer.voice as Record<string, VoiceStats>)
          : {},
      av_sync: {
        auto: r.mixer?.av_sync?.auto !== false,
        offset_ms: num(r.mixer?.av_sync?.offset_ms, 0),
      },
    },
    stream: {
      state: String(sl.state ?? "idle"),
      message: typeof sl.message === "string" ? sl.message : null,
      settings: {
        url: String(r.stream_settings?.url ?? ""),
        platform: String(r.stream_settings?.platform ?? ""),
      },
    },
    record: {
      state: String(rl.state ?? "idle"),
      elapsed_secs: num(rl.elapsed_secs, 0),
      message: typeof rl.message === "string" ? rl.message : null,
    },
    video_settings: {
      output_width: num(r.video_settings?.output_width, 1920),
      output_height: num(r.video_settings?.output_height, 1080),
      output_fps: num(r.video_settings?.output_fps, 30),
      encoder: String(r.video_settings?.encoder ?? "auto"),
      bitrate_kbps: num(r.video_settings?.bitrate_kbps, 8000),
      camera_device:
        typeof r.video_settings?.camera_device === "string" ? r.video_settings.camera_device : null,
      camera_width: num(r.video_settings?.camera_width, 1280),
      camera_height: num(r.video_settings?.camera_height, 720),
      camera_fps: num(r.video_settings?.camera_fps, 30),
    },
  };
}

const prefs = loadPrefs();

export const useProduction = create<ProductionStore>()((set, get) => ({
  ready: false,
  state: null,
  programUrl: null,
  programSeq: 0,
  diag: null,
  platforms: [],
  streamPlatform: prefs.platform,
  streamUrl: prefs.urls[prefs.platform] ?? "",
  streamKey: "",
  streamError: null,
  recordPath: prefs.recordPath,
  recordFormat: "mp4",
  recordError: null,
  mediaPaused: {},
  busy: {},

  init: async () => {
    if (!isTauri) return;
    await get().refresh();
    try {
      set({ platforms: await api.streamPlatforms() });
    } catch {
      /* older backend */
    }
    if (unlisten) return;
    unlisten = await onSfEvent((e) => {
      switch (e.event) {
        case "production.scene_changed":
        case "production.source_changed":
        case "production.stream_state":
        case "production.record_state":
        case "production.mixer_changed":
          void get().refresh();
          break;
        case "production.transition_progress":
          set({ transitionProgress: (e as { progress?: number }).progress ?? null });
          break;
        case "production.transition_done":
          set({ transitionProgress: null });
          void get().refresh();
          break;
        default:
          break;
      }
    });
  },

  dispose: () => {
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  },

  refresh: async () => {
    if (!isTauri || refreshInFlight) return;
    refreshInFlight = true;
    try {
      const state = normalizeProductionState(await api.productionState());
      const prev = get().state;
      set({ state, ready: true });
      if (state.stream.state === "error" && state.stream.message && prev?.stream.state !== "error") {
        set({ streamError: state.stream.message });
      }
      if (state.record.state === "error" && state.record.message && prev?.record.state !== "error") {
        set({ recordError: state.record.message });
      }
    } catch (e) {
      toast(`Could not reach the engine: ${errText(e)}`);
    } finally {
      refreshInFlight = false;
    }
  },

  pollMonitor: async () => {
    const st = get().state;
    const scene = st?.program_scene_id ?? st?.scenes[0]?.id ?? null;
    if (!isTauri || !scene || monitorInFlight) return;
    monitorInFlight = true;
    try {
      const buf = await api.monitorFrameIfNew(scene, get().programSeq);
      if (buf && buf.byteLength > 8) {
        const seq = Number(new DataView(buf).getBigUint64(0, true));
        set({ programSeq: seq, programUrl: swapUrl("program", buf.slice(8)) });
      }
    } catch {
      /* next poll retries */
    } finally {
      monitorInFlight = false;
    }
  },

  perf: [],
  selectedSourceId: null,
  selectSource: (id) => set({ selectedSourceId: id }),
  sourceSetLocked: async (source_id, locked) => {
    const scene = get().sceneId();
    if (!scene) return;
    try {
      await api.sourceSetLocked(scene, source_id, locked);
      await get().refresh();
    } catch (e) {
      toast(`Could not ${locked ? "lock" : "unlock"}: ${errText(e)}`);
    }
  },
  sourceDuplicate: async (source_id) => {
    const scene = get().sceneId();
    if (!scene) return;
    try {
      const d = await api.sourceDuplicate(scene, source_id);
      await get().refresh();
      set({ selectedSourceId: d.id });
    } catch (e) {
      toast(`Could not duplicate: ${errText(e)}`);
    }
  },
  sceneMove: async (scene_id, to) => {
    try {
      await api.sceneMove(scene_id, to);
      await get().refresh();
    } catch (e) {
      toast(`Could not move scene: ${errText(e)}`);
    }
  },
  setChannelPan: (source_id, pan) => {
    patchChannel(set, get, source_id, { pan });
    void api.mixerSetPan(source_id, pan).catch((e) => toast(`Pan failed: ${errText(e)}`));
  },
  setChannelSolo: async (source_id, solo) => {
    patchChannel(set, get, source_id, { solo });
    try {
      await api.mixerSetSolo(source_id, solo);
    } catch (e) {
      toast(`Solo failed: ${errText(e)}`);
    }
  },
  setChannelTrim: (source_id, trim_db) => {
    patchChannel(set, get, source_id, { trim_db });
    void api.mixerSetTrim(source_id, trim_db).catch((e) => toast(`Trim failed: ${errText(e)}`));
  },

  pollDiagnostics: async () => {
    if (!isTauri || diagInFlight) return;
    diagInFlight = true;
    try {
      const d = await api.diagnostics();
      const sample: PerfSample = {
        t: Date.now(),
        cpu: d.system.system_cpu_percent ?? d.system.cpu_percent ?? null,
        gpu: d.system.gpu_percent ?? null,
        fps: d.render_fps,
        dropped: d.lagged_ticks + d.skipped_render + d.dropped_network,
      };
      set((s) => ({ diag: d, perf: [...s.perf.slice(-119), sample] }));
    } catch {
      /* next poll retries */
    } finally {
      diagInFlight = false;
    }
  },

  // Edits target the PREVIEW scene (studio mode). With one scene, preview
  // and program are the same scene, so edits are live.
  sceneId: () => {
    const st = get().state;
    return st?.preview_scene_id ?? st?.program_scene_id ?? st?.scenes[0]?.id ?? null;
  },

  previewUrl: null,
  pollPreviewMonitor: async () => {
    const st = get().state;
    const pv = st?.preview_scene_id;
    if (!pv || pv === st?.program_scene_id) {
      if (get().previewUrl) set({ previewUrl: swapUrl("preview", null) });
      return;
    }
    try {
      const buf = await api.scenePreviewJpg(pv);
      if (buf && buf.byteLength) set({ previewUrl: swapUrl("preview", buf) });
    } catch {
      /* scene vanished between polls */
    }
  },
  transitionProgress: null,
  transitionKind: (() => {
    try {
      const k = localStorage.getItem("sf.transition.kind");
      return k === "fade" || k === "wipe" ? k : "mix";
    } catch {
      return "mix";
    }
  })() as TransitionKindName,
  transitionMs: (() => {
    try {
      const v = Number(localStorage.getItem("sf.transition.ms"));
      return v >= 100 && v <= 5000 ? v : 500;
    } catch {
      return 500;
    }
  })(),
  setTransition: (kind, ms) => {
    set({ transitionKind: kind, transitionMs: ms });
    try {
      localStorage.setItem("sf.transition.kind", kind);
      localStorage.setItem("sf.transition.ms", String(ms));
    } catch {
      /* private mode */
    }
  },
  setPreviewScene: async (scene_id) => {
    try {
      await api.setPreviewScene(scene_id);
      await get().refresh();
    } catch (e) {
      toast(`Could not select scene: ${errText(e)}`);
    }
  },
  cut: async () => {
    try {
      await api.transitionCut();
      await get().refresh();
    } catch (e) {
      toast(`Cut failed: ${errText(e)}`);
    }
  },
  auto: async () => {
    const { transitionKind, transitionMs, state } = get();
    if (!state || state.preview_scene_id === state.program_scene_id) return;
    try {
      set({ transitionProgress: 0 });
      await api.transitionAuto(transitionMs, transitionKind);
    } catch (e) {
      set({ transitionProgress: null });
      toast(`Transition failed: ${errText(e)}`);
    }
  },
  sceneCreate: async () => {
    try {
      const n = (get().state?.scenes.length ?? 0) + 1;
      const sc = await api.sceneCreate(`Scene ${n}`);
      await api.setPreviewScene(sc.id);
      await get().refresh();
    } catch (e) {
      toast(`Could not create scene: ${errText(e)}`);
    }
  },
  sceneDuplicate: async (scene_id) => {
    try {
      const sc = await api.sceneDuplicate(scene_id);
      await api.setPreviewScene(sc.id);
      await get().refresh();
    } catch (e) {
      toast(`Could not duplicate scene: ${errText(e)}`);
    }
  },
  sceneRename: async (scene_id, name) => {
    try {
      await api.sceneRename(scene_id, name);
      await get().refresh();
    } catch (e) {
      toast(`Could not rename scene: ${errText(e)}`);
    }
  },
  sceneDelete: async (scene_id) => {
    try {
      await api.sceneDelete(scene_id);
      await get().refresh();
    } catch (e) {
      toast(`Could not delete scene: ${errText(e)}`);
    }
  },

  sourceAdd: async (kind, name) => {
    const scene = get().sceneId();
    if (!scene) {
      toast("The engine is not ready yet.");
      return false;
    }
    set((s) => ({ busy: { ...s.busy, add: true } }));
    try {
      await api.sourceAdd(scene, kind, name);
      await get().refresh();
      return true;
    } catch (e) {
      toast(errText(e));
      return false;
    } finally {
      set((s) => ({ busy: { ...s.busy, add: false } }));
    }
  },

  sourceRemove: async (source_id) => {
    const scene = get().sceneId();
    if (!scene) return;
    try {
      await api.sourceRemove(scene, source_id);
      await get().refresh();
    } catch (e) {
      toast(`Could not remove the source: ${errText(e)}`);
    }
  },

  sourceReorder: async (order) => {
    const scene = get().sceneId();
    if (!scene) return;
    try {
      await api.sourceReorder(scene, order);
      await get().refresh();
    } catch (e) {
      toast(`Could not reorder: ${errText(e)}`);
    }
  },

  sourceSetVisible: async (source_id, visible) => {
    const scene = get().sceneId();
    const { state } = get();
    if (!scene || !state) return;
    set({
      state: {
        ...state,
        scenes: state.scenes.map((sc) =>
          sc.id === scene
            ? { ...sc, sources: sc.sources.map((s) => (s.id === source_id ? { ...s, visible } : s)) }
            : sc,
        ),
      },
    });
    try {
      await api.sourceSetVisible(scene, source_id, visible);
    } catch (e) {
      toast(`Could not change visibility: ${errText(e)}`);
      await get().refresh();
    }
  },

  sourceSetTransform: async (source_id, t) => {
    const scene = get().sceneId();
    const { state } = get();
    if (!scene || !state) return;
    set({
      state: {
        ...state,
        scenes: state.scenes.map((sc) =>
          sc.id === scene
            ? { ...sc, sources: sc.sources.map((s) => (s.id === source_id ? { ...s, transform: t } : s)) }
            : sc,
        ),
      },
    });
    try {
      await api.sourceSetTransform(scene, source_id, t);
    } catch (e) {
      toast(`Could not place source: ${errText(e)}`);
      await get().refresh();
    }
  },

  sceneApplyLayout: async (layout) => {
    const scene = get().sceneId();
    if (!scene) return;
    try {
      const n = await api.sceneApplyLayout(scene, layout);
      if (n < 2 && layout !== "full") toast("Add a second video source (camera, screen or file) for this layout.");
      await get().refresh();
    } catch (e) {
      toast(`Layout failed: ${errText(e)}`);
    }
  },

  mediaSetPaused: async (source_id, paused) => {
    set((s) => ({ mediaPaused: { ...s.mediaPaused, [source_id]: paused } }));
    try {
      const actual = await api.mediaSetPaused(source_id, paused);
      set((s) => ({ mediaPaused: { ...s.mediaPaused, [source_id]: actual } }));
    } catch (e) {
      toast(`Playback control failed: ${errText(e)}`);
      set((s) => ({ mediaPaused: { ...s.mediaPaused, [source_id]: !paused } }));
    }
  },

  setChannelGain: (source_id, gain) => {
    const { state } = get();
    if (state) {
      set({
        state: {
          ...state,
          mixer: {
            ...state.mixer,
            channels: state.mixer.channels.map((c) => (c.source_id === source_id ? { ...c, gain } : c)),
          },
        },
      });
    }
    pendingGains.set(`ch:${source_id}`, gain);
    scheduleGainFlush();
  },

  setChannelMute: async (source_id, mute) => {
    const { state } = get();
    if (state) {
      set({
        state: {
          ...state,
          mixer: {
            ...state.mixer,
            channels: state.mixer.channels.map((c) => (c.source_id === source_id ? { ...c, mute } : c)),
          },
        },
      });
    }
    try {
      await api.mixerSetMute(source_id, mute);
    } catch (e) {
      toast(`Mute failed: ${errText(e)}`);
      await get().refresh();
    }
  },

  setMasterGain: (gain) => {
    const { state } = get();
    if (state) {
      set({ state: { ...state, mixer: { ...state.mixer, master: { ...state.mixer.master, gain } } } });
    }
    pendingGains.set("master", gain);
    scheduleGainFlush();
  },

  setMasterMute: async (mute) => {
    const { state } = get();
    if (state) {
      set({ state: { ...state, mixer: { ...state.mixer, master: { ...state.mixer.master, mute } } } });
    }
    try {
      await api.mixerSetMasterMute(mute);
    } catch (e) {
      toast(`Mute failed: ${errText(e)}`);
      await get().refresh();
    }
  },

  setMonitorEnabled: async (enabled) => {
    try {
      const on = await api.setMonitorEnabled(enabled);
      const { state } = get();
      if (state) set({ state: { ...state, mixer: { ...state.mixer, monitor_enabled: on } } });
    } catch (e) {
      toast(`Speaker monitoring unavailable: ${errText(e)}`);
      await get().refresh();
    }
  },

  setEnhancerSettings: async (settings) => {
    const { state } = get();
    if (state) set({ state: { ...state, mixer: { ...state.mixer, enhancer: settings } } });
    try {
      const applied = await api.setEnhancerSettings(settings);
      const cur = get().state;
      if (cur) set({ state: { ...cur, mixer: { ...cur.mixer, enhancer: applied } } });
    } catch (e) {
      toast(`Voice enhancer: ${errText(e)}`);
      await get().refresh();
    }
  },

  setAvSync: (auto, offset_ms) => {
    const { state } = get();
    if (state) set({ state: { ...state, mixer: { ...state.mixer, av_sync: { auto, offset_ms } } } });
    if (avTimer) clearTimeout(avTimer);
    avTimer = setTimeout(() => {
      avTimer = null;
      void api.avSyncSet(auto, offset_ms).catch((e) => toast(`A/V sync: ${errText(e)}`));
    }, 60);
  },

  setStreamPlatform: (v) => {
    const p = loadPrefs();
    set({ streamPlatform: v, streamUrl: p.urls[v] ?? "", streamError: null });
  },
  setStreamUrl: (v) => set({ streamUrl: v }),
  setStreamKey: (v) => set({ streamKey: v }),

  streamStart: async () => {
    set({ streamError: null });
    set((s) => ({ busy: { ...s.busy, stream: true } }));
    try {
      const { streamUrl, streamPlatform, streamKey, recordPath } = get();
      await api.streamSetSettings(streamUrl.trim(), streamKey.trim(), streamPlatform);
      savePrefs(streamPlatform, streamUrl.trim(), recordPath);
      await api.streamStart();
    } catch (e) {
      set({ streamError: errText(e) });
    } finally {
      set((s) => ({ busy: { ...s.busy, stream: false } }));
      await get().refresh();
    }
  },

  streamStop: async () => {
    try {
      await api.streamStop();
    } catch (e) {
      set({ streamError: errText(e) });
    }
    await get().refresh();
  },

  clearStreamError: () => set({ streamError: null }),

  setRecordPath: (v) => set({ recordPath: v }),
  setRecordFormat: (v) => set({ recordFormat: v }),

  recordStart: async () => {
    set({ recordError: null });
    try {
      const { recordPath, recordFormat, streamPlatform, streamUrl } = get();
      if (!recordPath.trim()) {
        set({ recordError: "Choose where to save the recording first." });
        return;
      }
      await api.recordSetSettings(recordPath.trim(), recordFormat, "high");
      savePrefs(streamPlatform, streamUrl.trim(), recordPath.trim());
      await api.recordStart();
    } catch (e) {
      set({ recordError: errText(e) });
    }
    await get().refresh();
  },

  recordStop: async () => {
    try {
      await api.recordStop();
    } catch (e) {
      set({ recordError: errText(e) });
    }
    await get().refresh();
  },

  recordPause: async () => {
    try {
      await api.recordPause();
    } catch (e) {
      set({ recordError: errText(e) });
    }
    await get().refresh();
  },

  recordResume: async () => {
    try {
      await api.recordResume();
    } catch (e) {
      set({ recordError: errText(e) });
    }
    await get().refresh();
  },

  clearRecordError: () => set({ recordError: null }),
}));
