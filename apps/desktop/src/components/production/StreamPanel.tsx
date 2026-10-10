import { useProduction } from "../../lib/productionStore";

const PLATFORM_PRESETS: Record<string, string> = {
  YouTube: "rtmp://a.rtmp.youtube.com/live2",
  Twitch: "rtmp://live.twitch.tv/app",
  Custom: "",
};

function pillClass(state: string): string {
  switch (state.toLowerCase()) {
    case "live":
    case "streaming":
      return "status-pill live";
    case "connecting":
      return "status-pill connecting";
    case "error":
      return "status-pill error";
    default:
      return "status-pill idle";
  }
}

/**
 * Stream panel. Honest statuses only: the "LIVE" badge appears only when the
 * backend reports it. Start currently fails with the backend's "not
 * implemented" message, which surfaces as an error pill + toast. The stream
 * key is never displayed back (password input, no echo).
 */
export default function StreamPanel() {
  const stream = useProduction((s) => s.state?.stream ?? null);
  const streamUrl = useProduction((s) => s.streamUrl);
  const setStreamUrl = useProduction((s) => s.setStreamUrl);
  const streamPlatform = useProduction((s) => s.streamPlatform);
  const setStreamPlatform = useProduction((s) => s.setStreamPlatform);
  const streamKey = useProduction((s) => s.streamKey);
  const setStreamKey = useProduction((s) => s.setStreamKey);
  const streamError = useProduction((s) => s.streamError);
  const clearStreamError = useProduction((s) => s.clearStreamError);
  const streamSaveSettings = useProduction((s) => s.streamSaveSettings);
  const streamStart = useProduction((s) => s.streamStart);
  const streamStop = useProduction((s) => s.streamStop);

  const state = stream?.state ?? "idle";
  const savedUrl = stream?.settings.url ?? "";

  const setPlatform = (platform: string) => {
    setStreamPlatform(platform);
    const preset = PLATFORM_PRESETS[platform];
    if (preset !== undefined) setStreamUrl(preset);
  };

  return (
    <div className="panel">
      <div className="panel-head">
        <span className="panel-title">Stream</span>
        <span className={pillClass(state)}>{state}</span>
      </div>
      <div className="panel-body">
        {streamError && (
          <div className="error-pill">
            {streamError}
            <button
              className="icon-btn"
              onClick={clearStreamError}
              title="Dismiss"
            >
              ✕
            </button>
          </div>
        )}
        {savedUrl && (
          <div className="note" style={{ marginBottom: 8 }}>
            Saved URL: <span className="mono">{savedUrl}</span> · platform:{" "}
            {stream?.settings.platform}
          </div>
        )}
        <div className="form-grid">
          <label>Platform</label>
          <select
            value={streamPlatform}
            onChange={(e) => setPlatform(e.target.value)}
          >
            {Object.keys(PLATFORM_PRESETS).map((p) => (
              <option key={p} value={p}>
                {p}
              </option>
            ))}
          </select>
          <label>RTMP URL</label>
          <input
            type="text"
            value={streamUrl}
            onChange={(e) => setStreamUrl(e.target.value)}
            placeholder="rtmp://…"
            className="mono"
          />
          <label>Stream key</label>
          <input
            type="password"
            value={streamKey}
            onChange={(e) => setStreamKey(e.target.value)}
            placeholder="••••••••"
            autoComplete="off"
          />
        </div>
        <div className="btn-row" style={{ marginTop: 10, marginBottom: 0 }}>
          <button className="btn ghost" onClick={() => void streamSaveSettings()}>
            Save
          </button>
          <button className="btn" onClick={() => void streamStart()}>
            Start
          </button>
          <button className="btn ghost" onClick={() => void streamStop()}>
            Stop
          </button>
        </div>
        <div className="note" style={{ marginTop: 8 }}>
          Stream output is not implemented in the backend yet — starting will
          report the backend's error. The stream key is never echoed back.
        </div>
      </div>
    </div>
  );
}
