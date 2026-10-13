import { useEffect } from "react";
import { useProduction } from "../lib/productionStore";
import { Section, Row, Dot } from "./ui";

const ms = (v: number | null | undefined) => (v == null ? "—" : `${v < 10 ? v.toFixed(1) : Math.round(v)} ms`);
const fmtUptime = (s: number) => {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
};

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
              {d.sources.map((s) => (
                <Row
                  key={s.id}
                  icon={<Dot tone={s.error ? "bad" : s.fps > 0 ? "ok" : "warn"} />}
                  title={s.name}
                  subtitle={
                    s.error ??
                    [
                      s.width ? `${s.width}×${s.height}` : null,
                      s.format,
                      `${Math.round(s.fps)} fps in`,
                      s.device_fps != null ? `device ${Math.round(s.device_fps)} fps` : null,
                      s.dropped ? `${s.dropped} older frames skipped` : null,
                    ]
                      .filter(Boolean)
                      .join(" · ")
                  }
                  trailing={<span className="mono">{ms(s.age_ms)}</span>}
                />
              ))}
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

            <Section title="System">
              <Row title="CPU (this app)" subtitle={`${d.system.cores} logical cores = 100 %`} trailing={<span className="mono">{d.system.cpu_percent.toFixed(0)} %</span>} />
              <Row title="Memory" trailing={<span className="mono">{Math.round(d.system.ram_mb)} MB</span>} />
              <Row title="GPU color time" subtitle="Per frame, all sources" trailing={<span className="mono">{ms(d.gpu_ms)}</span>} />
            </Section>
          </div>
        )}
      </aside>
    </div>
  );
}
