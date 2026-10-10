import { useProduction } from "../../lib/productionStore";

function pillClass(state: string): string {
  switch (state.toLowerCase()) {
    case "recording":
      return "status-pill recording";
    case "paused":
      return "status-pill connecting";
    case "error":
      return "status-pill error";
    default:
      return "status-pill idle";
  }
}

function fmtElapsed(secs: number): string {
  const s = Math.floor(secs);
  const mm = Math.floor(s / 60);
  const ss = s % 60;
  return `${String(mm).padStart(2, "0")}:${String(ss).padStart(2, "0")}`;
}

/**
 * Record panel. Honest statuses only: a "recording" indicator appears only
 * when the backend reports it. Start currently fails with the backend's
 * "not implemented" message, surfaced as an error pill + toast.
 */
export default function RecordPanel() {
  const record = useProduction((s) => s.state?.record ?? null);
  const recordPath = useProduction((s) => s.recordPath);
  const setRecordPath = useProduction((s) => s.setRecordPath);
  const recordFormat = useProduction((s) => s.recordFormat);
  const setRecordFormat = useProduction((s) => s.setRecordFormat);
  const recordQuality = useProduction((s) => s.recordQuality);
  const setRecordQuality = useProduction((s) => s.setRecordQuality);
  const recordError = useProduction((s) => s.recordError);
  const clearRecordError = useProduction((s) => s.clearRecordError);
  const recordSaveSettings = useProduction((s) => s.recordSaveSettings);
  const recordStart = useProduction((s) => s.recordStart);
  const recordStop = useProduction((s) => s.recordStop);
  const recordPause = useProduction((s) => s.recordPause);
  const recordResume = useProduction((s) => s.recordResume);

  const state = record?.state ?? "idle";
  const elapsed = record?.elapsed_secs ?? 0;
  const isRecording = state.toLowerCase() === "recording";
  const isPaused = state.toLowerCase() === "paused";

  return (
    <div className="panel">
      <div className="panel-head">
        <span className="panel-title">Record</span>
        <span className={pillClass(state)}>{state}</span>
        {(isRecording || isPaused) && (
          <span className="elapsed mono">{fmtElapsed(elapsed)}</span>
        )}
      </div>
      <div className="panel-body">
        {recordError && (
          <div className="error-pill">
            {recordError}
            <button
              className="icon-btn"
              onClick={clearRecordError}
              title="Dismiss"
            >
              ✕
            </button>
          </div>
        )}
        <div className="form-grid">
          <label>Path</label>
          <input
            type="text"
            value={recordPath}
            onChange={(e) => setRecordPath(e.target.value)}
            placeholder="recording.mp4"
            className="mono"
          />
          <label>Format</label>
          <select
            value={recordFormat}
            onChange={(e) => setRecordFormat(e.target.value)}
          >
            <option value="mp4">MP4</option>
            <option value="mkv">MKV</option>
          </select>
          <label>Quality</label>
          <select
            value={recordQuality}
            onChange={(e) => setRecordQuality(e.target.value)}
          >
            <option value="low">Low</option>
            <option value="medium">Medium</option>
            <option value="high">High</option>
          </select>
        </div>
        <div className="btn-row" style={{ marginTop: 10, marginBottom: 0 }}>
          <button className="btn ghost" onClick={() => void recordSaveSettings()}>
            Save
          </button>
          {!isRecording && !isPaused && (
            <button className="btn" onClick={() => void recordStart()}>
              Start
            </button>
          )}
          {isRecording && (
            <button className="btn ghost" onClick={() => void recordPause()}>
              Pause
            </button>
          )}
          {isPaused && (
            <button className="btn" onClick={() => void recordResume()}>
              Resume
            </button>
          )}
          {(isRecording || isPaused) && (
            <button className="btn ghost" onClick={() => void recordStop()}>
              Stop
            </button>
          )}
        </div>
        <div className="note" style={{ marginTop: 8 }}>
          Recording output is not implemented in the backend yet — starting
          will report the backend's error.
        </div>
      </div>
    </div>
  );
}
