import { useEffect, useState } from "react";
import { api } from "../lib/ipc";

/**
 * Performance monitor (vMix-style): FPS, render time, CPU%.
 * Polls the backend every second.
 */
export default function PerfPanel() {
  const [fps, setFps] = useState(0);
  const [renderMs, setRenderMs] = useState(0);
  const [cpu, setCpu] = useState(0);
  const [kbps, setKbps] = useState(0);
  const [skipped, setSkipped] = useState(0);
  const [dropped, setDropped] = useState(0);
  const [stages, setStages] = useState<[number, number, number, number]>([0, 0, 0, 0]);
  const [encoder, setEncoder] = useState("");

  useEffect(() => {
    let cancelled = false;
    const poll = async () => {
      try {
        const [f, r, c, k, sk, dr] = await api.perfStats();
        const sg = await api.perfStages().catch(() => [0, 0, 0, 0]);
        const enc = await api.streamEncoderName();
        if (!cancelled) {
          setFps(Math.round(f));
          setRenderMs(Math.round(r * 10) / 10);
          setCpu(Math.round(c));
          setKbps(Math.round(k));
          setSkipped(Math.round(sk ?? 0));
          setDropped(Math.round(dr ?? 0));
          setStages([
            Math.round((sg[0] ?? 0) * 10) / 10,
            Math.round((sg[1] ?? 0) * 10) / 10,
            Math.round((sg[2] ?? 0) * 10) / 10,
            Math.round((sg[3] ?? 0) * 10) / 10,
          ]);
          setEncoder(enc);
        }
      } catch {
        /* backend may not support it yet */
      }
    };
    void poll();
    const t = setInterval(() => void poll(), 1000);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, []);

  return (
    <div
      className="perf-panel mono"
      style={{
        display: "flex",
        gap: 12,
        padding: "4px 12px",
        fontSize: 11,
        background: "var(--bg-raised)",
        borderRadius: 6,
        alignItems: "center",
      }}
      title="Performance: frames/sec, backend render ms, CPU% estimate, skipped (render/encode) vs dropped (network) frames"
    >
      <span>FPS: {fps}</span>
      <span>Render: {renderMs} ms</span>
      <span>CPU: {cpu}%</span>
      <span>↑ {kbps} kbps</span>
      <span title="Frames skipped: render/encode couldn't keep up (PC-side)">
        Skip: {skipped}
      </span>
      <span title="Packets dropped: network couldn't keep up">Drop: {dropped}</span>
      <span title="Last tick stage times in ms: render (capture+grade+composite) / audio / record / stream (encode+send)">
        Stages: {stages[0]}/{stages[1]}/{stages[2]}/{stages[3]} ms
      </span>
      {encoder && <span>Enc: {encoder}</span>}
    </div>
  );
}
