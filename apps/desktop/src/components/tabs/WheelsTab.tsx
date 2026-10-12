import { useStudio } from "../../lib/store";
import type { Vec4 } from "../../lib/types";
import ColorWheel, { type WheelSpec } from "../ColorWheel";
import Slider from "../Slider";

/** Shadows / Midtones / Highlights (+ Global offset) color wheels. */
const SPECS: WheelSpec[] = [
  { key: "lift", label: "Shadows", hint: "Tints the darks (lift)", neutral: 0, chroma: 0.03, lumScale: 0.04 },
  { key: "gamma_wheel", label: "Midtones", hint: "Tints the mids (gamma)", neutral: 1, chroma: 0.15, lumScale: 0.35 },
  { key: "gain", label: "Highlights", hint: "Tints the brights (gain)", neutral: 1, chroma: 0.2, lumScale: 0.4 },
  { key: "offset", label: "Global", hint: "Shifts everything (offset)", neutral: 0, chroma: 0.02, lumScale: 0.03 },
];

const RANGES: Record<string, { min: number; max: number }> = {
  lift: { min: -1, max: 1 },
  offset: { min: -1, max: 1 },
  gamma_wheel: { min: 0.1, max: 3 },
  gain: { min: 0, max: 4 },
};

export default function WheelsTab() {
  const grade = useStudio((s) => s.grade);
  const setParam = useStudio((s) => s.setParam);
  const resetSection = useStudio((s) => s.resetSection);

  if (grade === null) return <div className="note">Waiting for backend…</div>;

  return (
    <div>
      <div className="wheel-grid">
        {SPECS.map((spec) => (
          <ColorWheel
            key={spec.key}
            spec={spec}
            value={grade[spec.key] as Vec4}
            onChannels={(patch) => {
              for (const [ch, v] of Object.entries(patch)) {
                if (v !== undefined) setParam(`${spec.key}_${ch}`, v);
              }
            }}
          />
        ))}
      </div>
      <div className="btn-row" style={{ marginTop: 10 }}>
        <button className="btn ghost" onClick={() => void resetSection("wheels")}>
          Reset wheels
        </button>
      </div>
      <details className="advanced">
        <summary>Advanced: per-channel values</summary>
        {SPECS.map((spec) => (
          <div key={spec.key} className="param-group">
            <div className="section-title">{spec.label}</div>
            {(["r", "g", "b", "master"] as const).map((ch, idx) => (
              <Slider
                key={ch}
                label={ch === "master" ? "Master" : ch.toUpperCase()}
                param={`${spec.key}_${ch}`}
                min={RANGES[spec.key].min}
                max={RANGES[spec.key].max}
                step={0.005}
                neutral={spec.neutral}
                value={(grade[spec.key] as Vec4)[idx]}
                format={(v) => v.toFixed(3)}
              />
            ))}
          </div>
        ))}
      </details>
    </div>
  );
}
