import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { useProduction } from "../../lib/productionStore";
import { useStudio } from "../../lib/store";
import { api } from "../../lib/ipc";
import { isPendingCaptureSource, type SourceKind } from "../../lib/types";

const CAMERA_RESOLUTIONS = [
  { id: "480p", label: "480p (854×480)", width: 854, height: 480 },
  { id: "720p", label: "720p (1280×720)", width: 1280, height: 720 },
  { id: "1080p", label: "1080p (1920×1080)", width: 1920, height: 1080 },
] as const;

const KIND_OPTIONS = [
  "test_pattern",
  "color",
  "camera",
  "screen",
  "media",
  "text",
  "audio_input",
] as const;
type KindKey = (typeof KIND_OPTIONS)[number];

/**
 * Shows the camera's actually-negotiated format (codec/resolution/fps) under
 * a camera source row. Fetched once per source; hidden when unavailable
 * (non-Windows or no negotiated format yet).
 */
function CameraFormatBadge({ sourceId }: { sourceId: string }) {
  const [fmt, setFmt] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    void api
      .cameraFormat(sourceId)
      .then((f) => {
        if (!cancelled) setFmt(f);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [sourceId]);
  if (!fmt) return null;
  // Backend appends " [via <attempt>]"; show the compact part in the badge,
  // full string in the tooltip.
  const short = fmt.split(" [")[0];
  return (
    <span
      className="camera-format"
      title={`Negotiated camera format — what the camera actually gave: ${fmt}`}
      style={{ whiteSpace: "nowrap" }}
    >
      {short}
    </span>
  );
}

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
  const [cameraRes, setCameraRes] = useState<string>("720p");
  const [cameraFps, setCameraFps] = useState<string>("30");
  const [audioDevices, setAudioDevices] = useState<string[]>([]);
  const [audioDevice, setAudioDevice] = useState<string>("");

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

  // Load audio input devices when the audio_input kind is selected.
  useEffect(() => {
    if (kind !== "audio_input") return;
    let cancelled = false;
    void api
      .listAudioDevices()
      .then((list) => {
        if (cancelled) return;
        setAudioDevices(list);
        setAudioDevice((prev) => prev || list[0] || "");
      })
      .catch(() => {
        if (!cancelled) setAudioDevices([]);
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
      case "camera": {
        const res =
          CAMERA_RESOLUTIONS.find((r) => r.id === cameraRes) ??
          CAMERA_RESOLUTIONS[1];
        return {
          camera: {
            device: cameraDevice || null,
            width: res.width,
            height: res.height,
            fps: cameraFps === "60" ? 60 : 30,
          },
        };
      }
      case "screen":
        return { screen: {} };
      case "audio_input":
        return { audio_input: { device: audioDevice || null } };
      default:
        return { test_pattern: {} };
    }
  };

  const add = () => {
    if (!scene) return;
    // Validate before hitting the backend: an empty media path used to
    // produce a cryptic "cannot open media ''" capture error.
    if (kind === "media" && !mediaPath.trim()) {
      useStudio.getState().pushToast("Please choose a media file first (Browse…)");
      return;
    }
    const label = name.trim() || kindLabel(kind);
    void sourceAdd(scene.id, buildKind(), label);
    setName("");
    setText("");
    setMediaPath("");
  };

  const browseMedia = async () => {
    try {
      const picked = await open({
        multiple: false,
        title: "Choose media file",
        filters: [
          {
            name: "Video",
            extensions: ["mp4", "mkv", "mov", "avi", "webm", "m4v"],
          },
          {
            name: "Audio",
            extensions: ["mp3", "wav", "aac", "m4a", "ogg", "flac"],
          },
          { name: "All files", extensions: ["*"] },
        ],
      });
      if (typeof picked === "string") setMediaPath(picked);
    } catch {
      /* dialog cancelled or unavailable — keep manual path entry */
    }
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
              {src.kind === "camera" && <CameraFormatBadge sourceId={src.id} />}
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
            <>
              <input
                type="text"
                placeholder="Media path"
                value={mediaPath}
                onChange={(e) => setMediaPath(e.target.value)}
                title="Media file path (or use Browse…)"
                style={{ flex: 1, minWidth: 0 }}
              />
              <button
                type="button"
                onClick={() => void browseMedia()}
                title="Choose a media file…"
              >
                Browse…
              </button>
            </>
          )}
          {kind === "camera" && (
            <>
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
              <select
                value={cameraRes}
                onChange={(e) => setCameraRes(e.target.value)}
                title="Capture resolution (lower = less lag)"
              >
                {CAMERA_RESOLUTIONS.map((r) => (
                  <option key={r.id} value={r.id}>
                    {r.label}
                  </option>
                ))}
              </select>
              <select
                value={cameraFps}
                onChange={(e) => setCameraFps(e.target.value)}
                title="Capture frame rate"
              >
                <option value="30">30 fps</option>
                <option value="60">60 fps</option>
              </select>
            </>
          )}
          {kind === "audio_input" && (
            <select
              value={audioDevice}
              onChange={(e) => setAudioDevice(e.target.value)}
              title="Microphone / line-in device"
            >
              {audioDevices.length === 0 && (
                <option value="">No audio inputs found</option>
              )}
              {audioDevices.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
          )}
          <button className="btn ghost" onClick={add}>
            Add
          </button>
          {(kind === "audio_input") && (
            <div className="note">
              Audio-only source: feeds the mixer and meters, no video.
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
    case "audio_input":
      return "Audio Input";
    default:
      return k;
  }
}
