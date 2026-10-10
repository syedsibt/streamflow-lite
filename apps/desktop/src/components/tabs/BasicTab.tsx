import { useStudio } from "../../lib/store";
import { BASIC_PARAMS } from "../../lib/types";
import Slider from "../Slider";

/** Neutral defaults for the Basic tab (identity grade). */
const NEUTRAL: Record<string, number> = {
  exposure: 0,
  brightness: 0,
  contrast: 0,
  saturation: 100,
  vibrance: 0,
  temperature: 6500,
  tint: 0,
  highlights: 0,
  shadows: 0,
  whites: 0,
  blacks: 0,
  gamma: 1,
  sharpness: 0,
};

export default function BasicTab() {
  const grade = useStudio((s) => s.grade);
  const autoColor = useStudio((s) => s.autoColor);
  const resetSection = useStudio((s) => s.resetSection);
  const autoColorNote = useStudio((s) => s.autoColorNote);

  if (grade === null) return <div className="note">Waiting for backend…</div>;

  const g = grade as unknown as Record<string, number>;

  return (
    <div>
      {BASIC_PARAMS.map((def) => (
        <Slider
          key={def.param}
          label={def.label}
          param={def.param}
          min={def.min}
          max={def.max}
          step={def.step}
          value={g[def.param] ?? NEUTRAL[def.param] ?? def.min}
          format={def.format}
        />
      ))}
      <div className="btn-row">
        <button className="btn" onClick={() => void autoColor()}>
          Auto Color
        </button>
        <button className="btn ghost" onClick={() => void resetSection("basic")}>
          Reset
        </button>
      </div>
      {autoColorNote.map((note, i) => (
        <div key={i} className="note">
          {note}
        </div>
      ))}
    </div>
  );
}
