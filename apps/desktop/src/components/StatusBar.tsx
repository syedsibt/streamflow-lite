import { useEffect } from "react";
import { useProduction } from "../lib/productionStore";
import type { PerfSample } from "../lib/types";

const pct = (v: number | null | undefined) => (v == null ? "Unavailable" : `${Math.round(v)}%`);

/** Tiny line chart of the last two minutes of real samples. */
export function Spark({ data, pick, max, cls }: { data: PerfSample[]; pick: (s: PerfSample) => number | null; max: number; cls: string }) {
  const w = 72;
  const h = 16;
  const pts = data
    .map((s, i) => {
      const v = pick(s);
      if (v == null) return null;
      const x = data.length > 1 ? (i / (data.length - 1)) * w : w;
      const y = h - Math.min(1, Math.max(0, v / max)) * (h - 1) - 0.5;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .filter(Boolean);
  return (
    <svg className={"spark " + cls} width={w} height={h} viewBox={`0 0 ${w} ${h}`} aria-hidden="true">
      {pts.length > 1 && <polyline points={pts.join(" ")} fill="none" strokeWidth="1.2" />}
    </svg>
  );
}

/** Always-visible health line (measured values; "Unavailable" when the OS can't report one). */
export default function StatusBar({ onOpen }: { onOpen: () => void }) {
  const d = useProduction((s) => s.diag);
  const st = useProduction((s) => s.state);
  const perf = useProduction((s) => s.perf);
  const poll = useProduction((s) => s.pollDiagnostics);

  useEffect(() => {
    void poll();
    const t = setInterval(() => {
      if (!document.hidden) void poll();
    }, 1000);
    return () => clearInterval(t);
  }, [poll]);

  const sys = d?.system;
  const dropped = d ? d.lagged_ticks + d.skipped_render + d.dropped_network : 0;
  const lat = d?.latency.app_total_ms;
  const camWarn = d?.sources.find((s) => s.error || (s.width > 0 && s.kind === "camera" && s.fps < d.target_fps * 0.8));
  const stream = d?.stream_status ?? "idle";
  const rec = st?.record.state ?? "idle";
  const internal = d?.internal_errors ?? 0;

  return (
    <footer className="statusbar">
      <button type="button" className="sb" onClick={onOpen} title={sys ? `Whole computer ${pct(sys.system_cpu_percent)} · StreamFlow ${pct(sys.cpu_percent)} of ${sys.cores} cores` : ""}>
        CPU <b>{pct(sys?.system_cpu_percent)}</b>
        <Spark data={perf} pick={(s) => s.cpu} max={100} cls="cpu" />
      </button>
      <button type="button" className="sb" onClick={onOpen} title={sys?.gpu_note ?? (sys?.gpu_encode_percent != null ? `Video encoder engine ${Math.round(sys.gpu_encode_percent)}%` : "")}>
        GPU <b>{pct(sys?.gpu_percent)}</b>
        <Spark data={perf} pick={(s) => s.gpu} max={100} cls="gpu" />
      </button>
      <span className="sb" title={sys ? `StreamFlow uses ${Math.round(sys.ram_mb)} MB` : ""}>
        Memory <b>{sys?.mem_used_mb != null && sys.mem_total_mb ? `${(sys.mem_used_mb / 1024).toFixed(1)} / ${(sys.mem_total_mb / 1024).toFixed(1)} GB` : "Unavailable"}</b>
      </span>
      <button type="button" className={"sb" + (d && d.render_fps < d.target_fps * 0.9 ? " warn" : "")} onClick={onOpen}>
        FPS <b>{d ? `${Math.round(d.render_fps)}/${d.target_fps}` : "—"}</b>
        <Spark data={perf} pick={(s) => s.fps} max={d?.target_fps ?? 60} cls="fps" />
      </button>
      <button type="button" className={"sb" + (dropped ? " warn" : "")} onClick={onOpen} title={d ? `Late frames ${d.lagged_ticks} · encode failures ${d.skipped_render} · network drops ${d.dropped_network}` : ""}>
        Dropped <b>{d ? dropped : "—"}</b>
      </button>
      <span className={"sb" + (lat != null && lat > 400 ? " warn" : "")} title="App side: capture + processing + send buffer (the platform adds its own delay)">
        Latency <b>{lat != null ? `${Math.round(lat)} ms` : "—"}</b>
      </span>
      <span className="sb">
        Bitrate <b>{stream === "live" && d ? `${Math.round(d.stream_kbps)} kbps` : "—"}</b>
      </span>
      <span className={"sb tally-s " + stream}>Stream <b>{stream === "live" ? "Live" : stream === "error" ? "Error" : stream === "idle" ? "Off" : stream}</b></span>
      <span className={"sb tally-s " + (rec === "recording" ? "rec" : "")}>Rec <b>{rec === "recording" ? "On" : rec === "paused" ? "Paused" : "Off"}</b></span>
      <span className="sb-grow" />
      {camWarn && (
        <button type="button" className="sb warn" onClick={onOpen} title={camWarn.error ?? undefined}>
          {camWarn.name}: {camWarn.error ? "error" : `${camWarn.fps.toFixed(0)} fps`}
        </button>
      )}
      {internal > 0 && (
        <button type="button" className="sb bad" onClick={onOpen} title={d?.last_internal_error ?? ""}>
          {internal} internal error{internal > 1 ? "s" : ""}
        </button>
      )}
      <button type="button" className="sb link" onClick={onOpen}>
        Diagnostics
      </button>
    </footer>
  );
}
