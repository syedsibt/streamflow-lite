import { useRef, useState } from "react";
import { useStudio } from "../lib/store";
import { useProduction } from "../lib/productionStore";
import type { ColorGrade } from "../lib/types";

/** Neutral (identity) grade for Before/After comparison. */
const NEUTRAL_GRADE: ColorGrade = {
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
  lift: [0, 0, 0, 0],
  gamma_wheel: [1, 1, 1, 1],
  gain: [1, 1, 1, 1],
  offset: [0, 0, 0, 0],
  curves: {
    master: { points: [[0, 0], [1, 1]] },
    r: { points: [[0, 0], [1, 1]] },
    g: { points: [[0, 0], [1, 1]] },
    b: { points: [[0, 0], [1, 1]] },
    luma: { points: [[0, 0], [1, 1]] },
    hue_vs_sat: { points: [[0, 0], [1, 1]] },
  },
  lut: null,
  skin_protect: false,
};

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

  // Before/After: stash the working grade, show neutral, restore on toggle-off.
  const [showingBefore, setShowingBefore] = useState(false);
  const beforeStash = useRef<ColorGrade | null>(null);

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

  const toggleBeforeAfter = () => {
    if (showingBefore) {
      setShowingBefore(false);
      if (beforeStash.current) void applyGradeSnapshot(beforeStash.current);
    } else {
      if (grade) beforeStash.current = grade;
      setShowingBefore(true);
      void applyGradeSnapshot(NEUTRAL_GRADE);
    }
  };

  return (
    <div className="header">
      <div className="title">Color Studio</div>
      <select
        value={inputId}
        onChange={(e) => {
          const opt = e.target.selectedOptions[0];
          void setInputId(e.target.value, opt?.dataset.name ?? e.target.value);
        }}
        title="Color Studio target: which source the filters and grades apply to"
      >
        <option value="cam1" data-name="Camera 1">
          Camera 1 (default)
        </option>
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
      <button
        className={showingBefore ? "active" : ""}
        onClick={toggleBeforeAfter}
        title="Toggle before/after"
      >
        Before/After
      </button>
      <button onClick={() => void resetSection(null)} title="Reset all color">
        Reset
      </button>
    </div>
  );
}
