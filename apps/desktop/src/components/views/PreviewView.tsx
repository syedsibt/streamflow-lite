import { useProduction } from "../../lib/productionStore";
import Monitor from "../Monitor";

/**
 * PREVIEW: the program output, as large as the window allows. This is the
 * exact frame the encoder receives (one render, shared by every output).
 * The chips below toggle sources on and off the picture.
 */
export default function PreviewView() {
  const state = useProduction((s) => s.state);
  const sceneId = useProduction((s) => s.sceneId());
  const sourceSetVisible = useProduction((s) => s.sourceSetVisible);
  const scene = state?.scenes.find((s) => s.id === sceneId);
  const visual = [...(scene?.sources ?? [])].filter((s) => s.kind !== "audioinput").sort((a, b) => b.order - a.order);

  return (
    <div className="workspace single">
      <div className="stage-area big">
        <Monitor />
        {visual.length > 0 && (
          <div className="chip-row" role="group" aria-label="Sources on the picture">
            {visual.map((s) => (
              <button
                key={s.id}
                type="button"
                className={"toggle-chip" + (s.visible ? " on" : "")}
                aria-pressed={s.visible}
                title={`${s.visible ? "Hide" : "Show"} ${s.name}`}
                onClick={() => void sourceSetVisible(s.id, !s.visible)}
              >
                <span className="toggle-chip-text">{s.name}</span>
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
