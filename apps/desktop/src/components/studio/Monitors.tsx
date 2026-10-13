import { useEffect, useRef, useState } from "react";
import { useProduction } from "../../lib/productionStore";
import type { SourceTransform, SourceView, TransitionKindName } from "../../lib/types";
import MeterCanvas from "./MeterCanvas";

const FULL: SourceTransform = { x: 0, y: 0, w: 1, h: 1, crop_left: 0, crop_top: 0, crop_right: 0, crop_bottom: 0, fit: "fill", opacity: 1, rotation: 0 };
const VIDEO_KINDS = new Set(["camera", "screen", "media", "image", "text", "color", "testpattern"]);

const fmtTime = (s: number) => {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = Math.floor(s % 60);
  return `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
};

/** Polls the preview-scene picture (~10 fps) while it differs from program. */
function usePreviewPicture(active: boolean) {
  const poll = useProduction((s) => s.pollPreviewMonitor);
  const pv = useProduction((s) => s.state?.preview_scene_id);
  useEffect(() => {
    if (!active) return;
    let stop = false;
    let t: ReturnType<typeof setTimeout> | null = null;
    const loop = async () => {
      if (stop) return;
      if (!document.hidden) await poll();
      if (!stop) t = setTimeout(() => void loop(), 100);
    };
    void loop();
    return () => {
      stop = true;
      if (t) clearTimeout(t);
    };
  }, [active, pv, poll]);
}

type Mode = "move" | "n" | "s" | "e" | "w" | "ne" | "nw" | "se" | "sw";

/**
 * Placement overlay on the PREVIEW monitor: click a layer to select it,
 * drag to move, drag a handle to resize (Shift keeps the aspect). Snaps to
 * the canvas edges and centre. Locked layers can be selected, not moved.
 */
function PlacementOverlay({ sources }: { sources: SourceView[] }) {
  const selected = useProduction((s) => s.selectedSourceId);
  const select = useProduction((s) => s.selectSource);
  const setT = useProduction((s) => s.sourceSetTransform);
  const ref = useRef<HTMLDivElement>(null);
  const drag = useRef<{ mode: Mode; x: number; y: number; t0: SourceTransform; id: string } | null>(null);
  const pending = useRef<{ id: string; t: SourceTransform } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [live, setLive] = useState<{ id: string; t: SourceTransform } | null>(null);

  const visible = sources.filter((s) => s.visible && VIDEO_KINDS.has(s.kind)).sort((a, b) => b.order - a.order);
  const tOf = (s: SourceView) => (live && live.id === s.id ? live.t : s.transform ?? FULL);

  // The box follows the pointer locally; the engine gets ≤ 20 updates/s.
  const flush = () => {
    timer.current = null;
    const p = pending.current;
    if (p) void setT(p.id, p.t);
  };

  const onDown = (e: React.PointerEvent, s: SourceView, mode: Mode) => {
    e.stopPropagation();
    select(s.id);
    if (s.locked || e.button !== 0) return;
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    drag.current = { mode, x: e.clientX, y: e.clientY, t0: { ...tOf(s) }, id: s.id };
  };

  const onMove = (e: React.PointerEvent) => {
    const d = drag.current;
    const el = ref.current;
    if (!d || !el) return;
    const r = el.getBoundingClientRect();
    const dx = (e.clientX - d.x) / r.width;
    const dy = (e.clientY - d.y) / r.height;
    const t = { ...d.t0 };
    const snap = (v: number, targets: number[]) => {
      for (const s of targets) if (Math.abs(v - s) < 0.012) return s;
      return v;
    };
    if (d.mode === "move") {
      t.x = d.t0.x + dx;
      t.y = d.t0.y + dy;
      // Snap left/centre/right and top/middle/bottom edges.
      const sx = snap(t.x, [0, 0.5 - t.w / 2, 1 - t.w]);
      const sy = snap(t.y, [0, 0.5 - t.h / 2, 1 - t.h]);
      t.x = sx;
      t.y = sy;
    } else {
      if (d.mode.includes("e")) t.w = Math.max(0.02, d.t0.w + dx);
      if (d.mode.includes("s")) t.h = Math.max(0.02, d.t0.h + dy);
      if (d.mode.includes("w")) {
        t.w = Math.max(0.02, d.t0.w - dx);
        t.x = d.t0.x + (d.t0.w - t.w);
      }
      if (d.mode.includes("n")) {
        t.h = Math.max(0.02, d.t0.h - dy);
        t.y = d.t0.y + (d.t0.h - t.h);
      }
      if (e.shiftKey && d.t0.w > 0 && d.t0.h > 0) {
        const k = d.mode === "n" || d.mode === "s" ? t.h / d.t0.h : t.w / d.t0.w;
        t.w = d.t0.w * k;
        t.h = d.t0.h * k;
        if (d.mode.includes("w")) t.x = d.t0.x + d.t0.w - t.w;
        if (d.mode.includes("n")) t.y = d.t0.y + d.t0.h - t.h;
      }
    }
    setLive({ id: d.id, t });
    pending.current = { id: d.id, t };
    if (timer.current == null) timer.current = setTimeout(flush, 50);
  };

  const onUp = () => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    if (drag.current && pending.current) void setT(pending.current.id, pending.current.t);
    drag.current = null;
    pending.current = null;
    setLive(null);
  };

  // Click on empty canvas: pick the topmost layer under the pointer.
  const onBackground = (e: React.PointerEvent) => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const px = (e.clientX - r.left) / r.width;
    const py = (e.clientY - r.top) / r.height;
    const hit = visible.find((s) => {
      const t = tOf(s);
      return px >= t.x && px <= t.x + t.w && py >= t.y && py <= t.y + t.h;
    });
    select(hit ? hit.id : null);
  };

  return (
    <div ref={ref} className="placement" onPointerDown={onBackground} onPointerMove={onMove} onPointerUp={onUp} onPointerCancel={onUp}>
      {visible.map((s) => {
        const t = tOf(s);
        const isSel = s.id === selected;
        const style: React.CSSProperties = {
          left: `${t.x * 100}%`,
          top: `${t.y * 100}%`,
          width: `${t.w * 100}%`,
          height: `${t.h * 100}%`,
          transform: t.rotation ? `rotate(${t.rotation}deg)` : undefined,
        };
        return (
          <div
            key={s.id}
            className={"pl-box" + (isSel ? " sel" : "") + (s.locked ? " locked" : "")}
            style={style}
            onPointerDown={(e) => onDown(e, s, "move")}
            title={s.locked ? `${s.name} (locked)` : s.name}
          >
            {isSel && <span className="pl-name">{s.name}{s.locked ? " · locked" : ""}</span>}
            {isSel &&
              !s.locked &&
              (["nw", "n", "ne", "e", "se", "s", "sw", "w"] as Mode[]).map((m) => (
                <span key={m} className={"pl-h " + m} onPointerDown={(e) => onDown(e, s, m)} />
              ))}
          </div>
        );
      })}
    </div>
  );
}

/** PREVIEW | switch | PROGRAM, the heart of the workspace. */
export default function Monitors() {
  const state = useProduction((s) => s.state);
  const programUrl = useProduction((s) => s.programUrl);
  const previewUrl = useProduction((s) => s.previewUrl);
  const diag = useProduction((s) => s.diag);
  const cut = useProduction((s) => s.cut);
  const auto = useProduction((s) => s.auto);
  const progress = useProduction((s) => s.transitionProgress);
  const kind = useProduction((s) => s.transitionKind);
  const ms = useProduction((s) => s.transitionMs);
  const setTransition = useProduction((s) => s.setTransition);

  const pv = state?.preview_scene_id ?? null;
  const pg = state?.program_scene_id ?? null;
  const split = !!pv && pv !== pg;
  usePreviewPicture(split);
  const pvScene = state?.scenes.find((s) => s.id === pv);
  const pgScene = state?.scenes.find((s) => s.id === pg);
  const vs = state?.video_settings;
  const fmt = vs ? `${vs.output_width}×${vs.output_height} · ${vs.output_fps} fps` : "";
  const stream = state?.stream.state ?? "idle";
  const live = stream === "live";
  const rec = state?.record.state ?? "idle";
  const busy = progress != null;
  const pvUrl = split ? previewUrl : programUrl;
  const dropped = diag ? diag.lagged_ticks + diag.skipped_render + diag.dropped_network : null;

  return (
    <div className="monitors">
      <section className="mon pvw" aria-label="Preview">
        <header className="tally pvw">
          <b>Preview</b>
          <span className="tally-scene">{pvScene?.name ?? "—"}</span>
          {!split && <span className="tally-note">same as program — edits are live</span>}
          <span className="tally-fmt">{fmt}</span>
        </header>
        <div className="mon-row">
          <div className="mon-stage">
            <div className="mon-frame">
              {pvUrl ? <img src={pvUrl} alt="" draggable={false} /> : <div className="mon-empty">No picture yet. Add a source below.</div>}
              {pvScene && <PlacementOverlay sources={pvScene.sources} />}
            </div>
          </div>
        </div>
        <footer className="mon-info">
          <span>{pvScene ? `${pvScene.sources.filter((s) => s.visible && s.kind !== "audioinput").length} visible layers` : ""}</span>
          <span>Drag to move · handles resize · Shift keeps shape</span>
        </footer>
      </section>

      <div className="switcher" role="group" aria-label="Transition">
        <button type="button" className="take cut" disabled={!split || busy} onClick={() => void cut()} title="Cut to preview (Enter)">
          Cut
        </button>
        <button type="button" className="take auto" disabled={!split || busy} onClick={() => void auto()} title="Auto transition (Space)">
          Auto
          {busy && <span className="take-progress" style={{ height: `${Math.round((progress ?? 0) * 100)}%` }} />}
        </button>
        <select aria-label="Transition type" value={kind} disabled={busy} onChange={(e) => setTransition(e.target.value as TransitionKindName, ms)}>
          <option value="mix">Mix</option>
          <option value="fade">Fade to black</option>
          <option value="wipe">Wipe</option>
        </select>
        <label className="ms-field" title="Duration">
          <input type="number" min={100} max={5000} step={100} value={ms} disabled={busy} onChange={(e) => setTransition(kind, Math.min(5000, Math.max(100, Number(e.target.value) || 500)))} />
          ms
        </label>
        {!split && <p className="switch-hint">Select another scene for preview to switch.</p>}
      </div>

      <section className="mon pgm" aria-label="Program">
        <header className="tally pgm">
          <b>Program</b>
          <span className="tally-scene">{pgScene?.name ?? "—"}</span>
          {live && <span className="badge live">Live {diag ? fmtTime(diag.stream_uptime_s) : ""}</span>}
          {stream === "connecting" && <span className="badge warn">Connecting</span>}
          {stream === "reconnecting" && <span className="badge warn">Reconnecting</span>}
          {rec === "recording" && <span className="badge rec">Rec {fmtTime(state?.record.elapsed_secs ?? 0)}</span>}
          {rec === "paused" && <span className="badge">Rec paused</span>}
          <span className="tally-fmt">{fmt}</span>
        </header>
        <div className="mon-row">
          <div className="mon-stage">
            <div className={"mon-frame" + (live ? " onair" : "")}>{programUrl ? <img src={programUrl} alt="" draggable={false} /> : <div className="mon-empty">No picture</div>}</div>
          </div>
          <MeterCanvas channel="master" title="Program audio" />
        </div>
        <footer className="mon-info">
          <span>{diag ? `${Math.round(diag.render_fps)}/${diag.target_fps} fps` : "—"}</span>
          <span className={dropped ? "warn" : ""}>{dropped == null ? "" : `${dropped} dropped`}</span>
          <span>{diag?.latency.app_total_ms != null ? `${Math.round(diag.latency.app_total_ms)} ms latency` : "latency —"}</span>
          <span>{live && diag ? `${Math.round(diag.stream_kbps)} kbps` : stream === "error" ? "stream error" : "not streaming"}</span>
        </footer>
      </section>
    </div>
  );
}
