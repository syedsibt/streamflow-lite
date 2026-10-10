import { useState } from "react";
import { useStudio } from "../../lib/store";
import type { LutPosition } from "../../lib/types";

const inputStyle = {
  width: "100%",
  background: "var(--bg-raised)",
  border: "1px solid var(--border)",
  borderRadius: 6,
  color: "var(--text)",
  padding: "8px 10px",
  fontSize: 12,
  marginBottom: 10,
} as const;

export default function LutTab() {
  const grade = useStudio((s) => s.grade);
  const loadLut = useStudio((s) => s.loadLut);
  const resetSection = useStudio((s) => s.resetSection);

  const [path, setPath] = useState("");
  const [intensity, setIntensity] = useState(80);
  const [position, setPosition] = useState<LutPosition>("After");

  const current = grade?.lut ?? null;

  return (
    <div>
      {current ? (
        <div className="note" style={{ marginBottom: 8 }}>
          Loaded: {current.file} — {Math.round(current.intensity * 100)}% —{" "}
          {current.position}
        </div>
      ) : (
        <div className="note" style={{ marginBottom: 8 }}>
          No LUT loaded.
        </div>
      )}
      <input
        type="text"
        placeholder="color/luts/my.cube"
        value={path}
        onChange={(e) => setPath(e.target.value)}
        style={inputStyle}
      />
      <div className="slider-row">
        <label>Intensity</label>
        <input
          type="range"
          min={0}
          max={100}
          step={1}
          value={intensity}
          onChange={(e) => setIntensity(parseInt(e.target.value, 10))}
        />
        <span className="value">{intensity}%</span>
      </div>
      <div className="btn-row">
        <button
          className={position === "Before" ? "btn" : "btn ghost"}
          onClick={() => setPosition("Before")}
        >
          Before
        </button>
        <button
          className={position === "After" ? "btn" : "btn ghost"}
          onClick={() => setPosition("After")}
        >
          After
        </button>
      </div>
      <div className="btn-row">
        <button
          className="btn"
          disabled={!path.trim()}
          onClick={() => void loadLut(path.trim(), position, intensity / 100)}
        >
          Apply LUT
        </button>
        <button
          className="btn ghost"
          onClick={() => void resetSection("lut")}
        >
          Remove LUT
        </button>
      </div>
      <div className="note">
        File picker & drag-drop import land with the Tauri dialog plugin; type a
        project-relative path for now.
      </div>
    </div>
  );
}
