import { useEffect, useState } from "react";
import { useProduction } from "../../lib/productionStore";
import { api } from "../../lib/ipc";
import { isPendingCaptureSource, type SourceKind } from "../../lib/types";

const KIND_OPTIONS = [
  "test_pattern",
  "color",
  "camera",
  "screen",
  "media",
  "text",
] as const;
type KindKey = (typeof KIND_OPTIONS)[number];

/**
 * Sources of the preview scene: visibility eye toggle, reorder up/down,
 * add (TestPattern/Color/Camera/Screen/Media/Text), remove.
 */
export default function SourcesPanel() {
  const state = useProduction((s) => s.state);
  const sourceAdd = useProduction((s) => s.sourceAdd);
  const sourceRemove = useProduction((s) => s.sourceRemove);
  const sourceReorder = useProduction((s) => s.sourceReorder);
  const sourceSetVisible = useProduction((s) => s.sourceSetVisible);

  const [kind, setKind] = useState<KindKey>("test_pattern");
  const [name, setName] = useState("");
  const [color, setColor] = useState("#3366ff");
  const [text, setText] = useState("");
  const [mediaPath, setMediaPath] = useState("");
  const [cameras, setCameras] = useState<string[]>([]);
  const [cameraDevice, setCameraDevice] = useState<string>("");

  // Load camera devices when the camera kind is selected.
  useEffect(() => {
    if (kind !== "camera") return;
    let cancelled = false;
    void api
      .listCameras()
      .then((list) => {
        if (cancelled) return;
        setCameras(list);
        setCameraDevice((prev) => prev || list[0] || "");
      })
      .catch(() => {
        if (!cancelled) setCameras([]);
      });
    return () => {
      cancelled = true;
    };
  }, [kind]);

  const scene = state?.scenes.find((s) => s.id === state.preview_scene_id) ?? null;
  const sources = [...(scene?.sources ?? [])].sort((a, b) => a.order - b.order);

  const buildKind = (): SourceKind => {
    switch (kind) {
      case "color": {
        const n = parseInt(color.slice(1), 16);
        return {
          color: {
            r: (n >> 16) & 0xff,
            g: (n >> 8) & 0xff,
            b: n & 0xff,
          },
        };
      }
      case "media":
        return { media: { path: mediaPath } };
      case "text":
        return { text: { text } };
      case "camera":
        return { camera: { device: cameraDevice || null } };
      case "screen":
        return { screen: {} };
      default:
        return { test_pattern: {} };
    }
  };

  const add = () => {
    if (!scene) return;
    const label = name.trim() || kindLabel(kind);
    void sourceAdd(scene.id, buildKind(), label);
    setName("");
    setText("");
    setMediaPath("");
  };

  const move = (id: string, dir: -1 | 1) => {
    if (!scene) return;
    const order = sources.map((s) => s.id);
    const i = order.indexOf(id);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= order.length) return;
    [order[i], order[j]] = [order[j], order[i]];
    void sourceReorder(scene.id, order);
  };

  return (
    <div className="panel sources-panel">
      <div className="panel-head">
        <span className="panel-title">
          Sources{scene ? ` — ${scene.name}` : ""}
        </span>
      </div>
      <div className="panel-body">
        {!scene && (
          <div className="note">Select a preview scene to see its sources.</div>
        )}
        {scene &&
          sources.map((src) => (
            <div key={src.id} className={"source-row" + (src.visible ? "" : " hidden-src")}>
              <button
                className="icon-btn eye"
                title={src.visible ? "Hide" : "Show"}
                onClick={() =>
                  void sourceSetVisible(scene.id, src.id, !src.visible)
                }
              >
                {src.visible ? "👁" : "🚫"}
              </button>
              <span className="source-name">{src.name}</span>
              {isPendingCaptureSource(src) && (
                <span className="pending-badge" title="This source kind has no capture backend yet">
                  pending capture backend
                </span>
              )}
              <span className="source-actions">
                <button
                  className="icon-btn"
                  title="Move up"
                  onClick={() => move(src.id, -1)}
                >
                  ↑
                </button>
                <button
                  className="icon-btn"
                  title="Move down"
                  onClick={() => move(src.id, 1)}
                >
                  ↓
                </button>
                <button
                  className="icon-btn danger"
                  title="Remove source"
                  onClick={() => void sourceRemove(scene.id, src.id)}
                >
                  ✕
                </button>
              </span>
            </div>
          ))}
        {scene && sources.length === 0 && (
          <div className="note">No sources in this scene.</div>
        )}
      </div>
      {scene && (
        <div className="source-add">
          <select value={kind} onChange={(e) => setKind(e.target.value as KindKey)}>
            {KIND_OPTIONS.map((k) => (
              <option key={k} value={k}>
                {kindLabel(k)}
              </option>
            ))}
          </select>
          <input
            type="text"
            placeholder="Source name"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          {kind === "color" && (
            <input
              type="color"
              value={color}
              onChange={(e) => setColor(e.target.value)}
              title="Source color"
            />
          )}
          {kind === "text" && (
            <input
              type="text"
              placeholder="Text content"
              value={text}
              onChange={(e) => setText(e.target.value)}
            />
          )}
          {kind === "media" && (
            <input
              type="text"
              placeholder="Media path"
              value={mediaPath}
              onChange={(e) => setMediaPath(e.target.value)}
            />
          )}
          {kind === "camera" && (
            <select
              value={cameraDevice}
              onChange={(e) => setCameraDevice(e.target.value)}
              title="Camera device"
            >
              {cameras.length === 0 && (
                <option value="">No cameras found</option>
              )}
              {cameras.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
          )}
          <button className="btn ghost" onClick={add}>
            Add
          </button>
          {(kind === "screen") && (
            <div className="note">
              Screen capture has no backend yet — it will show a
              "pending capture backend" badge.
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function kindLabel(k: string): string {
  switch (k) {
    case "test_pattern":
      return "Test Pattern";
    case "color":
      return "Color";
    case "camera":
      return "Camera";
    case "screen":
      return "Screen";
    case "media":
      return "Media";
    case "text":
      return "Text";
    default:
      return k;
  }
}
