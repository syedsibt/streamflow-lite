/**
 * Production state (zustand). The single source of truth for the Production
 * screen: scenes, sources, preview/program buses, transitions, mixer,
 * stream, record, and macros.
 *
 * Patterns mirror `lib/store.ts`: optimistic local updates, throttled
 * (60 Hz trailing) gain sync, toasts via the studio store's toaster (single
 * visible toaster in the app), and refresh-on-production-event via the shared
 * "sf-event" channel.
 *
 * Honesty rules (backend contract): stream/record start currently fail with
 * the backend's "not implemented" message. Failures surface as error pills +
 * toasts — the store never fabricates a live/recording state; indicators
 * only reflect the `production_state()` response.
 */
import { create } from "zustand";
import { api, isTauri, onSfEvent } from "./ipc";
import { useStudio } from "./store";
import type {
  EnhancerSettings,
  MacroActionInput,
  ProductionStateData,
  SourceKind,
} from "./types";

interface ProductionStore {
  ready: boolean;
  state: ProductionStateData | null;
  previewUrl: string | null;
  programUrl: string | null;
  thumbs: Record<string, string>;
  transitionDurationMs: number;
  transitionKind: string;
  streamUrl: string;
  streamPlatform: string;
  streamKey: string;
  streamError: string | null;
  recordPath: string;
  recordFormat: string;
  recordQuality: string;
  recordError: string | null;
  // Media playback pause state, keyed by source id (frontend mirror of
  // the backend producer state; refresh() does not reset it).
  mediaPaused: Record<string, boolean>;

  init: () => Promise<void>;
  dispose: () => void;
  refresh: () => Promise<void>;

  // Scenes / sources
  sceneCreate: (name: string) => Promise<void>;
  sceneRename: (scene_id: string, name: string) => Promise<void>;
  sceneDelete: (scene_id: string) => Promise<void>;
  sceneDuplicate: (scene_id: string) => Promise<void>;
  sourceAdd: (scene_id: string, kind: SourceKind, name: string) => Promise<void>;
  mediaSetPaused: (source_id: string, paused: boolean) => Promise<void>;
  sourceRemove: (scene_id: string, source_id: string) => Promise<void>;
  sourceReorder: (scene_id: string, order: string[]) => Promise<void>;
  sourceSetVisible: (
    scene_id: string,
    source_id: string,
    visible: boolean,
  ) => Promise<void>;

  // Buses + transitions
  setPreview: (scene_id: string) => Promise<void>;
  setProgram: (scene_id: string) => Promise<void>;
  transitionCut: () => Promise<void>;
  transitionAuto: () => Promise<void>;
  setTransitionDurationMs: (ms: number) => void;
  setTransitionKind: (kind: string) => void;

  // Monitor polling (call on an interval from the view)
  pollMonitors: () => Promise<void>;
  pollMultiview: () => Promise<void>;

  // Mixer
  setChannelGain: (source_id: string, gain: number) => void;
  setChannelMute: (source_id: string, mute: boolean) => Promise<void>;
  setMasterGain: (gain: number) => void;
  setMasterMute: (mute: boolean) => Promise<void>;
  setMonitorEnabled: (enabled: boolean) => Promise<void>;
  setEnhancerSettings: (settings: EnhancerSettings) => Promise<void>;

  // Stream
  setStreamUrl: (v: string) => void;
  setStreamPlatform: (v: string) => void;
  setStreamKey: (v: string) => void;
  streamSaveSettings: () => Promise<void>;
  streamStart: () => Promise<void>;
  streamStop: () => Promise<void>;
  clearStreamError: () => void;

  // Record
  setRecordPath: (v: string) => void;
  setRecordFormat: (v: string) => void;
  setRecordQuality: (v: string) => void;
  recordSaveSettings: () => Promise<void>;
  recordStart: () => Promise<void>;
  recordStop: () => Promise<void>;
  recordPause: () => Promise<void>;
  recordResume: () => Promise<void>;
  clearRecordError: () => void;

  // Macros
  macroAdd: (name: string, hotkey: string, action: MacroActionInput) => Promise<void>;
  macroRemove: (macro_id: string) => Promise<void>;
  macroDispatch: (hotkey: string) => Promise<void>;
}

const toast = (text: string) => useStudio.getState().pushToast(text);

/** Trailing 60 Hz sync for mixer gain drags (mirrors the studio store). */
let gainTimer: ReturnType<typeof setTimeout> | null = null;
/** "ch:<source_id>" -> gain, or "master" -> gain. Gains are 0..2.0. */
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
        toast(`mixer gain failed: ${String(e)}`);
      }
    }
  }, 16);
}

/** Idempotent event subscription (StrictMode mounts twice in dev). */
let unlisten: (() => void) | null = null;

/**
 * Normalize the backend's `production_state()` JSON into the frontend's
 * `ProductionStateData` shape. The backend (sf-ui-backend) is the tested
 * source of truth; its serde shapes differ from what the panels were first
 * written against, so every difference is reconciled here in ONE place:
 * - `stream_settings` + `stream_lifecycle` -> `stream: { state, message, settings }`
 * - `record_settings` + `record_lifecycle` -> `record: { state, elapsed_secs, message }`
 * - `mixer.channels` is a `{ source_id: strip }` map -> array, names joined
 *   from the scenes' sources
 * - `SourceState.kind` may be a string or an externally-tagged object
 *   (e.g. `{ Color: { r, g, b } }`) -> lowercase kind name
 * - `Macro.action` is internally-tagged (`{ action: "cut" }`) -> display label
 */
function kindToString(kind: unknown): string {
  if (typeof kind === "string") return kind.toLowerCase();
  if (kind && typeof kind === "object") {
    const key = Object.keys(kind as Record<string, unknown>)[0];
    return key ? key.toLowerCase() : "unknown";
  }
  return "unknown";
}

function macroActionLabel(action: unknown): string {
  if (typeof action === "string") return action;
  if (action && typeof action === "object") {
    const a = action as Record<string, unknown>;
    const tag = String(a.action ?? "unknown");
    if (tag === "auto_transition") return `auto_transition (${String(a.duration_ms ?? "?")}ms)`;
    if (tag === "apply_look")
      return `apply_look: ${String(a.look_id ?? "?")}`;
    return tag;
  }
  return "unknown";
}

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
        order: typeof s.order === "number" ? s.order : 0,
        status: String(s.status ?? ""),
        input_id: typeof s.input_id === "string" ? s.input_id : null,
      };
    }),
  }));
  const channelsObj = (r.mixer?.channels ?? {}) as Record<string, any>;
  const channels = Object.entries(channelsObj).map(([source_id, ch]: [string, any]) => ({
    source_id,
    name: nameBySource.get(source_id) ?? source_id,
    gain: typeof ch?.gain === "number" ? ch.gain : 1,
    mute: !!ch?.mute,
  }));
  const sl = (r.stream_lifecycle ?? {}) as Record<string, any>;
  const rl = (r.record_lifecycle ?? {}) as Record<string, any>;
  return {
    scenes,
    preview_scene_id: r.preview_scene_id ?? null,
    program_scene_id: r.program_scene_id ?? null,
    transition: {
      active: !!r.transition?.active,
      kind: String(r.transition?.kind ?? ""),
      progress: typeof r.transition?.progress === "number" ? r.transition.progress : 0,
    },
    mixer: {
      channels,
      master: {
        gain: typeof r.mixer?.master?.gain === "number" ? r.mixer.master.gain : 1,
        mute: !!r.mixer?.master?.mute,
      },
      meters_live: !!r.mixer?.meters_live,
      meters:
        r.mixer?.meters && typeof r.mixer.meters === "object"
          ? (r.mixer.meters as Record<string, number>)
          : {},
      monitor_enabled: !!r.mixer?.monitor_enabled,
      enhancer: {
        enabled: !!r.mixer?.enhancer?.enabled,
        gate_enabled: r.mixer?.enhancer?.gate_enabled !== false,
        gate_threshold_db:
          typeof r.mixer?.enhancer?.gate_threshold_db === "number"
            ? r.mixer.enhancer.gate_threshold_db
            : -40,
        eq_low_db:
          typeof r.mixer?.enhancer?.eq_low_db === "number"
            ? r.mixer.enhancer.eq_low_db
            : 0,
        eq_mid_db:
          typeof r.mixer?.enhancer?.eq_mid_db === "number"
            ? r.mixer.enhancer.eq_mid_db
            : 2,
        eq_high_db:
          typeof r.mixer?.enhancer?.eq_high_db === "number"
            ? r.mixer.enhancer.eq_high_db
            : 2,
        compressor_enabled: r.mixer?.enhancer?.compressor_enabled !== false,
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
      elapsed_secs: typeof rl.elapsed_secs === "number" ? rl.elapsed_secs : 0,
      message: typeof rl.message === "string" ? rl.message : null,
    },
    macros: (r.macros ?? []).map((m: any) => ({
      id: String(m.id ?? ""),
      name: String(m.name ?? ""),
      hotkey: String(m.hotkey ?? ""),
      action: macroActionLabel(m.action),
    })),
    video_settings: {
      output_width: Number(r.video_settings?.output_width ?? 1920),
      output_height: Number(r.video_settings?.output_height ?? 1080),
      output_fps: Number(r.video_settings?.output_fps ?? 30),
      encoder: String(r.video_settings?.encoder ?? "auto"),
      bitrate_kbps: Number(r.video_settings?.bitrate_kbps ?? 8000),
      camera_device:
        typeof r.video_settings?.camera_device === "string"
          ? r.video_settings.camera_device
          : null,
      camera_width: Number(r.video_settings?.camera_width ?? 1280),
      camera_height: Number(r.video_settings?.camera_height ?? 720),
      camera_fps: Number(r.video_settings?.camera_fps ?? 30),
    },
  };
}

/** Minimum gap between state refreshes driven by transition_progress. */
let lastProgressRefresh = 0;

export const useProduction = create<ProductionStore>()((set, get) => ({
  ready: false,
  state: null,
  previewUrl: null,
  programUrl: null,
  thumbs: {},
  transitionDurationMs: 500,
  transitionKind: "Mix",
  streamUrl: "",
  streamPlatform: "YouTube",
  streamKey: "",
  streamError: null,
  recordPath: "recording.mp4",
  recordFormat: "mp4",
  recordQuality: "high",
  recordError: null,
  mediaPaused: {},

  init: async () => {
    if (!isTauri) return; // placeholder mode in plain browsers
    await get().refresh();
    if (unlisten) return;
    unlisten = await onSfEvent((e) => {
      const st = get();
      switch (e.event) {
        case "production.scene_changed":
        case "production.source_changed":
        case "production.transition_done":
        case "production.stream_state":
        case "production.record_state":
        case "production.mixer_changed":
          void st.refresh();
          break;
        case "production.transition_progress": {
          // Throttle: transitions can emit progress at frame rate.
          const now = Date.now();
          if (now - lastProgressRefresh > 150) {
            lastProgressRefresh = now;
            void st.refresh();
          }
          break;
        }
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
    if (!isTauri) return;
    try {
      const raw = await api.productionState();
      const state = normalizeProductionState(raw);
      const prev = get().state;
      set({ state, ready: true });
      // Surface backend lifecycle error messages as dismissible error pills
      // (only on entering the error state, so dismissing sticks).
      if (
        state.stream.state === "error" &&
        state.stream.message &&
        prev?.stream.state !== "error"
      ) {
        set({ streamError: state.stream.message });
      }
      if (
        state.record.state === "error" &&
        state.record.message &&
        prev?.record.state !== "error"
      ) {
        set({ recordError: state.record.message });
      }
    } catch (e) {
      toast(`production_state failed: ${String(e)}`);
    }
  },

  // ------------------------------------------------------------------ scenes
  sceneCreate: async (name) => {
    try {
      await api.sceneCreate(name);
      await get().refresh();
    } catch (e) {
      toast(`scene_create failed: ${String(e)}`);
    }
  },

  sceneRename: async (scene_id, name) => {
    try {
      await api.sceneRename(scene_id, name);
      await get().refresh();
    } catch (e) {
      toast(`scene_rename failed: ${String(e)}`);
    }
  },

  sceneDelete: async (scene_id) => {
    try {
      await api.sceneDelete(scene_id);
      await get().refresh();
    } catch (e) {
      toast(`scene_delete failed: ${String(e)}`);
    }
  },

  sceneDuplicate: async (scene_id) => {
    try {
      await api.sceneDuplicate(scene_id);
      await get().refresh();
    } catch (e) {
      toast(`scene_duplicate failed: ${String(e)}`);
    }
  },

  sourceAdd: async (scene_id, kind, name) => {
    try {
      await api.sourceAdd(scene_id, kind, name);
      await get().refresh();
    } catch (e) {
      toast(`source_add failed: ${String(e)}`);
    }
  },

  sourceRemove: async (scene_id, source_id) => {
    try {
      await api.sourceRemove(scene_id, source_id);
      await get().refresh();
    } catch (e) {
      toast(`source_remove failed: ${String(e)}`);
    }
  },

  sourceReorder: async (scene_id, order) => {
    try {
      await api.sourceReorder(scene_id, order);
      await get().refresh();
    } catch (e) {
      toast(`source_reorder failed: ${String(e)}`);
    }
  },

  sourceSetVisible: async (scene_id, source_id, visible) => {
    // Optimistic.
    const { state } = get();
    if (state) {
      set({
        state: {
          ...state,
          scenes: state.scenes.map((sc) =>
            sc.id === scene_id
              ? {
                  ...sc,
                  sources: sc.sources.map((s) =>
                    s.id === source_id ? { ...s, visible } : s,
                  ),
                }
              : sc,
          ),
        },
      });
    }
    try {
      await api.sourceSetVisible(scene_id, source_id, visible);
    } catch (e) {
      toast(`source_set_visible failed: ${String(e)}`);
      await get().refresh();
    }
  },

  mediaSetPaused: async (source_id, paused) => {
    // Optimistic.
    set((s) => ({ mediaPaused: { ...s.mediaPaused, [source_id]: paused } }));
    try {
      const actual = await api.mediaSetPaused(source_id, paused);
      set((s) => ({ mediaPaused: { ...s.mediaPaused, [source_id]: actual } }));
    } catch (e) {
      toast(`media pause failed: ${String(e)}`);
      set((s) => ({
        mediaPaused: { ...s.mediaPaused, [source_id]: !paused },
      }));
    }
  },

  // -------------------------------------------------------- buses/transitions
  setPreview: async (scene_id) => {
    try {
      await api.productionSetPreview(scene_id);
      await get().refresh();
    } catch (e) {
      toast(`production_set_preview failed: ${String(e)}`);
    }
  },

  setProgram: async (scene_id) => {
    try {
      await api.productionSetProgram(scene_id);
      await get().refresh();
    } catch (e) {
      toast(`production_set_program failed: ${String(e)}`);
    }
  },

  transitionCut: async () => {
    try {
      await api.transitionCut();
      await get().refresh();
    } catch (e) {
      toast(`transition_cut failed: ${String(e)}`);
    }
  },

  transitionAuto: async () => {
    const { transitionDurationMs, transitionKind } = get();
    try {
      // The backend only accepts the lowercase kind name ("mix").
      await api.transitionAuto(transitionDurationMs, transitionKind.toLowerCase());
      await get().refresh();
    } catch (e) {
      toast(`transition_auto failed: ${String(e)}`);
    }
  },

  setTransitionDurationMs: (ms) => set({ transitionDurationMs: ms }),
  setTransitionKind: (kind) => set({ transitionKind: kind }),

  pollMonitors: async () => {
    const { state } = get();
    if (!isTauri || !state) return;
    try {
      const [previewUrl, programUrl] = await Promise.all([
        state.preview_scene_id
          ? api.scenePreviewJpg(state.preview_scene_id)
          : Promise.resolve(null),
        state.program_scene_id
          ? api.scenePreviewJpg(state.program_scene_id)
          : Promise.resolve(null),
      ]);
      set((s) => ({
        previewUrl: previewUrl ?? s.previewUrl,
        programUrl: programUrl ?? s.programUrl,
      }));
    } catch {
      /* silent: monitors are best-effort like the studio preview */
    }
  },

  pollMultiview: async () => {
    const { state } = get();
    if (!isTauri || !state) return;
    for (const sc of state.scenes) {
      try {
        const url = await api.scenePreviewJpg(sc.id);
        set((s) => ({ thumbs: { ...s.thumbs, [sc.id]: url } }));
      } catch {
        /* skip this scene this round */
      }
    }
  },

  // ------------------------------------------------------------------- mixer
  setChannelGain: (source_id, gain) => {
    const { state } = get();
    if (state) {
      set({
        state: {
          ...state,
          mixer: {
            ...state.mixer,
            channels: state.mixer.channels.map((c) =>
              c.source_id === source_id ? { ...c, gain } : c,
            ),
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
            channels: state.mixer.channels.map((c) =>
              c.source_id === source_id ? { ...c, mute } : c,
            ),
          },
        },
      });
    }
    try {
      await api.mixerSetMute(source_id, mute);
    } catch (e) {
      toast(`mixer_set_mute failed: ${String(e)}`);
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
      set({
        state: {
          ...state,
          mixer: { ...state.mixer, master: { ...state.mixer.master, mute } },
        },
      });
    }
    try {
      await api.mixerSetMasterMute(mute);
    } catch (e) {
      toast(`mixer_set_master_mute failed: ${String(e)}`);
      await get().refresh();
    }
  },

  setMonitorEnabled: async (enabled) => {
    try {
      const on = await api.setMonitorEnabled(enabled);
      const { state } = get();
      if (state) {
        set({
          state: { ...state, mixer: { ...state.mixer, monitor_enabled: on } },
        });
      }
      if (enabled && !on) {
        toast("Could not start audio monitor — no output device?");
      }
    } catch (e) {
      toast(`set_monitor_enabled failed: ${String(e)}`);
      await get().refresh();
    }
  },

  setEnhancerSettings: async (settings) => {
    const { state } = get();
    if (state) {
      set({ state: { ...state, mixer: { ...state.mixer, enhancer: settings } } });
    }
    try {
      const applied = await api.setEnhancerSettings(settings);
      const cur = get().state;
      if (cur) {
        set({ state: { ...cur, mixer: { ...cur.mixer, enhancer: applied } } });
      }
    } catch (e) {
      toast(`enhancer failed: ${String(e)}`);
      await get().refresh();
    }
  },

  // ------------------------------------------------------------------ stream
  setStreamUrl: (v) => set({ streamUrl: v }),
  setStreamPlatform: (v) => set({ streamPlatform: v }),
  setStreamKey: (v) => set({ streamKey: v }),

  streamSaveSettings: async () => {
    const { streamUrl, streamPlatform, streamKey } = get();
    // The backend parses lowercase platform ids ("youtube" | "twitch" |
    // "facebook" | "custom_rtmp"); the UI shows display labels.
    const platform = streamPlatform.toLowerCase().replace(/ /g, "_");
    try {
      await api.streamSetSettings(streamUrl, streamKey, platform);
      toast("stream settings saved");
    } catch (e) {
      toast(`stream_set_settings failed: ${String(e)}`);
    }
  },

  streamStart: async () => {
    set({ streamError: null });
    try {
      // Auto-save the current UI settings so Start works even if the user
      // never pressed Save (backend defaults to empty URL/key).
      const { streamUrl, streamPlatform, streamKey } = get();
      const platform = streamPlatform.toLowerCase().replace(/ /g, "_");
      await api.streamSetSettings(streamUrl, streamKey, platform);
      await api.streamStart();
      await get().refresh();
    } catch (e) {
      // Honest: the backend currently returns "not implemented" — surface it.
      const msg = `stream_start failed: ${String(e)}`;
      set({ streamError: msg });
      toast(msg);
      await get().refresh();
    }
  },

  streamStop: async () => {
    set({ streamError: null });
    try {
      await api.streamStop();
      await get().refresh();
    } catch (e) {
      const msg = `stream_stop failed: ${String(e)}`;
      set({ streamError: msg });
      toast(msg);
      await get().refresh();
    }
  },

  clearStreamError: () => set({ streamError: null }),

  // ------------------------------------------------------------------ record
  setRecordPath: (v) => set({ recordPath: v }),
  setRecordFormat: (v) => set({ recordFormat: v }),
  setRecordQuality: (v) => set({ recordQuality: v }),

  recordSaveSettings: async () => {
    const { recordPath, recordFormat, recordQuality } = get();
    try {
      await api.recordSetSettings(recordPath, recordFormat, recordQuality);
      toast("record settings saved");
    } catch (e) {
      toast(`record_set_settings failed: ${String(e)}`);
    }
  },

  recordStart: async () => {
    set({ recordError: null });
    try {
      // Auto-save the current UI settings so Start works even if the user
      // never pressed Save (backend defaults to an empty path).
      const { recordPath, recordFormat, recordQuality } = get();
      await api.recordSetSettings(recordPath, recordFormat, recordQuality);
      await api.recordStart();
      await get().refresh();
    } catch (e) {
      const msg = `record_start failed: ${String(e)}`;
      set({ recordError: msg });
      toast(msg);
      await get().refresh();
    }
  },

  recordStop: async () => {
    set({ recordError: null });
    try {
      await api.recordStop();
      await get().refresh();
    } catch (e) {
      const msg = `record_stop failed: ${String(e)}`;
      set({ recordError: msg });
      toast(msg);
      await get().refresh();
    }
  },

  recordPause: async () => {
    try {
      await api.recordPause();
      await get().refresh();
    } catch (e) {
      const msg = `record_pause failed: ${String(e)}`;
      set({ recordError: msg });
      toast(msg);
    }
  },

  recordResume: async () => {
    try {
      await api.recordResume();
      await get().refresh();
    } catch (e) {
      const msg = `record_resume failed: ${String(e)}`;
      set({ recordError: msg });
      toast(msg);
    }
  },

  clearRecordError: () => set({ recordError: null }),

  // ------------------------------------------------------------------ macros
  macroAdd: async (name, hotkey, action) => {
    try {
      await api.macroAdd(name, hotkey, action);
      await get().refresh();
    } catch (e) {
      toast(`macro_add failed: ${String(e)}`);
    }
  },

  macroRemove: async (macro_id) => {
    try {
      await api.macroRemove(macro_id);
      await get().refresh();
    } catch (e) {
      toast(`macro_remove failed: ${String(e)}`);
    }
  },

  macroDispatch: async (hotkey) => {
    try {
      await api.macroDispatch(hotkey);
      await get().refresh();
    } catch (e) {
      toast(`macro_dispatch failed: ${String(e)}`);
    }
  },
}));
