import { useState } from "react";
import { useStudio } from "../../lib/store";

/** Save / apply / delete complete looks (grade + LUT + filters + glow + skin). */
export default function PresetsTab() {
  const presets = useStudio((s) => s.presets);
  const savePreset = useStudio((s) => s.savePreset);
  const applyPreset = useStudio((s) => s.applyPreset);
  const deletePreset = useStudio((s) => s.deletePreset);
  const [name, setName] = useState("");

  return (
    <div>
      <div className="section-title">Save current look</div>
      <div className="add-row">
        <input
          type="text"
          placeholder="e.g. My Podcast Look"
          value={name}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && name.trim()) {
              void savePreset(name.trim());
              setName("");
            }
          }}
        />
        <button
          className="btn"
          disabled={!name.trim()}
          onClick={() => {
            void savePreset(name.trim());
            setName("");
          }}
        >
          Save
        </button>
      </div>
      <div className="note">
        Saves basic correction, curves, wheels, LUT + intensity, look + intensity, glow and skin settings.
        Saving with an existing name updates it.
      </div>
      <div className="section-title">My presets</div>
      {presets.length === 0 && <div className="note">No presets yet.</div>}
      <div className="lut-list">
        {presets.map((p) => (
          <div key={p.id} className="lut-item">
            <button className="lut-name" onClick={() => void applyPreset(p.id)} title="Apply to this source">
              {p.name}
            </button>
            <button
              className="reset-btn"
              title="Delete preset"
              onClick={() => {
                if (confirm(`Delete preset "${p.name}"?`)) void deletePreset(p.id);
              }}
            >
              ✕
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
