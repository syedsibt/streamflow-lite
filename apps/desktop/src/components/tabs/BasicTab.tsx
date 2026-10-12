import { useStudio } from "../../lib/store";
import { BASIC_GROUPS } from "../../lib/types";
import Slider from "../Slider";

/** Basic correction (white balance → tone → color), Lumetri-style order. */
export default function BasicTab() {
  const grade = useStudio((s) => s.grade);
  const autoColor = useStudio((s) => s.autoColor);
  const resetSection = useStudio((s) => s.resetSection);
  const autoColorNote = useStudio((s) => s.autoColorNote);

  if (grade === null) return <div className="note">Waiting for backend…</div>;
  const g = grade as unknown as Record<string, number>;

  return (
    <div>
      {BASIC_GROUPS.map((group) => (
        <div key={group.title} className="param-group">
          <div className="section-title">{group.title}</div>
          {group.params.map((def) => (
            <Slider
              key={def.param}
              label={def.label}
              param={def.param}
              min={def.min}
              max={def.max}
              step={def.step}
              neutral={def.neutral}
              value={g[def.param] ?? def.neutral ?? def.min}
              format={def.format}
            />
          ))}
        </div>
      ))}
      <div className="btn-row">
        <button className="btn" onClick={() => void autoColor()} title="Analyze the frame and set editable values">
          Auto Color
        </button>
        <button className="btn ghost" onClick={() => void resetSection("basic")}>
          Reset basic
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
