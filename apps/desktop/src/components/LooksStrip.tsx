import { useMemo, useState } from "react";
import { useStudio } from "../lib/store";

/** Main filter families first (CapCut-style), then the rest. */
const CATEGORY_ORDER = [
  "Cinematic",
  "Portrait",
  "Creative",
  "Social",
  "Creator",
  "Natural",
  "Vintage",
  "BlackAndWhite",
  "Event",
  "Gaming",
  "Fix",
];
const CATEGORY_LABEL: Record<string, string> = { BlackAndWhite: "B&W", Gaming: "Gaming/Screen" };

/**
 * One-click filters. A filter is a *parameter preset* (never baked pixels):
 * one active filter per source, with a live 0–100 % intensity that blends
 * every parameter proportionally.
 */
export default function LooksStrip() {
  const looks = useStudio((s) => s.looks);
  const stack = useStudio((s) => s.stack);
  const lookIntensity = useStudio((s) => s.lookIntensity);
  const setLookIntensity = useStudio((s) => s.setLookIntensity);
  const applyLook = useStudio((s) => s.applyLook);
  const clearLook = useStudio((s) => s.clearLook);
  const [cat, setCat] = useState<string>("Cinematic");

  const activeSlot = stack.find((s) => s.kind === "look");
  const activeId = activeSlot?.look_id ?? null;
  const intensity = activeSlot?.intensity ?? lookIntensity;

  const categories = useMemo(() => {
    const present = new Set(looks.map((l) => l.category));
    return [
      ...CATEGORY_ORDER.filter((c) => present.has(c)),
      ...[...present].filter((c) => !CATEGORY_ORDER.includes(c)).sort(),
    ];
  }, [looks]);

  const shown = useMemo(
    () => looks.filter((l) => l.category === cat).sort((a, b) => a.name.localeCompare(b.name)),
    [looks, cat],
  );

  return (
    <div className="looks-strip">
      <div className="looks-top">
        <div className="cat-chips" role="tablist" aria-label="Filter categories">
          {categories.map((c) => (
            <button
              key={c}
              role="tab"
              aria-selected={cat === c}
              className={cat === c ? "cat-chip active" : "cat-chip"}
              onClick={() => setCat(c)}
            >
              {CATEGORY_LABEL[c] ?? c}
            </button>
          ))}
        </div>
        <div className="slider-row look-intensity" title="Filter intensity — 0% = off, 100% = full filter">
          <label>Intensity</label>
          <input
            type="range"
            min={0}
            max={100}
            step={1}
            value={Math.round(intensity * 100)}
            onChange={(e) => setLookIntensity(parseFloat(e.target.value) / 100)}
            aria-label="Filter intensity"
          />
          <span className="value">{Math.round(intensity * 100)}%</span>
        </div>
      </div>
      <div className="look-chips">
        <button
          className={activeId ? "look-chip" : "look-chip active"}
          onClick={() => void clearLook()}
          title="No filter"
        >
          None
        </button>
        {looks.length === 0 && <div className="note">No filters available.</div>}
        {shown.map((l) => (
          <button
            key={l.id}
            className={activeId === l.id ? "look-chip active" : "look-chip"}
            onClick={() => void applyLook(l.id)}
            title={`Apply ${l.name}`}
          >
            {l.name}
          </button>
        ))}
      </div>
    </div>
  );
}
