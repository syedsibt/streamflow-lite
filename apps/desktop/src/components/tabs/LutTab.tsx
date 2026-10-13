import { useStudio } from "../../lib/store";
import Slider from "../Slider";

/**
 * LUT library: Import (.cube) → pick → live preview → intensity → done.
 * Every change is live on the program output; nothing is baked into the
 * camera image (remove/replace any time).
 */
export default function LutTab() {
  const grade = useStudio((s) => s.grade);
  const luts = useStudio((s) => s.luts);
  const importLut = useStudio((s) => s.importLut);
  const applyLibraryLut = useStudio((s) => s.applyLibraryLut);
  const removeLut = useStudio((s) => s.removeLut);
  const setLutIntensity = useStudio((s) => s.setLutIntensity);
  const setLutPosition = useStudio((s) => s.setLutPosition);
  const resetSection = useStudio((s) => s.resetSection);

  const current = grade?.lut ?? null;
  const currentName = current
    ? luts.find((l) => l.path === current.file)?.name ?? current.file.split(/[\\/]/).pop()
    : null;

  return (
    <div>
      <div className="btn-row">
        <button className="btn" onClick={() => void importLut()} title="Import a .cube LUT (Premiere Pro / Lumetri, Resolve, …)">
          + Import LUT
        </button>
        {current && (
          <button className="btn ghost" onClick={() => void resetSection("lut")} title="Detach the LUT from this source">
            No LUT
          </button>
        )}
      </div>

      {current ? (
        <div className="lut-active">
          <div className="section-title">Applied: {currentName}</div>
          <Slider
            label="Intensity"
            min={0}
            max={1}
            step={0.01}
            neutral={1}
            value={current.intensity}
            format={(v) => `${Math.round(v * 100)}%`}
            onChange={setLutIntensity}
          />
          <div className="seg" role="group" aria-label="LUT position">
            <button
              className={current.position === "Before" ? "active" : ""}
              onClick={() => void setLutPosition("Before")}
              title="Technical/input LUT (e.g. LOG → Rec.709): applied first, then your grade"
            >
              Input LUT (before grade)
            </button>
            <button
              className={current.position === "After" ? "active" : ""}
              onClick={() => void setLutPosition("After")}
              title="Creative/look LUT: applied after your grade"
            >
              Creative LUT (after grade)
            </button>
          </div>
        </div>
      ) : (
        <div className="note" style={{ margin: "8px 0" }}>
          No LUT on this source.
        </div>
      )}

      <div className="section-title">LUT library</div>
      {luts.length === 0 && (
        <div className="note">Your imported LUTs appear here. Click one to preview it live.</div>
      )}
      <div className="lut-list">
        {luts.map((l) => (
          <div key={l.id} className={`lut-item ${current?.file === l.path ? "active" : ""}`}>
            <button className="lut-name" onClick={() => void applyLibraryLut(l)} title={l.path}>
              {l.name}
            </button>
            <button
              className="reset-btn"
              title="Delete from library"
              onClick={() => {
                if (confirm(`Delete "${l.name}" from the LUT library?`)) void removeLut(l.id);
              }}
            >
              ✕
            </button>
          </div>
        ))}
      </div>
      <div className="note" style={{ marginTop: 10 }}>
        Supported: standard <b>.cube</b> (Adobe Cube 1.0 and Resolve style: 1D, 3D 2–256, shaper + 3D,
        DOMAIN / INPUT_RANGE). Premiere Pro: Lumetri Color ▸ ⋯ ▸ Export .cube. Not supported:
        .3dl, .look, .csp — export those as .cube.
      </div>
    </div>
  );
}
