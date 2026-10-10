import { useProduction } from "../../lib/productionStore";

/**
 * Scenes panel. Click a scene = set preview; double-click (or the Cut
 * button) = cut it to program. Add/rename/duplicate/delete per scene.
 */
export default function ScenesPanel() {
  const scenes = useProduction((s) => s.state?.scenes) ?? [];
  const previewSceneId = useProduction((s) => s.state?.preview_scene_id ?? null);
  const programSceneId = useProduction((s) => s.state?.program_scene_id ?? null);
  const setPreview = useProduction((s) => s.setPreview);
  const transitionCut = useProduction((s) => s.transitionCut);
  const sceneCreate = useProduction((s) => s.sceneCreate);
  const sceneRename = useProduction((s) => s.sceneRename);
  const sceneDelete = useProduction((s) => s.sceneDelete);
  const sceneDuplicate = useProduction((s) => s.sceneDuplicate);

  const addScene = () => {
    const name = window.prompt("Scene name:", `Scene ${scenes.length + 1}`);
    if (name && name.trim()) void sceneCreate(name.trim());
  };

  const rename = (id: string, current: string) => {
    const name = window.prompt("Rename scene:", current);
    if (name && name.trim() && name.trim() !== current)
      void sceneRename(id, name.trim());
  };

  const remove = (id: string, name: string) => {
    if (window.confirm(`Delete scene "${name}"?`)) void sceneDelete(id);
  };

  return (
    <div className="panel scenes-panel">
      <div className="panel-head">
        <span className="panel-title">Scenes</span>
        <button className="icon-btn" onClick={addScene} title="Add scene">
          +
        </button>
      </div>
      <div className="panel-body">
        {scenes.length === 0 && (
          <div className="note">No scenes yet — click + to add one.</div>
        )}
        {scenes.map((sc) => (
          <div
            key={sc.id}
            className={
              "scene-row" +
              (sc.id === previewSceneId ? " is-preview" : "") +
              (sc.id === programSceneId ? " is-program" : "")
            }
            onClick={() => void setPreview(sc.id)}
            onDoubleClick={() => {
              void setPreview(sc.id);
              void transitionCut();
            }}
            title="Click = set preview · double-click = cut to program"
          >
            <span
              className="bus-dot preview-dot"
              style={{ opacity: sc.id === previewSceneId ? 1 : 0.15 }}
            />
            <span
              className="bus-dot program-dot"
              style={{ opacity: sc.id === programSceneId ? 1 : 0.15 }}
            />
            <span className="scene-name">{sc.name}</span>
            <span className="scene-actions">
              <button
                className="icon-btn"
                title="Rename"
                onClick={(e) => {
                  e.stopPropagation();
                  rename(sc.id, sc.name);
                }}
              >
                ✎
              </button>
              <button
                className="icon-btn"
                title="Duplicate"
                onClick={(e) => {
                  e.stopPropagation();
                  void sceneDuplicate(sc.id);
                }}
              >
                ⧉
              </button>
              <button
                className="icon-btn danger"
                title="Delete"
                onClick={(e) => {
                  e.stopPropagation();
                  remove(sc.id, sc.name);
                }}
              >
                ✕
              </button>
            </span>
          </div>
        ))}
      </div>
      <div className="note" style={{ padding: "0 10px 8px" }}>
        Double-click a scene to cut it straight to program.
      </div>
    </div>
  );
}
