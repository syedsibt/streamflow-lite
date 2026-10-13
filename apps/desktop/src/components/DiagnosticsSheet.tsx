import { useEffect } from "react";
import { useProduction } from "../lib/productionStore";
import { Section, Row, Dot } from "./ui";
import type { SourceDiag } from "../lib/types";

const ms = (v: number | null | undefined) => (v == null ? "—" : `${v < 10 ? v.toFixed(1) : Math.round(v)} ms`);
const fmtUptime = (s: number) => {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
};

const pctv = (v: number | null | undefined) => (v == null ? "Unavailable" : `${Math.round(v)} %`);

/** Last two minutes of real samples: CPU (whole computer), GPU, output fps. */
function PerfChart() {
  const perf = useProduction((s) => s.perf);
  const target = useProduction((s) => s.diag?.target_fps ?? 30);
  const w = 500;
  const h = 90;
  const line = (pick: (p: (typeof perf)[number]) => number | null, max: number) =>
    perf
      .map((p, i) => {
        const v = pick(p);
        if (v == null) return null;
        const x = perf.length > 1 ? (i / 119) * w : 0;
        return `${x.toFixed(1)},${(h - Math.min(1, v / max) * (h - 2) - 1).toFixed(1)}`;
      })
      .filter(Boolean)
      .join(" ");
  return (
    <div className="perf-chart">
      <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" role="img" aria-label="CPU, GPU and frame rate over the last two minutes">
        {[0.25, 0.5, 0.75].map((f) => (
          <line key={f} x1="0" x2={w} y1={h * f} y2={h * f} className="grid" />
        ))}
        <polyline points={line((p) => p.cpu, 100)} className="cpu" />
        <polyline points={line((p) => p.gpu, 100)} className="gpu" />
        <polyline points={line((p) => p.fps, target)} className="fps" />
      </svg>
      <div className="perf-legend">
        <span className="cpu">CPU %</span>
        <span className="gpu">GPU %</span>
        <span className="fps">FPS (of {target})</span>
        <span className="dim">last {perf.length} s</span>
      </div>
    </div>
  );
}

/** Stage bar: proportional to the frame budget. */
function Stage({ label, value, budget }: { label: string; value: number; budget: number }) {
  const pct = Math.min(100, (value / Math.max(budget, 1)) * 100);
  const tone = value > budget ? "bad" : value > budget * 0.6 ? "warn" : "ok";
  return (
    <div className="stage">
      <span className="stage-label">{label}</span>
      <span className="stage-track">
        <span className={`stage-fill ${tone}`} style={{ width: `${pct}%` }} />
      </span>
      <span className="stage-value">{ms(value)}</span>
    </div>
  );
}

/**
 * Diagnostics sheet: every number here is measured in the running
 * pipeline. Latency is split into what the app adds (measured) and what
 * the platform adds (outside the app's control).
 */
export default function DiagnosticsSheet({ open, onClose }: { open: boolean; onClose: () => void }) {
  const d = useProduction((s) => s.diag);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;
  const budget = d ? 1000 / Math.max(d.target_fps, 1) : 33.3;

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <aside className="sheet" role="dialog" aria-label="Diagnostics" onClick={(e) => e.stopPropagation()}>
        <header className="sheet-head">
          <h2>Diagnostics</h2>
          <button type="button" className="text-btn" onClick={onClose}>
            Done
          </button>
        </header>
        {!d ? (
          <div className="sheet-body">
            <div className="empty">
              <div className="empty-title">Waiting for the engine…</div>
            </div>
          </div>
        ) : (
          <div className="sheet-body">
            <Section
              title="Latency"
              footer={
                <>
                  App side is measured from each frame's capture timestamp to the network.{" "}
                  {d.latency.platform_note}
                </>
              }
            >
              <Row title="Capture → render" subtitle="Device timestamp to the frame being drawn" trailing={<span className="mono">{ms(d.latency.capture_ms)}</span>} />
              <Row title="Processing" subtitle="Render + color + encode (last frame)" trailing={<span className="mono">{ms(d.latency.processing_ms)}</span>} />
              <Row title="Send buffer" subtitle="Encoded video waiting for the network" trailing={<span className="mono">{ms(d.latency.send_buffer_ms)}</span>} />
              <Row title="App total" trailing={<strong className="mono">{ms(d.latency.app_total_ms)}</strong>} />
            </Section>

            <Section title={`Frame pipeline · budget ${budget.toFixed(1)} ms`}>
              <div className="stages">
                <Stage label="Capture" value={d.capture_ms} budget={budget} />
                <Stage label="Render" value={d.render_ms} budget={budget} />
                <Stage label="  GPU color" value={d.gpu_ms} budget={budget} />
                <Stage label="Audio" value={d.audio_ms} budget={budget} />
                <Stage label="Encode + send" value={d.encode_ms} budget={budget} />
                <Stage label="Whole frame" value={d.tick_ms} budget={budget} />
              </div>
            </Section>

            <Section title="Frames">
              <Row title="Output frame rate" trailing={<span className="mono">{Math.round(d.render_fps)} / {d.target_fps} fps</span>} />
              <Row title="Late frames" subtitle="Ticks the PC could not finish in time (skipped, never queued)" trailing={<span className="mono">{d.lagged_ticks}</span>} />
              <Row title="Encode failures" trailing={<span className="mono">{d.skipped_render}</span>} />
              <Row title="Network drops" subtitle="Video shed because the upload could not keep up" trailing={<span className="mono">{d.dropped_network}</span>} />
            </Section>

            <Section title="Video sources">
              {d.sources.length === 0 && <Row title="No video sources" dim />}
              {d.sources.map((s) => {
                const live = s.device_packet_fps != null;
                const f1 = (v: number | null) => (v == null ? "—" : v.toFixed(1));
                return (
                  <div key={s.id}>
                    <Row
                      icon={<Dot tone={s.error ? "bad" : s.fps >= d.target_fps * 0.8 ? "ok" : "warn"} />}
                      title={s.name}
                      subtitle={
                        s.error ??
                        [s.width ? `${s.width}×${s.height}` : null, s.format?.split(" — ")[0]].filter(Boolean).join(" · ")
                      }
                      trailing={<span className="mono">{ms(s.age_ms)}</span>}
                    />
                    {live && (
                      <div className="diag-grid mono">
                        <span>Camera sends</span><span>{f1(s.device_packet_fps)} fps</span>
                        <span>Captured</span><span>{f1(s.device_fps)} fps</span>
                        <span>Reaching program</span><span>{f1(s.fps)} fps</span>
                        <span>Decode</span><span>{f1(s.decode_ms)} ms</span>
                        <span>Convert</span><span>{f1(s.convert_ms)} ms</span>
                        <span>Camera → capture</span><span>{f1(s.capture_latency_ms)} ms</span>
                        <span>Longest gap (1 s)</span><span>{f1(s.max_gap_ms)} ms</span>
                        <span>Skipped (newer frame)</span><span>{s.dropped}</span>
                        <span>Decode / read errors</span><span>{s.decode_errors} / {s.capture_errors}</span>
                      </div>
                    )}
                    {live && <div className="diag-note">{cameraVerdict(s, d.target_fps)}</div>}
                    {s.format?.includes(" — ") && <div className="diag-note">{s.format.split(" — ")[1]}</div>}
                  </div>
                );
              })}
            </Section>

            <Section
              title="Audio / A·V sync"
              footer={d.av_auto ? `Automatic sync is on (offset ${d.av_offset_ms > 0 ? "+" : ""}${d.av_offset_ms} ms).` : `Automatic sync is off — fixed audio delay ${d.av_offset_ms} ms.`}
            >
              {d.audio.length === 0 && <Row title="No live audio inputs" dim />}
              {d.audio.map((a) => (
                <Row
                  key={a.id}
                  icon={<Dot tone={a.device.startsWith("error") ? "bad" : a.device === "running" ? "ok" : "warn"} />}
                  title={a.name}
                  subtitle={[
                    a.device.startsWith("error") ? a.device : null,
                    `buffer ${ms(a.buffered_ms)}`,
                    a.underruns ? `${a.underruns} gaps` : null,
                    a.corrections ? `${a.corrections} sync corrections` : null,
                  ]
                    .filter(Boolean)
                    .join(" · ")}
                  trailing={
                    <span className="mono" title="Audio age as played / target (picture age + offset)">
                      {ms(a.audio_age_ms)}
                      {a.target_age_ms != null ? ` → ${ms(a.target_age_ms)}` : ""}
                    </span>
                  }
                />
              ))}
            </Section>

            <Section title="Encoder & stream">
              {d.encoder ? (
                <>
                  <Row
                    title={d.encoder.name}
                    subtitle={(d.encoder.hardware ? "Hardware encoder" : "Software encoder (CPU)") + (d.encoder.note ? ` · ${d.encoder.note}` : "")}
                    trailing={<span className="mono">{ms(d.encoder.encode_ms)}</span>}
                  />
                  <Row title="Encoded frame rate" trailing={<span className="mono">{Math.round(d.encoder.fps)} fps</span>} />
                </>
              ) : (
                <Row title="Encoder idle" subtitle="Starts with streaming or recording" dim />
              )}
              <Row
                title="Stream"
                subtitle={d.stream_error ?? (d.stream_status === "live" ? `Live for ${fmtUptime(d.stream_uptime_s)}` : null)}
                trailing={<span className="mono">{d.stream_status === "live" ? `${Math.round(d.stream_kbps)} kbps` : d.stream_status}</span>}
              />
              <Row title="Recording" trailing={<span className="mono">{d.recording ? "on" : "off"}</span>} />
            </Section>

            <Section title="System" footer={d.system.gpu_note ?? "GPU from Windows performance counters (same source as Task Manager)."}>
              <PerfChart />
              <Row title="CPU, whole computer" trailing={<span className="mono">{pctv(d.system.system_cpu_percent)}</span>} />
              <Row title="CPU, StreamFlow" subtitle={`${d.system.cores} logical cores = 100 %`} trailing={<span className="mono">{d.system.cpu_percent.toFixed(0)} %</span>} />
              <Row title="GPU, busiest engine" trailing={<span className="mono">{pctv(d.system.gpu_percent)}</span>} />
              <Row title="GPU, StreamFlow" trailing={<span className="mono">{pctv(d.system.gpu_app_percent)}</span>} />
              <Row title="GPU video encoder" subtitle="NVENC / Quick Sync / AMF engine" trailing={<span className="mono">{pctv(d.system.gpu_encode_percent)}</span>} />
              <Row title="Memory in use" trailing={<span className="mono">{d.system.mem_used_mb != null && d.system.mem_total_mb ? `${(d.system.mem_used_mb / 1024).toFixed(1)} of ${(d.system.mem_total_mb / 1024).toFixed(1)} GB` : "Unavailable"}</span>} />
              <Row title="Memory, StreamFlow" trailing={<span className="mono">{Math.round(d.system.ram_mb)} MB</span>} />
              <Row title="Graphics adapter" subtitle={d.gpu_adapter?.includes("Cpu") ? "Software rendering — expect low frame rates" : undefined} trailing={<span className="mono">{d.gpu_adapter ?? "Unknown"}</span>} />
            </Section>

            <Section title="Errors" footer="Internal errors are contained and logged to %LOCALAPPDATA%\StreamFlow\streamflow-errors.log; the app keeps running.">
              <Row title="Internal errors" subtitle={d.last_internal_error} trailing={<span className="mono">{d.internal_errors}</span>} />
              <Row title="GPU errors" subtitle={d.last_gpu_error} trailing={<span className="mono">{d.gpu_errors}</span>} />
            </Section>
          </div>
        )}
      </aside>
    </div>
  );
}

/** Where a slow camera loses frames, from the measured stage rates. */
function cameraVerdict(s: SourceDiag, target: number): string {
  const nominal = Number(/@(\d+(?:\.\d+)?)fps/.exec(s.format ?? "")?.[1] ?? 0) || target;
  const sends = s.device_packet_fps ?? 0;
  const captured = s.device_fps ?? 0;
  const want = Math.min(nominal, target);
  if (sends < nominal * 0.8)
    return `The camera itself sends only ${sends.toFixed(1)} of ${nominal} fps — limited before the app (mode, USB bandwidth, or auto‑exposure in low light).`;
  if (captured < Math.min(sends, want) * 0.8)
    return `Camera sends ${sends.toFixed(0)} fps but capture keeps ${captured.toFixed(1)} — decode/convert (${((s.decode_ms ?? 0) + (s.convert_ms ?? 0)).toFixed(1)} ms) or read errors limit it.`;
  if (s.fps < want * 0.8)
    return `Capture is fine (${captured.toFixed(0)} fps) but only ${s.fps.toFixed(1)} fps reach the program — the render loop is late.`;
  return "Camera, capture and program are all at full rate.";
}
