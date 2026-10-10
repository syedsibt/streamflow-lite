import { useProduction } from "../../lib/productionStore";

/**
 * Multiview-lite: one small thumbnail per scene (~192×108, ~0.5 Hz),
 * click = set preview.
 */
export default function MultiviewGrid() {
  const scenes = useProduction((s) => s.state?.scenes) ?? [];
  const previewSceneId = useProduction((s) => s.state?.preview_scene_id ?? null);
  const programSceneId = useProduction((s) => s.state?.program_scene_id ?? null);
  const thumbs = useProduction((s) => s.thumbs);
  const setPreview = useProduction((s) => s.setPreview);

  if (scenes.length === 0) return null;

  return (
    <div className="panel multiview">
      <div className="panel-head">
        <span className="panel-title">Multiview</span>
      </div>
      <div className="multiview-grid">
        {scenes.map((sc) => {
          const url = thumbs[sc.id] ?? null;
          const cls =
            "mv-thumb" +
            (sc.id === previewSceneId ? " is-preview" : "") +
            (sc.id === programSceneId ? " is-program" : "");
          return (
            <div
              key={sc.id}
              className={cls}
              onClick={() => void setPreview(sc.id)}
              title={`Set preview: ${sc.name}`}
            >
              {url ? (
                <img src={url} alt={sc.name} />
              ) : (
                <div className="mv-empty note">…</div>
              )}
              <div className="mv-label">{sc.name}</div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
