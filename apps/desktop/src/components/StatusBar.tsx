import { useEffect } from "react";
import { useProduction } from "../lib/productionStore";
import type { Diagnostics, ProductionStateData } from "../lib/types";
import { Dot, type Tone } from "./ui";

export interface StatusItem {
  key: string;
  label: string;
  value: string;
  tone: Tone;
  title: string;
}

const r0 = (v: number) => Math.round(v);

/** The seven always-visible indicators, computed from measured data only. */
export function statusItems(d: Diagnostics | null, st: ProductionStateData | null): StatusItem[] {
  if (!d || !st) {
    return ["VIDEO", "AUDIO", "COLOR", "FPS", "DROPPED", "LATENCY", "STREAM"].map((k) => ({
      key: k,
      label: k,
      value: "—",
      tone: "off" as Tone,
      title: "Waiting for the engine",
    }));
  }
  const vsrc = d.sources;
  const vErr = vsrc.find((s) => s.error);
  const vLive = vsrc.filter((s) => s.fps > 0).length;
  const video: StatusItem = vErr
    ? { key: "VIDEO", label: "VIDEO", value: "Error", tone: "bad", title: `${vErr.name}: ${vErr.error}` }
    : vsrc.length === 0
      ? { key: "VIDEO", label: "VIDEO", value: "None", tone: "off", title: "No camera, screen or media source" }
      : vLive > 0
        ? { key: "VIDEO", label: "VIDEO", value: "OK", tone: "ok", title: `${vLive} source(s) delivering frames` }
        : { key: "VIDEO", label: "VIDEO", value: "Waiting", tone: "warn", title: "Sources added, no frames yet" };

  const aErr = d.audio.find((a) => a.device.startsWith("error"));
  const audioCount = st.mixer.channels.length;
  const audio: StatusItem = aErr
    ? { key: "AUDIO", label: "AUDIO", value: "Error", tone: "bad", title: `${aErr.name}: ${aErr.device}` }
    : audioCount === 0 && d.audio.length === 0
      ? { key: "AUDIO", label: "AUDIO", value: "None", tone: "off", title: "No microphone or audio source" }
      : st.mixer.master.mute
        ? { key: "AUDIO", label: "AUDIO", value: "Muted", tone: "warn", title: "Master is muted" }
        : { key: "AUDIO", label: "AUDIO", value: "OK", tone: "ok", title: "Audio is flowing to the output" };

  const color: StatusItem = {
    key: "COLOR",
    label: "COLOR",
    value: d.color_active ? "On" : "Off",
    tone: d.color_active ? "ok" : "off",
    title: d.color_active ? "A grade, LUT or filter is active on the output" : "No color processing",
  };

  const ratio = d.target_fps > 0 ? d.render_fps / d.target_fps : 0;
  const fps: StatusItem = {
    key: "FPS",
    label: "FPS",
    value: `${r0(d.render_fps)}`,
    tone: ratio >= 0.95 ? "ok" : ratio >= 0.8 ? "warn" : "bad",
    title: `Output ${r0(d.render_fps)} of ${d.target_fps} fps · last frame ${d.tick_ms.toFixed(1)} ms`,
  };

  const dropped = d.lagged_ticks + d.skipped_render + d.dropped_network;
  const drop: StatusItem = {
    key: "DROPPED",
    label: "DROPPED",
    value: `${dropped}`,
    tone: dropped === 0 ? "ok" : "warn",
    title: `Late frames (PC too slow): ${d.lagged_ticks} · encode failures: ${d.skipped_render} · network drops: ${d.dropped_network}`,
  };

  const lat = d.latency.app_total_ms;
  const latency: StatusItem = {
    key: "LATENCY",
    label: "LATENCY",
    value: lat == null ? "—" : `${r0(lat)} ms`,
    tone: lat == null ? "off" : lat < 250 ? "ok" : lat < 600 ? "warn" : "bad",
    title:
      lat == null
        ? "No live picture to measure"
        : `App side: capture ${r0(d.latency.capture_ms ?? 0)} ms + processing ${r0(d.latency.processing_ms)} ms + send buffer ${r0(d.latency.send_buffer_ms)} ms. The platform adds its own delay.`,
  };

  const s = d.stream_status;
  const stream: StatusItem =
    s === "live"
      ? { key: "STREAM", label: "STREAM", value: `${r0(d.stream_kbps)} kbps`, tone: "live", title: "Live" }
      : s === "connecting" || s === "reconnecting"
        ? { key: "STREAM", label: "STREAM", value: s === "connecting" ? "Connecting" : "Reconnecting", tone: "warn", title: s }
        : s === "error"
          ? { key: "STREAM", label: "STREAM", value: "Error", tone: "bad", title: d.stream_error ?? "Stream error" }
          : { key: "STREAM", label: "STREAM", value: "Off", tone: "off", title: "Not streaming" };

  return [video, audio, color, fps, drop, latency, stream];
}

/** Bottom status strip; click (or Enter) opens the diagnostics sheet. */
export default function StatusBar({ onOpen }: { onOpen: () => void }) {
  const diag = useProduction((s) => s.diag);
  const state = useProduction((s) => s.state);
  const pollDiagnostics = useProduction((s) => s.pollDiagnostics);

  useEffect(() => {
    void pollDiagnostics();
    const t = setInterval(() => {
      if (!document.hidden) void pollDiagnostics();
    }, 1000);
    return () => clearInterval(t);
  }, [pollDiagnostics]);

  const items = statusItems(diag, state);
  return (
    <button type="button" className="statusbar" onClick={onOpen} title="Open diagnostics">
      {items.map((it) => (
        <span key={it.key} className="status-item" title={it.title}>
          <Dot tone={it.tone} />
          <span className="status-label">{it.label}</span>
          <span className="status-value">{it.value}</span>
        </span>
      ))}
      <span className="status-more">Diagnostics</span>
    </button>
  );
}
