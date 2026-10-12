import { useStudio } from "../../lib/store";
import { CREATIVE_PARAMS } from "../../lib/types";
import Slider from "../Slider";

/**
 * Creative (Lumetri-style): the look + its intensity live in the filter
 * strip below the preview; here are the creative finishing controls.
 */
export default function CreativeTab() {
  const grade = useStudio((s) => s.grade);
  const stack = useStudio((s) => s.stack);
  const looks = useStudio((s) => s.looks);
  const lookIntensity = useStudio((s) => s.lookIntensity);
  const setLookIntensity = useStudio((s) => s.setLookIntensity);
  const clearLook = useStudio((s) => s.clearLook);
  const resetSection = useStudio((s) => s.resetSection);

  if (grade === null) return <div className="note">Waiting for backend…</div>;
  const g = grade as unknown as Record<string, number>;
  const lookSlot = stack.find((s) => s.kind === "look");
  const lookName = looks.find((l) => l.id === lookSlot?.look_id)?.name;

  return (
    <div>
      <div className="section-title">Look</div>
      {lookSlot ? (
        <>
          <div className="note">
            Active: <b>{lookName ?? lookSlot.label}</b> — pick another in the filter strip to swap.
          </div>
          <Slider
            label="Intensity"
            min={0}
            max={1}
            step={0.01}
            neutral={1}
            value={lookSlot.intensity ?? lookIntensity}
            format={(v) => `${Math.round(v * 100)}%`}
            onChange={setLookIntensity}
          />
          <div className="btn-row">
            <button className="btn ghost" onClick={() => void clearLook()}>
              Remove look
            </button>
          </div>
        </>
      ) : (
        <div className="note">No look — choose one from the filter strip below the preview.</div>
      )}
      <div className="section-title">Adjustments</div>
      {CREATIVE_PARAMS.map((def) => (
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
      <div className="btn-row">
        <button className="btn ghost" onClick={() => void resetSection("creative")}>
          Reset creative
        </button>
      </div>
    </div>
  );
}
