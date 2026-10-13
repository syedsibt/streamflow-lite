import { useState } from "react";
import { useProduction } from "../../lib/productionStore";

/**
 * SCENES: click = load into preview; double-click = rename. The program
 * scene carries the red tally, the preview scene the green one. Keys 1–9
 * select the scene in that position.
 */
export default function ScenesPanel() {
  const state = useProduction((s) => s.state);
  const setPreview = useProduction((s) => s.setPreviewScene);
  const create = useProduction((s) => s.sceneCreate);
  const duplicate = useProduction((s) => s.sceneDuplicate);
  const rename = useProduction((s) => s.sceneRename);
  const remove = useProduction((s) => s.sceneDelete);
  const move = useProduction((s) => s.sceneMove);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [confirm, setConfirm] = useState(false);
  const scenes = state?.scenes ?? [];
  const pv = state?.preview_scene_id;
  const pg = state?.program_scene_id;
  const idx = scenes.findIndex((s) => s.id === pv);

  return (
    <div className="scenes">
      <ol className="scene-list">
        {scenes.map((sc, i) => (
          <li
            key={sc.id}
            className={(sc.id === pv ? "pv " : "") + (sc.id === pg ? "pg" : "")}
            onClick={() => void setPreview(sc.id)}
            onDoubleClick={() => setRenaming(sc.id)}
            title="Click: preview · double-click: rename"
          >
            <span className="sc-key">{i < 9 ? i + 1 : ""}</span>
            {renaming === sc.id ? (
              <input
                autoFocus
                defaultValue={sc.name}
                onClick={(e) => e.stopPropagation()}
                onBlur={(e) => {
                  setRenaming(null);
                  const v = e.target.value.trim();
                  if (v && v !== sc.name) void rename(sc.id, v);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                  if (e.key === "Escape") setRenaming(null);
                }}
              />
            ) : (
              <span className="sc-name">{sc.name}</span>
            )}
            <span className="sc-tally">
              {sc.id === pg && <i className="t-pg">PGM</i>}
              {sc.id === pv && <i className="t-pv">PVW</i>}
            </span>
          </li>
        ))}
      </ol>
      <div className="panel-tools">
        <button type="button" className="tool" onClick={() => void create()} title="New empty scene">
          New
        </button>
        <button type="button" className="tool" disabled={!pv} onClick={() => pv && void duplicate(pv)} title="Duplicate the preview scene (cameras are shared, not reopened)">
          Duplicate
        </button>
        <button type="button" className="tool" disabled={idx <= 0} onClick={() => pv && void move(pv, idx - 1)} title="Move up">
          Up
        </button>
        <button type="button" className="tool" disabled={idx < 0 || idx >= scenes.length - 1} onClick={() => pv && void move(pv, idx + 1)} title="Move down">
          Down
        </button>
        <button
          type="button"
          className={"tool" + (confirm ? " danger" : "")}
          disabled={!pv || pv === pg || scenes.length < 2}
          onClick={() => {
            if (!confirm) return setConfirm(true);
            setConfirm(false);
            if (pv) void remove(pv);
          }}
          onBlur={() => setConfirm(false)}
          title={pv === pg ? "The scene on air can't be deleted" : "Delete the preview scene"}
        >
          {confirm ? "Confirm" : "Delete"}
        </button>
      </div>
    </div>
  );
}
