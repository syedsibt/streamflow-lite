import { useState } from "react";
import { useProduction } from "../../lib/productionStore";
import type { MacroActionInput } from "../../lib/types";

/** Macros: name + hotkey + action, add/remove, click a hotkey to dispatch. */

type ActionKind =
  | "cut"
  | "auto_transition"
  | "toggle_stream"
  | "toggle_record"
  | "apply_look";

export default function MacrosPanel() {
  const macros = useProduction((s) => s.state?.macros) ?? [];
  const macroAdd = useProduction((s) => s.macroAdd);
  const macroRemove = useProduction((s) => s.macroRemove);
  const macroDispatch = useProduction((s) => s.macroDispatch);

  const [name, setName] = useState("");
  const [hotkey, setHotkey] = useState("");
  const [actionKind, setActionKind] = useState<ActionKind>("cut");
  const [durationMs, setDurationMs] = useState(500);
  const [lookId, setLookId] = useState("");
  const [intensity, setIntensity] = useState(80);

  const buildAction = (): MacroActionInput | null => {
    switch (actionKind) {
      case "cut":
        return { action: "cut" };
      case "auto_transition":
        return { action: "auto_transition", duration_ms: durationMs };
      case "toggle_stream":
        return { action: "toggle_stream" };
      case "toggle_record":
        return { action: "toggle_record" };
      case "apply_look":
        if (!lookId.trim()) return null;
        return {
          action: "apply_look",
          look_id: lookId.trim(),
          intensity: Math.min(1, Math.max(0, intensity / 100)),
        };
    }
  };

  const add = () => {
    if (!name.trim() || !hotkey.trim()) return;
    const action = buildAction();
    if (!action) return;
    void macroAdd(name.trim(), hotkey.trim(), action);
    setName("");
    setHotkey("");
  };

  return (
    <div className="panel">
      <div className="panel-head">
        <span className="panel-title">Macros</span>
      </div>
      <div className="panel-body">
        {macros.length === 0 && (
          <div className="note">No macros defined.</div>
        )}
        {macros.map((m) => (
          <div key={m.id} className="macro-row">
            <button
              className="hotkey-chip"
              onClick={() => void macroDispatch(m.hotkey)}
              title={`Dispatch ${m.hotkey} (${m.action})`}
            >
              {m.hotkey}
            </button>
            <span className="macro-name">{m.name}</span>
            <button
              className="icon-btn danger"
              onClick={() => void macroRemove(m.id)}
              title="Remove macro"
            >
              ✕
            </button>
          </div>
        ))}
        <div className="macro-add">
          <input
            type="text"
            placeholder="Name"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <input
            type="text"
            placeholder="Hotkey"
            value={hotkey}
            onChange={(e) => setHotkey(e.target.value)}
          />
          <select
            value={actionKind}
            onChange={(e) => setActionKind(e.target.value as ActionKind)}
            title="Macro action"
          >
            <option value="cut">Cut</option>
            <option value="auto_transition">Auto transition</option>
            <option value="toggle_stream">Toggle stream</option>
            <option value="toggle_record">Toggle record</option>
            <option value="apply_look">Apply look</option>
          </select>
          {actionKind === "auto_transition" && (
            <input
              type="number"
              min={50}
              max={5000}
              step={50}
              value={durationMs}
              onChange={(e) => setDurationMs(parseInt(e.target.value, 10) || 500)}
              title="Transition duration (ms)"
            />
          )}
          {actionKind === "apply_look" && (
            <>
              <input
                type="text"
                placeholder="Look id"
                value={lookId}
                onChange={(e) => setLookId(e.target.value)}
              />
              <input
                type="number"
                min={0}
                max={100}
                value={intensity}
                onChange={(e) => setIntensity(parseInt(e.target.value, 10) || 0)}
                title="Look intensity %"
              />
            </>
          )}
          <button className="btn ghost" onClick={add}>
            Add
          </button>
        </div>
      </div>
    </div>
  );
}
