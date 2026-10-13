import { useEffect, useState } from "react";
import { useProduction } from "../../lib/productionStore";
import type { TransitionKindName } from "../../lib/types";
import Monitor, { PreviewMonitor } from "../Monitor";

/**
 * STUDIO (Preview → Program). Edits in Input go to the PREVIEW scene; CUT or
 * AUTO puts it on air. PROGRAM is exactly the frame the encoder receives.
 * With a single scene, preview and program are the same scene (live edits).
 *
 * Keys: Enter = CUT, Space = AUTO, 1–9 = select scene for preview.
 */
export default function PreviewView() {
  const state = useProduction((s) => s.state);
  const setPreviewScene = useProduction((s) => s.setPreviewScene);
  const cut = useProduction((s) => s.cut);
  const auto = useProduction((s) => s.auto);
  const progress = useProduction((s) => s.transitionProgress);
  const kind = useProduction((s) => s.transitionKind);
  const ms = useProduction((s) => s.transitionMs);
  const setTransition = useProduction((s) => s.setTransition);
  const sceneCreate = useProduction((s) => s.sceneCreate);
  const sceneDuplicate = useProduction((s) => s.sceneDuplicate);
  const sceneRename = useProduction((s) => s.sceneRename);
  const sceneDelete = useProduction((s) => s.sceneDelete);
  const sourceSetVisible = useProduction((s) => s.sourceSetVisible);
  const [renaming, setRenaming] = useState<string | null>(null);

  const scenes = state?.scenes ?? [];
  const pv = state?.preview_scene_id ?? null;
  const pg = state?.program_scene_id ?? null;
  const studio = !!pv && pv !== pg;
  const pvScene = scenes.find((s) => s.id === pv);
  const pgScene = scenes.find((s) => s.id === pg);
  const busy = progress != null;

  // Broadcast keys (ignored while typing in a field).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "SELECT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      if (e.key === "Enter") {
        e.preventDefault();
        void cut();
      } else if (e.key === " ") {
        e.preventDefault();
        void auto();
      } else if (/^[1-9]$/.test(e.key)) {
        const sc = scenes[Number(e.key) - 1];
        if (sc) void setPreviewScene(sc.id);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cut, auto, scenes, setPreviewScene]);

  const visual = [...(pvScene?.sources ?? [])].filter((s) => s.kind !== "audioinput").sort((a, b) => b.order - a.order);

  return (
    <div className="workspace single studio">
      <div className={"studio-monitors" + (studio ? " two" : "")}>
        {studio && (
          <div className="studio-mon">
            <div className="studio-label pv">PREVIEW · {pvScene?.name}</div>
            <PreviewMonitor />
          </div>
        )}
        <div className="studio-mon">
          <div className="studio-label pg">PROGRAM · {pgScene?.name}{!studio && scenes.length > 1 ? " (also in preview)" : ""}</div>
          <Monitor />
        </div>
      </div>

      <div className="studio-bar">
        <div className="scene-strip" role="listbox" aria-label="Scenes">
          {scenes.map((sc, i) => (
            <div
              key={sc.id}
              role="option"
              aria-selected={sc.id === pv}
              className={"scene-tile" + (sc.id === pv ? " pv" : "") + (sc.id === pg ? " pg" : "")}
              onClick={() => void setPreviewScene(sc.id)}
              onDoubleClick={() => setRenaming(sc.id)}
              title={`${sc.name} — click: preview, double-click: rename (key ${i + 1})`}
            >
              {renaming === sc.id ? (
                <input
                  autoFocus
                  defaultValue={sc.name}
                  onClick={(e) => e.stopPropagation()}
                  onBlur={(e) => {
                    setRenaming(null);
                    if (e.target.value.trim() && e.target.value !== sc.name) void sceneRename(sc.id, e.target.value.trim());
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                    if (e.key === "Escape") setRenaming(null);
                  }}
                />
              ) : (
                <>
                  <span className="scene-key">{i + 1}</span>
                  <span className="scene-name">{sc.name}</span>
                  <span className="scene-count">{sc.sources.filter((s) => s.kind !== "audioinput").length}</span>
                </>
              )}
            </div>
          ))}
          <div className="scene-actions">
            <button type="button" className="btn small secondary" onClick={() => void sceneCreate()}>New</button>
            {pv && <button type="button" className="btn small secondary" onClick={() => void sceneDuplicate(pv)}>Duplicate</button>}
            {pv && scenes.length > 1 && pv !== pg && (
              <button type="button" className="btn small secondary danger" onClick={() => void sceneDelete(pv)}>Delete</button>
            )}
          </div>
        </div>

        <div className="transition-panel">
          <select aria-label="Transition" value={kind} onChange={(e) => setTransition(e.target.value as TransitionKindName, ms)} disabled={busy}>
            <option value="mix">Mix</option>
            <option value="fade">Fade (black)</option>
            <option value="wipe">Wipe</option>
          </select>
          <label className="ms-field">
            <input type="number" min={100} max={5000} step={100} value={ms} disabled={busy} onChange={(e) => setTransition(kind, Math.min(5000, Math.max(100, Number(e.target.value) || 500)))} />
            <span>ms</span>
          </label>
          <button type="button" className="take cut" disabled={!studio || busy} onClick={() => void cut()} title="Cut (Enter)">CUT</button>
          <button type="button" className="take auto" disabled={!studio || busy} onClick={() => void auto()} title="Auto transition (Space)">
            AUTO
            {busy && <span className="take-progress" style={{ width: `${Math.round((progress ?? 0) * 100)}%` }} />}
          </button>
        </div>
      </div>

      {visual.length > 0 && (
        <div className="chip-row" role="group" aria-label={`Sources in ${pvScene?.name ?? "scene"}`}>
          {visual.map((s) => (
            <button
              key={s.id}
              type="button"
              className={"toggle-chip" + (s.visible ? " on" : "")}
              aria-pressed={s.visible}
              title={`${s.visible ? "Hide" : "Show"} ${s.name}`}
              onClick={() => void sourceSetVisible(s.id, !s.visible)}
            >
              <span className="toggle-chip-text">{s.name}</span>
            </button>
          ))}
        </div>
      )}
      {!studio && scenes.length <= 1 && (
        <div className="hint-line">One scene: edits are live. Create a scene (New / Duplicate) to prepare the next shot in Preview, then CUT or AUTO.</div>
      )}
    </div>
  );
}
