import { useState } from "react";
import { useStudio } from "../../lib/store";

export default function FiltersTab() {
  const stack = useStudio((s) => s.stack);
  const effectKinds = useStudio((s) => s.effectKinds);
  const addEffect = useStudio((s) => s.addEffect);
  const setSlotIntensity = useStudio((s) => s.setSlotIntensity);
  const toggleSlot = useStudio((s) => s.toggleSlot);
  const removeSlot = useStudio((s) => s.removeSlot);
  const moveSlot = useStudio((s) => s.moveSlot);
  const [newKind, setNewKind] = useState("");

  return (
    <div>
      {stack.length === 0 && (
        <div className="note" style={{ marginBottom: 8 }}>
          Stack is empty — add a filter below.
        </div>
      )}
      {stack.map((s) => (
        <div key={s.id} className={s.enabled ? "slot" : "slot disabled"}>
          <div className="slot-head">
            <input
              type="checkbox"
              checked={s.enabled}
              onChange={(e) => void toggleSlot(s.id, e.target.checked)}
              title={s.enabled ? "Disable" : "Enable"}
            />
            <span className="label">
              {s.label}
              {s.is_ai ? " (AI)" : ""}
            </span>
            <span className={`cost-badge ${s.cost}`}>{s.cost}</span>
            <button
              className="icon-btn"
              title="Move up"
              onClick={() => void moveSlot(s.id, -1)}
            >
              ↑
            </button>
            <button
              className="icon-btn"
              title="Move down"
              onClick={() => void moveSlot(s.id, 1)}
            >
              ↓
            </button>
            <button
              className="icon-btn"
              title="Remove"
              onClick={() => void removeSlot(s.id)}
            >
              ×
            </button>
          </div>
          <div
            className="slider-row"
            style={{ marginTop: 6, marginBottom: 0 }}
          >
            <label>Intensity</label>
            <input
              type="range"
              min={0}
              max={100}
              step={1}
              value={Math.round(s.intensity * 100)}
              onChange={(e) =>
                void setSlotIntensity(s.id, parseFloat(e.target.value) / 100)
              }
            />
            <span className="value">{Math.round(s.intensity * 100)}%</span>
          </div>
        </div>
      ))}
      <div className="btn-row" style={{ marginTop: 8 }}>
        <select
          value={newKind}
          onChange={(e) => setNewKind(e.target.value)}
          style={{
            flex: 1,
            background: "var(--bg-raised)",
            color: "var(--text)",
            border: "1px solid var(--border)",
            borderRadius: 8,
            padding: "8px 10px",
            fontSize: 12,
          }}
        >
          <option value="" disabled>
            Add filter…
          </option>
          {effectKinds.map((k) => (
            <option key={k.kind} value={k.kind}>
              {k.label} ({k.cost}){k.is_ai ? " (AI)" : ""}
            </option>
          ))}
        </select>
        <button
          className="btn"
          disabled={!newKind}
          onClick={() => {
            if (newKind) void addEffect(newKind);
          }}
        >
          Add
        </button>
      </div>
    </div>
  );
}
