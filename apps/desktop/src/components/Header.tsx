import { useState } from "react";
import { useStudio } from "../lib/store";
import { useProduction } from "../lib/productionStore";
import type { ColorGrade } from "../lib/types";

export default function Header() {
  const inputId = useStudio((s) => s.inputId);
  const setInputId = useStudio((s) => s.setInputId);
  const grade = useStudio((s) => s.grade);
  const applyGradeSnapshot = useStudio((s) => s.applyGradeSnapshot);
  const resetSection = useStudio((s) => s.resetSection);
  // Real production sources (camera/media) with engine input ids — the
  // Color Studio target list. Filters/grades apply to the selected input.
  const scenes = useProduction((s) => s.state?.scenes ?? []);
  const realSources = scenes.flatMap((sc) =>
    sc.sources
      .filter((src) => src.input_id)
      .map((src) => ({ input_id: src.input_id as string, name: src.name })),
  );

  // A/B comparison slots: full-grade snapshots.
  const [snapA, setSnapA] = useState<ColorGrade | null>(null);
  const [snapB, setSnapB] = useState<ColorGrade | null>(null);
  const [activeAB, setActiveAB] = useState<"A" | "B" | null>(null);

  // Before/After is a *preview-only* comparison: the stream keeps the graded
  // image (the old toggle applied a neutral grade to the live input).
  const previewMode = useStudio((s) => s.previewMode);
  const setPreviewMode = useStudio((s) => s.setPreviewMode);

  const clickA = () => {
    if (activeAB === "A") return;
    if (grade) setSnapB(grade); // stash current work in the other slot so nothing is lost
    if (snapA) void applyGradeSnapshot(snapA);
    setActiveAB("A");
  };

  const clickB = () => {
    if (activeAB === "B") return;
    if (grade) setSnapA(grade); // stash current work in the other slot so nothing is lost
    if (snapB) void applyGradeSnapshot(snapB);
    setActiveAB("B");
  };

  return (
    <div className="color-header">
      <select
        className="source-select"
        value={inputId}
        onChange={(e) => {
          const opt = e.target.selectedOptions[0];
          void setInputId(e.target.value, opt?.dataset.name ?? e.target.value);
        }}
        title="Color Studio target: which source the filters and grades apply to"
      >
        {realSources.length === 0 && (
          <option value={inputId} data-name="No video source">
            No video source
          </option>
        )}
        {realSources.map((src) => (
          <option key={src.input_id} value={src.input_id} data-name={src.name}>
            {src.name}
          </option>
        ))}
      </select>
      <div className="spacer" />
      <button
        className={activeAB === "A" ? "active" : ""}
        onClick={clickA}
        title="Compare snapshot A"
      >
        A
      </button>
      <button
        className={activeAB === "B" ? "active" : ""}
        onClick={clickB}
        title="Compare snapshot B"
      >
        B
      </button>
      <div className="seg" role="group" aria-label="Before / after (preview only)">
        {(["before", "split", "after"] as const).map((m) => (
          <button
            key={m}
            className={previewMode === m ? "active" : ""}
            onClick={() => setPreviewMode(m)}
            title={
              m === "split"
                ? "Left: original · Right: processed (preview only — the stream is unaffected)"
                : m === "before"
                  ? "Original camera image (preview only — the stream keeps your grade)"
                  : "Processed output — exactly what goes to the stream"
            }
          >
            {m === "before" ? "Before" : m === "split" ? "Split" : "After"}
          </button>
        ))}
      </div>
      <button className="text-btn" onClick={() => void resetSection(null)} title="Reset all color">
        Reset
      </button>
    </div>
  );
}
