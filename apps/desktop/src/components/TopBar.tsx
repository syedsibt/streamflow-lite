import { useState } from "react";
import { useProduction } from "../lib/productionStore";
import ErrorBoundary from "./ErrorBoundary";
import { Glyph } from "./ui";
import VideoInputDialog from "./VideoInput";

export type StepId = "production" | "color";

const fmtTime = (s: number) => {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = Math.floor(s % 60);
  return `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
};
const pct = (v: number | null | undefined) => (v == null ? "n/a" : `${Math.round(v)}%`);

/** Product, workspace switch, the two on-air controls, and live load. */
export default function TopBar({ step, setStep, onDiagnostics }: { step: StepId; setStep: (s: StepId) => void; onDiagnostics: () => void }) {
  const stream = useProduction((s) => s.state?.stream.state ?? "idle");
  const rec = useProduction((s) => s.state?.record.state ?? "idle");
  const recSecs = useProduction((s) => s.state?.record.elapsed_secs ?? 0);
  const diag = useProduction((s) => s.diag);
  const busyStream = useProduction((s) => !!s.busy.stream);
  const streamStart = useProduction((s) => s.streamStart);
  const streamStop = useProduction((s) => s.streamStop);
  const recordStart = useProduction((s) => s.recordStart);
  const recordStop = useProduction((s) => s.recordStop);
  const live = stream === "live";
  const connecting = stream === "connecting" || stream === "reconnecting";
  const recOn = rec === "recording" || rec === "paused";
  const sys = diag?.system;
  const fpsLow = diag ? diag.render_fps < diag.target_fps * 0.9 : false;
  const [videoInput, setVideoInput] = useState(false);
  const cams = useProduction((s) => s.state?.scenes.some((sc) => sc.sources.some((x) => x.kind === "camera")) ?? false);

  return (
    <header className="topbar">
      <div className="brand">StreamFlow</div>
      <nav className="views" role="tablist" aria-label="Workspace">
        {(
          [
            ["production", "Production"],
            ["color", "Color grading"],
          ] as [StepId, string][]
        ).map(([id, label]) => (
          <button key={id} type="button" role="tab" aria-selected={step === id} className={step === id ? "on" : ""} onClick={() => setStep(id)}>
            {label}
          </button>
        ))}
      </nav>
      <button type="button" className={"vi-btn" + (cams ? "" : " dim")} onClick={() => setVideoInput(true)} title="Video input: what your camera / capture card captures (size, frame rate, format) — separate from the stream output">
        <Glyph name="video" size={15} />
        Video input
      </button>
      {videoInput && (
        <ErrorBoundary label="Video input">
          <VideoInputDialog onClose={() => setVideoInput(false)} />
        </ErrorBoundary>
      )}
      <div className="onair">
        <button
          type="button"
          className={"air stream" + (live ? " on" : connecting ? " pending" : stream === "error" ? " err" : "")}
          disabled={busyStream}
          onClick={() => void (live || connecting ? streamStop() : streamStart())}
          title={live ? "End the stream" : "Start streaming to the destination set in Output"}
        >
          <i />
          {live ? `Live ${diag ? fmtTime(diag.stream_uptime_s) : ""}` : connecting ? (stream === "connecting" ? "Connecting…" : "Reconnecting…") : busyStream ? "Starting…" : stream === "error" ? "Stream failed — retry" : "Go live"}
        </button>
        <button
          type="button"
          className={"air rec" + (rec === "recording" ? " on" : rec === "paused" ? " pending" : "")}
          onClick={() => void (recOn ? recordStop() : recordStart())}
          title={recOn ? "Stop recording" : "Start recording (file set in Output → Record)"}
        >
          <i />
          {recOn ? `${rec === "paused" ? "Paused" : "Rec"} ${fmtTime(recSecs)}` : "Record"}
        </button>
      </div>
      <button type="button" className="loads" onClick={onDiagnostics} title="Open diagnostics">
        <span>
          CPU <b>{pct(sys?.system_cpu_percent)}</b>
        </span>
        <span title={sys?.gpu_note ?? undefined}>
          GPU <b>{pct(sys?.gpu_percent)}</b>
        </span>
        <span className={fpsLow ? "warn" : ""}>
          FPS <b>{diag ? Math.round(diag.render_fps) : "—"}</b>
        </span>
      </button>
    </header>
  );
}
