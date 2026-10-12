import { useProduction } from "../lib/productionStore";
import { useStudio } from "../lib/store";
import { api } from "../lib/ipc";

export type ViewId = "production" | "studio" | "video" | "audio";

interface TopBarProps {
  view: ViewId;
  setView: (v: ViewId) => void;
}

/**
 * Slim top-level view switch: Production | Color Studio | Video,
 * plus an always-visible FPS control. FPS applies in real time:
 * the program tick follows immediately, and live stream/record
 * encoders are rebuilt on the fly (continuous timestamps).
 */
export default function TopBar({ view, setView }: TopBarProps) {
  const fps = useProduction((s) => s.state?.video_settings.output_fps ?? 30);
  const refresh = useProduction((s) => s.refresh);

  const setFps = async (next: number) => {
    if (next === fps) return;
    try {
      const cur = await api.videoSettingsGet();
      await api.videoSettingsSet({ ...cur, output_fps: next });
      await refresh();
    } catch (e) {
      useStudio.getState().pushToast(`fps change failed: ${String(e)}`);
    }
  };

  return (
    <div className="viewbar">
      <div className="viewbar-title">StreamFlow Lite</div>
      <div className="viewbar-tabs">
        <button
          className={view === "production" ? "active" : ""}
          onClick={() => setView("production")}
        >
          Production
        </button>
        <button
          className={view === "studio" ? "active" : ""}
          onClick={() => setView("studio")}
        >
          Color Studio
        </button>
        <button
          className={view === "video" ? "active" : ""}
          onClick={() => setView("video")}
        >
          Video
        </button>
        <button
          className={view === "audio" ? "active" : ""}
          onClick={() => setView("audio")}
        >
          Audio
        </button>
      </div>
      <div className="viewbar-fps" title="Output frame rate — applies in real time">
        <span>FPS</span>
        {[30, 60].map((f) => (
          <button
            key={f}
            className={fps === f ? "active" : ""}
            onClick={() => void setFps(f)}
          >
            {f}
          </button>
        ))}
      </div>
    </div>
  );
}
