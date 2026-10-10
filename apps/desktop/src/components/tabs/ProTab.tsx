import { useStudio } from "../../lib/store";
import { WHEELS } from "../../lib/types";
import Slider from "../Slider";

const RANGES: Record<string, { min: number; max: number; step: number }> = {
  lift: { min: -1, max: 1, step: 0.01 },
  offset: { min: -1, max: 1, step: 0.01 },
  gamma_wheel: { min: 0.1, max: 3, step: 0.01 },
  gain: { min: 0, max: 4, step: 0.01 },
};

const CHANNELS = [
  { suffix: "_r", label: "R", idx: 0 },
  { suffix: "_g", label: "G", idx: 1 },
  { suffix: "_b", label: "B", idx: 2 },
  { suffix: "_master", label: "Master", idx: 3 },
] as const;

const fmt = (v: number) => v.toFixed(2);

export default function ProTab() {
  const grade = useStudio((s) => s.grade);
  const resetSection = useStudio((s) => s.resetSection);

  return (
    <div>
      <div className="wheel-grid">
        {WHEELS.map(({ key, label }) => {
          const range = RANGES[key] ?? { min: -1, max: 1, step: 0.01 };
          const vec = grade?.[key];
          return (
            <div key={key} className="wheel">
              <div className="wheel-label">{label}</div>
              {CHANNELS.map((c) => (
                <Slider
                  key={c.suffix}
                  label={c.label}
                  param={`${key}${c.suffix}`}
                  min={range.min}
                  max={range.max}
                  step={range.step}
                  value={vec?.[c.idx] ?? 0}
                  format={fmt}
                />
              ))}
            </div>
          );
        })}
      </div>
      <div className="btn-row" style={{ marginTop: 10 }}>
        <button
          className="btn ghost"
          onClick={() => void resetSection("wheels")}
        >
          Reset wheels
        </button>
      </div>
      <div className="note">
        Puck-style wheels are planned; per-channel sliders map 1:1 to the engine
        wheels.
      </div>
    </div>
  );
}
