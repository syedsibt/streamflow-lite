import { useMemo } from "react";
import { useStudio } from "../lib/store";

export default function LooksStrip() {
  const looks = useStudio((s) => s.looks);
  const lookIntensity = useStudio((s) => s.lookIntensity);
  const setLookIntensity = useStudio((s) => s.setLookIntensity);
  const applyLook = useStudio((s) => s.applyLook);

  // Grouped view: sorted by category, then name.
  const sorted = useMemo(
    () =>
      [...looks].sort(
        (a, b) =>
          a.category.localeCompare(b.category) ||
          a.name.localeCompare(b.name),
      ),
    [looks],
  );

  return (
    <div className="looks-strip">
      <div
        className="slider-row"
        style={{ flexShrink: 0, width: 190, marginBottom: 0 }}
      >
        <label>Look</label>
        <input
          type="range"
          min={0}
          max={100}
          step={1}
          value={Math.round(lookIntensity * 100)}
          onChange={(e) => setLookIntensity(parseFloat(e.target.value) / 100)}
          title="Look intensity"
        />
        <span className="value">{Math.round(lookIntensity * 100)}%</span>
      </div>
      {sorted.length === 0 && (
        <div className="note">No looks available.</div>
      )}
      {sorted.map((l) => (
        <button
          key={l.id}
          className="look-chip"
          onClick={() => void applyLook(l.id)}
          title={`Apply ${l.name}`}
        >
          {l.name}
          <span className="cat">{l.category}</span>
        </button>
      ))}
    </div>
  );
}
