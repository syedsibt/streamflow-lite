/**
 * Studio state (zustand). The single source of truth for the Color Studio
 * screen. Spec §8 rules implemented here:
 * - slider drags update the backend throttled to 60 Hz (trailing);
 * - double-click a slider resets that parameter (see `resetParam`).
 */
import { create } from "zustand";
import { api, isTauri, onSfEvent } from "./ipc";
import type {
  ColorGrade,
  ColorPresetInfo,
  CurveName,
  EffectKindInfo,
  EffectParams,
  FilterTarget,
  LookInfo,
  LutInfo,
  PreviewMode,
  ScopeDataView,
  ScopeKindName,
  SlotView,
} from "./types";

export type TabId =
  | "basic"
  | "creative"
  | "curves"
  | "wheels"
  | "lut"
  | "effects"
  | "filters"
  | "presets";

interface Toast {
  id: number;
  text: string;
}

interface StudioState {
  ready: boolean;
  inputId: string;
  inputName: string;
  grade: ColorGrade | null;
  stack: SlotView[];
  looks: LookInfo[];
  effectKinds: EffectKindInfo[];
  scopes: Partial<Record<ScopeKindName, ScopeDataView>>;
  previewUrl: string | null;
  activeTab: TabId;
  lookIntensity: number; // 0..1 — drives the active look live
  toasts: Toast[];
  autoColorNote: string[]; // last auto-color explanations
  previewMode: PreviewMode;
  luts: LutInfo[];
  presets: ColorPresetInfo[];

  setPreviewMode: (m: PreviewMode) => void;
  refreshLuts: () => Promise<void>;
  /** Opens a file picker, validates + imports the .cube, applies it. */
  importLut: () => Promise<void>;
  applyLibraryLut: (lut: LutInfo) => Promise<void>;
  removeLut: (id: string) => Promise<void>;
  setLutIntensity: (v: number) => void;
  setLutPosition: (pos: "Before" | "After") => Promise<void>;
  refreshPresets: () => Promise<void>;
  savePreset: (name: string) => Promise<void>;
  applyPreset: (id: string) => Promise<void>;
  deletePreset: (id: string) => Promise<void>;
  clearLook: () => Promise<void>;
  /** Slot of the given effect kind (PascalCase, e.g. "Bloom"), if present. */
  effectSlot: (kind: string) => SlotView | undefined;
  /** Adds the effect if missing; returns its slot id. */
  ensureEffect: (kind: string) => Promise<string | null>;
  removeEffect: (kind: string) => Promise<void>;
  /** Live effect parameter edit (throttled). */
  setEffectParams: (kind: string, patch: Partial<EffectParams>) => Promise<void>;

  init: () => Promise<void>;
  setTab: (t: TabId) => void;
  /** Switch the Color Studio target to another engine input (a real
   * production source). Reloads grade/stack/scopes/preview for it. */
  setInputId: (inputId: string, inputName: string) => Promise<void>;
  setLookIntensity: (v: number) => void;
  setParam: (param: string, value: number) => void;
  resetParam: (param: string) => Promise<void>;
  setCurve: (curve: CurveName, points: [number, number][]) => Promise<void>;
  loadLut: (path: string, position: string, intensity: number) => Promise<void>;
  resetSection: (section: string | null) => Promise<void>;
  autoColor: () => Promise<void>;
  refreshStack: () => Promise<void>;
  addEffect: (kind: string) => Promise<void>;
  setSlotIntensity: (slotId: string, v: number) => Promise<void>;
  toggleSlot: (slotId: string, enabled: boolean) => Promise<void>;
  removeSlot: (slotId: string) => Promise<void>;
  moveSlot: (slotId: string, dir: -1 | 1) => Promise<void>;
  applyLook: (lookId: string) => Promise<void>;
  applyGradeSnapshot: (grade: ColorGrade) => Promise<void>;
  pollPreview: () => Promise<void>;
  pushToast: (text: string) => void;
}

let toastId = 0;
let previewInFlight = false;
/** Trailing throttles for look intensity / effect params (one per key). */
const throttles = new Map<string, ReturnType<typeof setTimeout>>();
function throttled(key: string, ms: number, fn: () => void) {
  const t = throttles.get(key);
  if (t) clearTimeout(t);
  throttles.set(
    key,
    setTimeout(() => {
      throttles.delete(key);
      fn();
    }, ms),
  );
}
/** Trailing 60 Hz sync for slider drags (spec §8). */
let syncTimer: ReturnType<typeof setTimeout> | null = null;
const pendingSync = new Map<string, number>();

function targetOf(inputId: string): FilterTarget {
  return { input: inputId };
}

export const useStudio = create<StudioState>()((set, get) => {
  function scheduleSync() {
    if (syncTimer !== null) return;
    syncTimer = setTimeout(async () => {
      syncTimer = null;
      const batch = [...pendingSync.entries()];
      pendingSync.clear();
      const { inputId, pushToast } = get();
      for (const [param, value] of batch) {
        try {
          const grade = await api.colorSetParam(inputId, param, value);
          set({ grade });
        } catch (e) {
          pushToast(`set_param failed: ${String(e)}`);
        }
      }
    }, 16);
  }

  return {
    ready: false,
    inputId: "cam1",
    inputName: "Camera 1",
    grade: null,
    stack: [],
    looks: [],
    effectKinds: [],
    scopes: {},
    previewUrl: null,
    activeTab: "basic",
    lookIntensity: 0.8,
    toasts: [],
    autoColorNote: [],
    previewMode: "after",
    luts: [],
    presets: [],

    init: async () => {
      if (!isTauri) return; // placeholder mode in plain browsers
      try {
        const { inputId } = get();
        const [grade, stack, looks, effectKinds] = await Promise.all([
          api.gradeSnapshot(inputId),
          api.stackSnapshot(targetOf(inputId)),
          api.listLooks(),
          api.listEffectKinds(),
        ]);
        set({ grade, stack, looks, effectKinds, ready: true });
        void get().refreshLuts();
        void get().refreshPresets();
        // Subscribe the three main scopes (spec §8 scopes row).
        for (const kind of ["waveform", "vectorscope", "histogram"] as const) {
          await api.scopesSubscribe(inputId, kind).catch(() => {});
        }
        await onSfEvent((e) => {
          const st = get();
          switch (e.event) {
            case "scopes.frame":
              if (e.input_id === st.inputId) {
                set({ scopes: { ...st.scopes, [e.kind]: e.data } });
              }
              break;
            case "gpu.cost_warning":
            case "color.drift_suggest":
              st.pushToast(e.event === "gpu.cost_warning" ? e.message : e.message);
              break;
            case "filter.stack_changed":
              void st.refreshStack();
              break;
            case "color.changed":
              // Grade already updated optimistically; refresh cheaply.
              void api
                .gradeSnapshot(st.inputId)
                .then((grade) => set({ grade }))
                .catch(() => {});
              break;
            default:
              break;
          }
        });
      } catch (e) {
        get().pushToast(`backend unreachable: ${String(e)}`);
      }
    },

    setTab: (t) => set({ activeTab: t }),
    setLookIntensity: (v) => {
      set({ lookIntensity: v });
      // Drive the active look live (one active look per input).
      const look = get().stack.find((s) => s.kind === "look");
      if (look) {
        set({
          stack: get().stack.map((s) => (s.id === look.id ? { ...s, intensity: v } : s)),
        });
        throttled("look-intensity", 30, () => {
          const { inputId, pushToast } = get();
          api
            .filterSetIntensity(targetOf(inputId), look.id, v)
            .catch((e) => pushToast(`look intensity failed: ${String(e)}`));
        });
      }
    },

    setInputId: async (inputId, inputName) => {
      const prev = get();
      if (prev.inputId === inputId || !isTauri) {
        set({ inputId, inputName });
        return;
      }
      // Unsubscribe scopes for the old input so its frames stop arriving.
      for (const kind of ["waveform", "vectorscope", "histogram"] as const) {
        await api.scopesUnsubscribe(prev.inputId, kind).catch(() => {});
      }
      set({
        inputId,
        inputName,
        grade: null,
        stack: [],
        scopes: {},
        previewUrl: null,
      });
      try {
        const [grade, stack] = await Promise.all([
          api.gradeSnapshot(inputId),
          api.stackSnapshot(targetOf(inputId)),
        ]);
        set({ grade, stack });
        for (const kind of ["waveform", "vectorscope", "histogram"] as const) {
          await api.scopesSubscribe(inputId, kind).catch(() => {});
        }
      } catch (e) {
        get().pushToast(`input switch failed: ${String(e)}`);
      }
    },

    setParam: (param, value) => {
      const { grade } = get();
      if (grade) {
        // Optimistic local update for responsive sliders.
        const next =
          param === "lut_intensity"
            ? ({ ...grade, lut: grade.lut ? { ...grade.lut, intensity: value } : null } as ColorGrade)
            : ({ ...grade, [param]: value } as ColorGrade);
        set({ grade: next });
      }
      pendingSync.set(param, value);
      scheduleSync();
    },

    resetParam: async (param) => {
      const { inputId, pushToast } = get();
      try {
        const grade = await api.colorResetParam(inputId, param);
        set({ grade });
      } catch (e) {
        pushToast(`reset failed: ${String(e)}`);
      }
    },

    setCurve: async (curve, points) => {
      const { inputId, pushToast } = get();
      try {
        await api.colorSetCurve(inputId, curve, points);
        const grade = await api.gradeSnapshot(inputId);
        set({ grade });
      } catch (e) {
        pushToast(`curve update failed: ${String(e)}`);
      }
    },

    loadLut: async (path, position, intensity) => {
      const { inputId, pushToast } = get();
      try {
        await api.colorLoadLut(inputId, path, position, intensity);
        const grade = await api.gradeSnapshot(inputId);
        set({ grade });
      } catch (e) {
        pushToast(`LUT load failed: ${String(e)}`);
      }
    },

    resetSection: async (section) => {
      const { inputId, pushToast } = get();
      try {
        const grade = await api.colorReset(inputId, section);
        set({ grade });
      } catch (e) {
        pushToast(`reset failed: ${String(e)}`);
      }
    },

    autoColor: async () => {
      const { inputId, pushToast } = get();
      try {
        const res = await api.colorAuto(inputId);
        set({ grade: res.grade, autoColorNote: res.explanation });
      } catch (e) {
        pushToast(`auto color failed: ${String(e)}`);
      }
    },

    refreshStack: async () => {
      const { inputId } = get();
      try {
        const stack = await api.stackSnapshot(targetOf(inputId));
        set({ stack });
      } catch {
        /* toast storm avoidance: silent */
      }
    },

    addEffect: async (kind) => {
      const { inputId, effectKinds, pushToast, refreshStack } = get();
      const info = effectKinds.find((k) => k.kind === kind);
      if (!info) return;
      try {
        await api.filterAdd(targetOf(inputId), {
          Effect: { kind: kind as never, params: info.default_params },
        });
        await refreshStack();
      } catch (e) {
        pushToast(`add filter failed: ${String(e)}`);
      }
    },

    setSlotIntensity: async (slotId, v) => {
      const { inputId, pushToast, refreshStack } = get();
      // Optimistic.
      set({
        stack: get().stack.map((s) => (s.id === slotId ? { ...s, intensity: v } : s)),
      });
      try {
        await api.filterSetIntensity(targetOf(inputId), slotId, v);
      } catch (e) {
        pushToast(`intensity failed: ${String(e)}`);
        await refreshStack();
      }
    },

    toggleSlot: async (slotId, enabled) => {
      const { inputId, pushToast, refreshStack } = get();
      set({
        stack: get().stack.map((s) => (s.id === slotId ? { ...s, enabled } : s)),
      });
      try {
        await api.filterToggle(targetOf(inputId), slotId, enabled);
      } catch (e) {
        pushToast(`toggle failed: ${String(e)}`);
        await refreshStack();
      }
    },

    removeSlot: async (slotId) => {
      const { inputId, pushToast, refreshStack } = get();
      try {
        await api.filterRemove(targetOf(inputId), slotId);
        await refreshStack();
      } catch (e) {
        pushToast(`remove failed: ${String(e)}`);
      }
    },

    moveSlot: async (slotId, dir) => {
      const { inputId, stack, pushToast, refreshStack } = get();
      const idx = stack.findIndex((s) => s.id === slotId);
      const j = idx + dir;
      if (idx < 0 || j < 0 || j >= stack.length) return;
      const order = stack.map((s) => s.id);
      [order[idx], order[j]] = [order[j], order[idx]];
      try {
        await api.filterReorder(targetOf(inputId), order);
        await refreshStack();
      } catch (e) {
        pushToast(`reorder failed: ${String(e)}`);
      }
    },

    applyLook: async (lookId) => {
      const { inputId, lookIntensity, pushToast, refreshStack } = get();
      try {
        await api.lookApply(targetOf(inputId), lookId, lookIntensity);
        await refreshStack();
        const grade = await api.gradeSnapshot(inputId);
        set({ grade });
      } catch (e) {
        pushToast(`apply look failed: ${String(e)}`);
      }
    },

    applyGradeSnapshot: async (grade) => {
      const { inputId, pushToast } = get();
      try {
        const next = await api.applyGrade(inputId, grade);
        set({ grade: next });
      } catch (e) {
        pushToast(`apply grade failed: ${String(e)}`);
      }
    },

    pollPreview: async () => {
      const { inputId, previewMode } = get();
      if (!isTauri || previewInFlight) return; // never queue requests up
      previewInFlight = true;
      try {
        const url = await api.previewGetFrame(inputId, 640, 360, previewMode);
        set({ previewUrl: url });
      } catch {
        /* silent: next poll retries */
      } finally {
        previewInFlight = false;
      }
    },

    setPreviewMode: (m) => set({ previewMode: m }),

    refreshLuts: async () => {
      try {
        set({ luts: await api.lutLibrary() });
      } catch {
        /* outside Tauri */
      }
    },

    importLut: async () => {
      const { pushToast } = get();
      if (!isTauri) return;
      try {
        const { open } = await import("@tauri-apps/plugin-dialog");
        const picked = await open({
          multiple: false,
          title: "Import LUT (.cube)",
          filters: [{ name: "Cube LUT", extensions: ["cube", "CUBE"] }],
        });
        if (!picked || Array.isArray(picked)) return;
        const lut = await api.lutImport(picked);
        await get().refreshLuts();
        for (const w of lut.warnings) pushToast(`LUT note: ${w}`);
        await get().applyLibraryLut(lut);
        pushToast(`LUT imported: ${lut.name}`);
      } catch (e) {
        pushToast(String(e));
      }
    },

    applyLibraryLut: async (lut) => {
      const { inputId, grade, pushToast } = get();
      const intensity = grade?.lut?.intensity ?? 1;
      const position = grade?.lut?.position ?? "After";
      try {
        await api.colorLoadLut(inputId, lut.path, position, intensity);
        set({ grade: await api.gradeSnapshot(inputId) });
      } catch (e) {
        pushToast(String(e));
      }
    },

    removeLut: async (id) => {
      const { pushToast, inputId } = get();
      try {
        await api.lutRemove(id);
        await get().refreshLuts();
        set({ grade: await api.gradeSnapshot(inputId) });
      } catch (e) {
        pushToast(String(e));
      }
    },

    setLutIntensity: (v) => get().setParam("lut_intensity", v),

    setLutPosition: async (pos) => {
      const { inputId, grade, pushToast } = get();
      if (!grade?.lut) return;
      try {
        await api.colorLoadLut(inputId, grade.lut.file, pos, grade.lut.intensity);
        set({ grade: await api.gradeSnapshot(inputId) });
      } catch (e) {
        pushToast(String(e));
      }
    },

    refreshPresets: async () => {
      try {
        set({ presets: await api.colorPresetList() });
      } catch {
        /* outside Tauri */
      }
    },

    savePreset: async (name) => {
      const { inputId, pushToast } = get();
      try {
        const p = await api.colorPresetSave(inputId, name);
        await get().refreshPresets();
        pushToast(`Preset saved: ${p.name}`);
      } catch (e) {
        pushToast(String(e));
      }
    },

    applyPreset: async (id) => {
      const { inputId, pushToast, refreshStack } = get();
      try {
        const grade = await api.colorPresetApply(inputId, id);
        set({ grade });
        await refreshStack();
      } catch (e) {
        pushToast(String(e));
      }
    },

    deletePreset: async (id) => {
      const { pushToast } = get();
      try {
        await api.colorPresetDelete(id);
        await get().refreshPresets();
      } catch (e) {
        pushToast(String(e));
      }
    },

    clearLook: async () => {
      const { inputId, pushToast, refreshStack } = get();
      try {
        await api.lookClear(targetOf(inputId));
        await refreshStack();
        set({ grade: await api.gradeSnapshot(inputId) });
      } catch (e) {
        pushToast(`clear look failed: ${String(e)}`);
      }
    },

    effectSlot: (kind) => get().stack.find((s) => s.kind === kind.toLowerCase()),

    ensureEffect: async (kind) => {
      const existing = get().effectSlot(kind);
      if (existing) return existing.id;
      const { inputId, effectKinds, pushToast, refreshStack } = get();
      const info = effectKinds.find((k) => k.kind === kind.toLowerCase());
      if (!info) return null;
      try {
        const id = await api.filterAdd(targetOf(inputId), {
          Effect: { kind: kind as never, params: info.default_params },
        });
        await refreshStack();
        return id;
      } catch (e) {
        pushToast(`add ${kind} failed: ${String(e)}`);
        return null;
      }
    },

    removeEffect: async (kind) => {
      const slot = get().effectSlot(kind);
      if (slot) await get().removeSlot(slot.id);
    },

    setEffectParams: async (kind, patch) => {
      const id = await get().ensureEffect(kind);
      if (!id) return;
      const slot = get().stack.find((s) => s.id === id);
      const info = get().effectKinds.find((k) => k.kind === kind.toLowerCase());
      const base = (slot?.params ?? (info?.default_params as unknown as EffectParams)) as EffectParams;
      const params = { ...base, ...patch } as EffectParams;
      set({ stack: get().stack.map((s) => (s.id === id ? { ...s, params } : s)) });
      throttled(`fx-${kind}`, 30, () => {
        const { inputId, pushToast } = get();
        api
          .filterSetParams(targetOf(inputId), id, params)
          .catch((e) => pushToast(`${kind} update failed: ${String(e)}`));
      });
    },

    pushToast: (text) => {
      const id = ++toastId;
      set({ toasts: [...get().toasts, { id, text }] });
      setTimeout(() => {
        set({ toasts: get().toasts.filter((t) => t.id !== id) });
      }, 5000);
    },
  };
});
