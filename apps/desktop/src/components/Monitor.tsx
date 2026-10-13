import { useEffect } from "react";
import { useProduction } from "../lib/productionStore";

/**
 * The program monitor: exactly the frame that goes to the encoder (same
 * grade, same composite — rendered once per tick, downscaled for display).
 *
 * Polls `monitor_frame_if_new` at display cadence with an in-flight guard:
 * an unchanged frame costs one tiny IPC call and no image transfer, and a
 * slow backend can never build a request backlog.
 */
export function useMonitorPolling() {
  const pollMonitor = useProduction((s) => s.pollMonitor);
  useEffect(() => {
    let stop = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const loop = async () => {
      if (stop) return;
      if (!document.hidden) await pollMonitor();
      // Under sustained load (engine missing frames) the monitor drops to
      // 15 fps first — preview smoothness gives way before the stream.
      const d = useProduction.getState().diag;
      const behind = d ? d.render_fps < d.target_fps * 0.85 : false;
      if (!stop) timer = setTimeout(() => void loop(), behind ? 66 : 33);
    };
    void loop();
    return () => {
      stop = true;
      if (timer) clearTimeout(timer);
    };
  }, [pollMonitor]);
}

export default function Monitor({ compact }: { compact?: boolean }) {
  const url = useProduction((s) => s.programUrl);
  const stream = useProduction((s) => s.state?.stream.state ?? "idle");
  const record = useProduction((s) => s.state?.record.state ?? "idle");
  const vs = useProduction((s) => s.state?.video_settings);
  const live = stream === "live";
  const recording = record === "recording";

  return (
    <div className={"monitor" + (compact ? " compact" : "")}>
      <div className="monitor-frame">
        {url ? (
          <img src={url} alt="Program output" draggable={false} />
        ) : (
          <div className="monitor-empty">
            <div className="monitor-empty-title">No picture yet</div>
            <div className="monitor-empty-hint">Add a camera, screen or media file in Input.</div>
          </div>
        )}
        <div className="monitor-chips">
          {live && <span className="chip live">LIVE</span>}
          {stream === "connecting" && <span className="chip warn">CONNECTING</span>}
          {stream === "reconnecting" && <span className="chip warn">RECONNECTING</span>}
          {recording && <span className="chip rec">REC</span>}
          {record === "paused" && <span className="chip">REC PAUSED</span>}
        </div>
        {vs && (
          <div className="monitor-format">
            {vs.output_height}p · {vs.output_fps} fps
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * The PREVIEW scene (studio mode: what CUT/AUTO will put on air). Rendered
 * on demand at monitor size, polled ~10 fps only while mounted.
 */
export function PreviewMonitor() {
  const url = useProduction((s) => s.previewUrl);
  const poll = useProduction((s) => s.pollPreviewMonitor);
  const pv = useProduction((s) => s.state?.preview_scene_id);
  useEffect(() => {
    let stop = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const loop = async () => {
      if (stop) return;
      if (!document.hidden) await poll();
      if (!stop) timer = setTimeout(() => void loop(), 100);
    };
    void loop();
    return () => {
      stop = true;
      if (timer) clearTimeout(timer);
    };
  }, [pv, poll]);
  return (
    <div className="monitor">
      <div className="monitor-frame">
        {url ? (
          <img src={url} alt="Preview scene" draggable={false} />
        ) : (
          <div className="monitor-empty">
            <div className="monitor-empty-title">Rendering preview…</div>
          </div>
        )}
        <div className="monitor-chips">
          <span className="chip pv">PREVIEW — not on air</span>
        </div>
      </div>
    </div>
  );
}
