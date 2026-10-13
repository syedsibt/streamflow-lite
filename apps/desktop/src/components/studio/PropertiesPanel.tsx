import { useEffect, useState } from "react";
import { api } from "../../lib/ipc";
import { errText, useProduction } from "../../lib/productionStore";
import { useStudio, type TabId } from "../../lib/store";
import type { CameraSourceInfo, EnhancerSettings, FitMode, SceneLayout, SourceDiag, SourceTransform, SourceView } from "../../lib/types";
import BasicTab from "../tabs/BasicTab";
import CreativeTab from "../tabs/CreativeTab";
import CurvesTab from "../tabs/CurvesTab";
import WheelsTab from "../tabs/WheelsTab";
import LutTab from "../tabs/LutTab";
import EffectsTab from "../tabs/EffectsTab";
import FiltersTab from "../tabs/FiltersTab";
import PresetsTab from "../tabs/PresetsTab";
import { KIND_NAME } from "./SourceStrip";

type PTab = string;

export interface ExtraTab {
  id: string;
  label: string;
  node: React.ReactNode;
}

const FULL_T: SourceTransform = { x: 0, y: 0, w: 1, h: 1, crop_left: 0, crop_top: 0, crop_right: 0, crop_bottom: 0, fit: "fill", opacity: 1, rotation: 0 };

/** Properties of the selected source; Audio processing is global. */
export default function PropertiesPanel({ onOpenColorPage, extra = [] }: { onOpenColorPage: () => void; extra?: ExtraTab[] }) {
  const state = useProduction((s) => s.state);
  const selectedId = useProduction((s) => s.selectedSourceId);
  const sceneId = useProduction((s) => s.sceneId());
  const [tab, setTab] = useState<PTab>("source");
  const scene = state?.scenes.find((s) => s.id === sceneId);
  const source = scene?.sources.find((s) => s.id === selectedId) ?? null;
  const visual = source && source.kind !== "audioinput";

  return (
    <div className="props">
      <div className="ptabs" role="tablist" aria-label="Properties">
        {(
          [
            ["source", "Source"],
            ["transform", "Transform"],
            ["color", "Color & LUT"],
            ["audio", "Audio processing"],
            ...extra.map((x) => [x.id, x.label]),
          ] as [PTab, string][]
        ).map(([id, label]) => (
          <button key={id} type="button" role="tab" aria-selected={tab === id} className={tab === id ? "on" : ""} onClick={() => setTab(id)}>
            {label}
          </button>
        ))}
        {source && <span className="ptitle" title={source.name}>{source.name}</span>}
      </div>
      <div className="pbody">
        {tab === "source" && (source ? <SourceTab key={source.id} source={source} sources={scene!.sources} /> : <NoSelection />)}
        {tab === "transform" && (
          <>
            <LayoutPresets />
            {visual ? <TransformTab key={source!.id} source={source!} /> : <NoSelection what="a video, image or text source to place it" />}
          </>
        )}
        {tab === "color" && (visual && source!.input_id ? <ColorTab source={source!} onOpen={onOpenColorPage} /> : <NoSelection what="a video, image or text source to grade it" />)}
        {tab === "audio" && <AudioProcessing />}
        {extra.map((x) => (tab === x.id ? <div key={x.id} className="embedded">{x.node}</div> : null))}
      </div>
    </div>
  );
}

function NoSelection({ what = "a source" }: { what?: string }) {
  return <div className="panel-empty">Select {what} in the Sources row or on the preview.</div>;
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

function KV({ k, v, tone }: { k: string; v: string; tone?: "ok" | "warn" | "bad" }) {
  return (
    <>
      <dt>{k}</dt>
      <dd className={tone ?? ""}>{v}</dd>
    </>
  );
}

/** Camera health from measured stage rates — never a bare "waiting". */
export function CameraHealth({ d, target }: { d: SourceDiag | undefined; target: number }) {
  if (!d) return <p className="note">No measurements yet.</p>;
  const f1 = (v: number | null | undefined) => (v == null ? "—" : v.toFixed(1));
  const nominal = Number(/@(\d+(?:\.\d+)?)fps/.exec(d.format ?? "")?.[1] ?? 0) || target;
  const sends = d.device_packet_fps;
  let verdict: { text: string; tone: "ok" | "warn" | "bad" } = { text: "Frame delivery stable", tone: "ok" };
  if (d.error) verdict = { text: d.error, tone: "bad" };
  else if (!d.width) verdict = { text: sends ? `Device sends ${sends.toFixed(0)} fps but no picture decoded yet` : "Device is not sending frames", tone: "warn" };
  else if (sends != null && sends < nominal * 0.8) verdict = { text: `Camera itself sends only ${sends.toFixed(1)} of ${nominal} fps — mode, USB bandwidth or low-light exposure`, tone: "warn" };
  else if (d.device_fps != null && sends != null && d.device_fps < Math.min(sends, target) * 0.8) verdict = { text: "Decoder can't keep up — capture is the bottleneck", tone: "warn" };
  else if (d.fps < Math.min(nominal, target) * 0.8) verdict = { text: "Render loop is late — frames are captured but not used", tone: "warn" };
  const lat = d.age_ms;
  return (
    <>
      <div className={"verdict " + verdict.tone}>{verdict.text}</div>
      <dl className="kv">
        <KV k="Resolution" v={d.width ? `${d.width}×${d.height}` : "—"} />
        <KV k="Format" v={d.format?.split(" [")[0] ?? "—"} />
        <KV k="Target fps" v={`${target}`} />
        {sends != null && <KV k="Camera sends" v={`${f1(sends)} fps`} tone={sends < nominal * 0.8 ? "warn" : undefined} />}
        {d.device_fps != null && <KV k="Capture fps" v={`${f1(d.device_fps)}`} />}
        <KV k="Render fps" v={f1(d.fps)} tone={d.fps < target * 0.8 ? "warn" : undefined} />
        <KV k="Dropped (stale)" v={`${d.dropped}`} />
        <KV k="Latency" v={`${Math.round(lat)} ms`} tone={lat > 250 ? "warn" : undefined} />
        {d.capture_latency_ms != null && <KV k="Capture latency" v={`${f1(d.capture_latency_ms)} ms`} />}
        {d.decode_ms != null && <KV k="Decode / convert" v={`${f1(d.decode_ms)} / ${f1(d.convert_ms)} ms`} />}
        {d.max_gap_ms != null && <KV k="Longest gap (1 s)" v={`${Math.round(d.max_gap_ms)} ms`} tone={d.max_gap_ms > 200 ? "warn" : undefined} />}
        <KV k="Buffer" v="1 frame (newest wins)" />
        {d.device_packet_fps != null && <KV k="Errors decode / read" v={`${d.decode_errors} / ${d.capture_errors}`} tone={d.decode_errors + d.capture_errors > 0 ? "warn" : undefined} />}
      </dl>
      {d.format?.includes(" — ") && <p className="note">{d.format.split(" — ")[1]}</p>}
    </>
  );
}

function SourceTab({ source, sources }: { source: SourceView; sources: SourceView[] }) {
  const diag = useProduction((s) => s.diag);
  const vs = useProduction((s) => s.state?.video_settings);
  const remove = useProduction((s) => s.sourceRemove);
  const reorder = useProduction((s) => s.sourceReorder);
  const duplicate = useProduction((s) => s.sourceDuplicate);
  const refresh = useProduction((s) => s.refresh);
  const select = useProduction((s) => s.selectSource);
  const mediaPaused = useProduction((s) => s.mediaPaused[source.id] ?? false);
  const mediaSetPaused = useProduction((s) => s.mediaSetPaused);
  const [confirm, setConfirm] = useState(false);
  const [cam, setCam] = useState<CameraSourceInfo | null>(null);
  const [devices, setDevices] = useState<string[]>([]);
  const [mics, setMics] = useState<string[]>([]);
  const [device, setDevice] = useState("");
  const [size, setSize] = useState("1280x720");
  const [fps, setFps] = useState(30);
  const [audio, setAudio] = useState("");
  const [applying, setApplying] = useState(false);
  const [msg, setMsg] = useState<{ tone: "bad" | "ok"; text: string } | null>(null);
  const d = diag?.sources.find((x) => x.id === source.id);
  const target = vs?.output_fps ?? 30;

  useEffect(() => {
    if (source.kind !== "camera") return;
    let off = false;
    void (async () => {
      try {
        const [cams, devs, adevs, ad] = await Promise.all([
          api.cameraSources(),
          api.listCameras().catch(() => [] as string[]),
          api.listAudioDevices().catch(() => [] as string[]),
          api.sourceAudioDevice(source.id).catch(() => null),
        ]);
        if (off) return;
        const c = cams.find((x) => x.id === source.id) ?? null;
        setCam(c);
        setDevices(devs);
        setMics(adevs);
        if (c) {
          setDevice(c.device ?? "");
          setSize(`${c.width}x${c.height}`);
          setFps(c.fps === 60 ? 60 : 30);
        }
        setAudio(ad ?? "");
      } catch {
        /* outside the app */
      }
    })();
    return () => {
      off = true;
    };
  }, [source.id, source.kind]);

  const ordered = [...sources].sort((a, b) => b.order - a.order).map((s) => s.id); // top first
  const idx = ordered.indexOf(source.id);
  const move = (dir: -1 | 1) => {
    const j = idx + dir;
    if (idx < 0 || j < 0 || j >= ordered.length) return;
    const o = [...ordered];
    [o[idx], o[j]] = [o[j], o[idx]];
    void reorder([...o].reverse());
  };

  const apply = async () => {
    const [w, h] = size.split("x").map(Number);
    setApplying(true);
    setMsg(null);
    try {
      const got = await api.cameraReconfigure({ source_id: source.id, device: device || null, width: w, height: h, fps, audio_device: audio || null });
      setMsg({ tone: "ok", text: `Camera running: ${got.split(" — ")[0]}` });
      await refresh();
    } catch (e) {
      setMsg({ tone: "bad", text: errText(e) });
    } finally {
      setApplying(false);
    }
  };

  return (
    <div className="ptab-body">
      <div className="kind-line">
        {KIND_NAME[source.kind] ?? source.kind}
        {source.detail && source.kind !== "text" ? <span title={source.detail}> · {source.detail.split(/[\\/]/).pop()}</span> : null}
      </div>
      {source.kind === "camera" && (
        <>
          <div className="form-grid">
            <label className="f wide">
              <span>Device</span>
              <select value={device} onChange={(e) => setDevice(e.target.value)}>
                <option value="">First available</option>
                {devices.map((x) => (
                  <option key={x} value={x}>
                    {x}
                  </option>
                ))}
                {device && !devices.includes(device) && <option value={device}>{device}</option>}
              </select>
            </label>
            <label className="f">
              <span>Resolution</span>
              <select value={size} onChange={(e) => setSize(e.target.value)}>
                <option value="1280x720">1280×720</option>
                <option value="1920x1080">1920×1080</option>
              </select>
            </label>
            <label className="f">
              <span>Frame rate</span>
              <select value={fps} onChange={(e) => setFps(Number(e.target.value))}>
                <option value={30}>30 fps</option>
                <option value={60}>60 fps</option>
              </select>
            </label>
            <label className="f wide">
              <span>Audio device</span>
              <select value={audio} onChange={(e) => setAudio(e.target.value)}>
                <option value="">No audio</option>
                {mics.map((x) => (
                  <option key={x} value={x}>
                    {x}
                  </option>
                ))}
                {audio && !mics.includes(audio) && <option value={audio}>{audio}</option>}
              </select>
            </label>
          </div>
          <div className="form-actions left">
            <button type="button" className="btn small" disabled={applying} onClick={() => void apply()}>
              {applying ? "Reopening camera…" : "Apply"}
            </button>
            {cam?.negotiated && <span className="note">Delivering: {cam.negotiated.split(" — ")[0]}</span>}
          </div>
          {msg && <div className={"form-msg " + msg.tone}>{msg.text}</div>}
          <p className="note">Exposure, focus, white balance and gain: not available in StreamFlow yet. Set them in the camera's own app; StreamFlow keeps them.</p>
          <h4 className="sub">Camera diagnostics</h4>
          <CameraHealth d={d} target={target} />
        </>
      )}
      {(source.kind === "screen" || source.kind === "media") && <CameraHealth d={d} target={target} />}
      {source.kind === "media" && (
        <div className="form-actions left">
          <button type="button" className="btn small secondary" onClick={() => void mediaSetPaused(source.id, !mediaPaused)}>
            {mediaPaused ? "Play" : "Pause"}
          </button>
        </div>
      )}
      {source.kind === "text" && <p className="note">“{source.detail}”</p>}
      <div className="form-actions left">
        <button type="button" className="btn small secondary" disabled={idx <= 0} onClick={() => move(-1)} title="Bring forward (drawn above)">
          Bring forward
        </button>
        <button type="button" className="btn small secondary" disabled={idx < 0 || idx >= ordered.length - 1} onClick={() => move(1)} title="Send backward">
          Send backward
        </button>
        {source.kind !== "audioinput" && (
          <button type="button" className="btn small secondary" onClick={() => void duplicate(source.id)}>
            Duplicate
          </button>
        )}
        <button
          type="button"
          className={"btn small " + (confirm ? "danger" : "secondary")}
          onClick={() => {
            if (!confirm) return setConfirm(true);
            void remove(source.id).then(() => select(null));
          }}
          onBlur={() => setConfirm(false)}
        >
          {confirm ? "Click again to remove" : "Remove"}
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Transform + layouts
// ---------------------------------------------------------------------------

const LAYOUTS: { id: SceneLayout; label: string; boxes: [number, number, number, number][] }[] = [
  { id: "full", label: "Full frame", boxes: [[0, 0, 1, 1]] },
  { id: "side_by_side", label: "Side by side", boxes: [[0, 0, 0.5, 1], [0.5, 0, 0.5, 1]] },
  { id: "side_by_side_fit", label: "Side by side, whole picture", boxes: [[0, 0.25, 0.5, 0.5], [0.5, 0.25, 0.5, 0.5]] },
  { id: "split_v", label: "Top and bottom", boxes: [[0, 0, 1, 0.5], [0, 0.5, 1, 0.5]] },
  { id: "pip_br", label: "Picture in picture, bottom right", boxes: [[0, 0, 1, 1], [0.67, 0.67, 0.3, 0.3]] },
  { id: "pip_bl", label: "Picture in picture, bottom left", boxes: [[0, 0, 1, 1], [0.03, 0.67, 0.3, 0.3]] },
  { id: "pip_tr", label: "Picture in picture, top right", boxes: [[0, 0, 1, 1], [0.67, 0.03, 0.3, 0.3]] },
  { id: "grid", label: "Grid of four", boxes: [[0, 0, 0.5, 0.5], [0.5, 0, 0.5, 0.5], [0, 0.5, 0.5, 0.5], [0.5, 0.5, 0.5, 0.5]] },
];

function LayoutPresets() {
  const apply = useProduction((s) => s.sceneApplyLayout);
  return (
    <div className="layouts">
      <span className="layouts-label">Layout for the visible videos (bottom layer = main)</span>
      <div className="layout-grid">
        {LAYOUTS.map((l) => (
          <button key={l.id} type="button" className="layout-btn" title={l.label} aria-label={l.label} onClick={() => void apply(l.id)}>
            <svg viewBox="0 0 32 18" width="32" height="18" aria-hidden="true">
              <rect x="0.5" y="0.5" width="31" height="17" className="lo-frame" />
              {l.boxes.map((b, i) => (
                <rect key={i} x={b[0] * 32 + 1} y={b[1] * 18 + 1} width={b[2] * 32 - 2} height={b[3] * 18 - 2} className={i === 0 ? "lo-main" : "lo-sub"} />
              ))}
            </svg>
          </button>
        ))}
      </div>
    </div>
  );
}

function Num({ label, value, onChange, min, max, step = 1, unit = "%" , scale = 100, disabled }: { label: string; value: number; onChange: (v: number) => void; min: number; max: number; step?: number; unit?: string; scale?: number; disabled?: boolean }) {
  return (
    <label className="num">
      <span>{label}</span>
      <input
        type="number"
        step={step}
        min={min}
        max={max}
        disabled={disabled}
        value={Math.round(value * scale * 10) / 10}
        onChange={(e) => {
          const v = Number(e.target.value);
          if (Number.isFinite(v)) onChange(Math.min(max, Math.max(min, v)) / scale);
        }}
      />
      <em>{unit}</em>
    </label>
  );
}

function TransformTab({ source }: { source: SourceView }) {
  const setT = useProduction((s) => s.sourceSetTransform);
  const setLocked = useProduction((s) => s.sourceSetLocked);
  const t = source.transform ?? FULL_T;
  const lock = source.locked;
  const upd = (p: Partial<SourceTransform>) => void setT(source.id, { ...t, ...p });
  return (
    <div className="ptab-body">
      {lock && (
        <div className="form-msg">
          Position is locked.{" "}
          <button type="button" className="link" onClick={() => void setLocked(source.id, false)}>
            Unlock
          </button>
        </div>
      )}
      <div className="num-grid">
        <Num label="X" value={t.x} min={-100} max={100} onChange={(x) => upd({ x })} disabled={lock} />
        <Num label="Y" value={t.y} min={-100} max={100} onChange={(y) => upd({ y })} disabled={lock} />
        <Num label="Width" value={t.w} min={1} max={400} onChange={(w) => upd({ w })} disabled={lock} />
        <Num label="Height" value={t.h} min={1} max={400} onChange={(h) => upd({ h })} disabled={lock} />
        <Num label="Rotation" value={t.rotation} min={-180} max={180} unit="°" scale={1} onChange={(rotation) => upd({ rotation })} disabled={lock} />
        <Num label="Opacity" value={t.opacity} min={0} max={100} onChange={(opacity) => upd({ opacity })} disabled={lock} />
        <label className="num">
          <span>Fit</span>
          <select value={t.fit} disabled={lock} onChange={(e) => upd({ fit: e.target.value as FitMode })}>
            <option value="fill">Fill (crop)</option>
            <option value="fit">Fit (bars)</option>
            <option value="stretch">Stretch</option>
          </select>
        </label>
      </div>
      <h4 className="sub">Crop (percent of the source picture)</h4>
      <div className="num-grid">
        <Num label="Left" value={t.crop_left} min={0} max={98} onChange={(crop_left) => upd({ crop_left })} disabled={lock} />
        <Num label="Right" value={t.crop_right} min={0} max={98} onChange={(crop_right) => upd({ crop_right })} disabled={lock} />
        <Num label="Top" value={t.crop_top} min={0} max={98} onChange={(crop_top) => upd({ crop_top })} disabled={lock} />
        <Num label="Bottom" value={t.crop_bottom} min={0} max={98} onChange={(crop_bottom) => upd({ crop_bottom })} disabled={lock} />
      </div>
      <div className="form-actions left">
        {[0.9, 1.1].map((k) => (
          <button
            key={k}
            type="button"
            className="btn small secondary"
            disabled={lock}
            title="Scale around the centre"
            onClick={() => upd({ w: t.w * k, h: t.h * k, x: t.x - (t.w * k - t.w) / 2, y: t.y - (t.h * k - t.h) / 2 })}
          >
            {k < 1 ? "Smaller" : "Larger"}
          </button>
        ))}
        <button type="button" className="btn small secondary" disabled={lock || !source.transform} onClick={() => void setT(source.id, null)}>
          Reset to full frame
        </button>
        <button type="button" className="btn small secondary" onClick={() => void setLocked(source.id, !lock)}>
          {lock ? "Unlock position" : "Lock position"}
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Color (the same tools as the Color page, targeting the selected source)
// ---------------------------------------------------------------------------

const CTABS: { id: TabId; label: string }[] = [
  { id: "basic", label: "Basic" },
  { id: "lut", label: "LUT" },
  { id: "wheels", label: "Wheels" },
  { id: "curves", label: "Curves" },
  { id: "creative", label: "Creative" },
  { id: "effects", label: "Glow & skin" },
  { id: "filters", label: "Effects" },
  { id: "presets", label: "Presets" },
];

function ColorTab({ source, onOpen }: { source: SourceView; onOpen: () => void }) {
  const init = useStudio((s) => s.init);
  const ready = useStudio((s) => s.ready);
  const inputId = useStudio((s) => s.inputId);
  const setInputId = useStudio((s) => s.setInputId);
  const active = useStudio((s) => s.activeTab);
  const setTab = useStudio((s) => s.setTab);
  useEffect(() => {
    void (async () => {
      if (!useStudio.getState().ready) await init();
      if (source.input_id && useStudio.getState().inputId !== source.input_id) await setInputId(source.input_id, source.name);
    })();
  }, [source.input_id, source.name, init, setInputId]);
  if (!ready || inputId !== source.input_id) return <div className="panel-empty">Loading grade…</div>;
  return (
    <div className="ptab-body color-embed">
      <div className="ctabs" role="tablist">
        {CTABS.map((t) => (
          <button key={t.id} type="button" role="tab" aria-selected={active === t.id} className={active === t.id ? "on" : ""} onClick={() => setTab(t.id)}>
            {t.label}
          </button>
        ))}
        <button type="button" className="link right" onClick={onOpen} title="Full grading page with scopes and before/after">
          Open with scopes
        </button>
      </div>
      <div className="cbody">
        {active === "basic" && <BasicTab />}
        {active === "creative" && <CreativeTab />}
        {active === "curves" && <CurvesTab />}
        {active === "wheels" && <WheelsTab />}
        {active === "lut" && <LutTab />}
        {active === "effects" && <EffectsTab />}
        {active === "filters" && <FiltersTab />}
        {active === "presets" && <PresetsTab />}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Audio processing (global)
// ---------------------------------------------------------------------------

function Range({ label, value, min, max, step, fmt, onChange, disabled, neutral }: { label: string; value: number; min: number; max: number; step: number; fmt: (v: number) => string; onChange: (v: number) => void; disabled?: boolean; neutral?: number }) {
  return (
    <label className={"hrange" + (disabled ? " disabled" : "")} onDoubleClick={() => neutral !== undefined && !disabled && onChange(neutral)}>
      <span>{label}</span>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        disabled={disabled}
        onChange={(e) => onChange(parseFloat(e.target.value))}
        style={{ ["--pct" as string]: `${((value - min) / (max - min)) * 100}%` }}
      />
      <output>{fmt(value)}</output>
    </label>
  );
}

function Check({ label, checked, onChange, disabled, hint }: { label: string; checked: boolean; onChange: (v: boolean) => void; disabled?: boolean; hint?: string }) {
  return (
    <label className={"check" + (disabled ? " disabled" : "")} title={hint}>
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <span>{label}</span>
      {hint && <small>{hint}</small>}
    </label>
  );
}

function AudioProcessing() {
  const mixer = useProduction((s) => s.state?.mixer);
  const sources = useProduction((s) => s.state?.scenes.flatMap((sc) => sc.sources) ?? []);
  const setEnh = useProduction((s) => s.setEnhancerSettings);
  const setAvSync = useProduction((s) => s.setAvSync);
  if (!mixer) return null;
  const enh = mixer.enhancer;
  const e = (p: Partial<EnhancerSettings>) => void setEnh({ ...enh, ...p });
  const av = mixer.av_sync;
  const db = (v: number) => `${v > 0 ? "+" : ""}${v.toFixed(1)} dB`;
  return (
    <div className="ptab-body two-col">
      <div>
        <h4 className="sub">Voice enhancer (microphones and camera audio)</h4>
        <Check label="Enhance voices" checked={enh.enabled} onChange={(v) => e({ enabled: v })} />
        <Check label="AI noise removal" hint="Fans, hum, keyboard, room noise" checked={enh.ai_denoise} disabled={!enh.enabled} onChange={(v) => e({ ai_denoise: v })} />
        <Range label="Strength" value={enh.ai_strength} min={0} max={1} step={0.05} neutral={1} fmt={(v) => `${Math.round(v * 100)}%`} disabled={!enh.enabled || !enh.ai_denoise} onChange={(v) => e({ ai_strength: v })} />
        <Check label="Noise gate" checked={enh.gate_enabled} disabled={!enh.enabled} onChange={(v) => e({ gate_enabled: v })} />
        <Range label="Threshold" value={enh.gate_threshold_db} min={-60} max={0} step={1} neutral={-40} fmt={(v) => `${Math.round(v)} dB`} disabled={!enh.enabled || !enh.gate_enabled} onChange={(v) => e({ gate_threshold_db: v })} />
        <Range label="EQ low" value={enh.eq_low_db} min={-12} max={12} step={0.5} neutral={0} fmt={db} disabled={!enh.enabled} onChange={(v) => e({ eq_low_db: v })} />
        <Range label="EQ mid" value={enh.eq_mid_db} min={-12} max={12} step={0.5} neutral={0} fmt={db} disabled={!enh.enabled} onChange={(v) => e({ eq_mid_db: v })} />
        <Range label="EQ high" value={enh.eq_high_db} min={-12} max={12} step={0.5} neutral={0} fmt={db} disabled={!enh.enabled} onChange={(v) => e({ eq_high_db: v })} />
        <Check label="Compressor" hint="Gentle levelling" checked={enh.compressor_enabled} disabled={!enh.enabled} onChange={(v) => e({ compressor_enabled: v })} />
      </div>
      <div>
        <h4 className="sub">A/V sync</h4>
        <Check label="Automatic sync" hint="Delay each mic to match its camera's measured picture delay" checked={av.auto} onChange={(v) => setAvSync(v, av.offset_ms)} />
        <Range label="Offset" value={av.offset_ms} min={-100} max={100} step={1} neutral={0} fmt={(v) => `${v > 0 ? "+" : ""}${Math.round(v)} ms`} onChange={(v) => setAvSync(av.auto, Math.round(v))} />
        <dl className="kv">
          {Object.entries(mixer.voice).map(([id, v]) => (
            <KV
              key={id}
              k={sources.find((s) => s.id === id)?.name ?? "Microphone"}
              v={v.audio_age_ms != null ? `${Math.round(v.audio_age_ms)} ms${v.target_age_ms != null ? ` → ${Math.round(v.target_age_ms)} ms` : ""}` : v.device}
              tone={v.device.startsWith("error") ? "bad" : undefined}
            />
          ))}
        </dl>
      </div>
    </div>
  );
}
