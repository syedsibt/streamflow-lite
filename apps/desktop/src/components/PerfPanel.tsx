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

  useEffect(() => {
    let cancelled = false;
    const poll = async () => {
      try {
        const [f, r, c] = await api.perfStats();
        if (!cancelled) {
          setFps(Math.round(f));
          setRenderMs(Math.round(r * 10) / 10);
          setCpu(Math.round(c));
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
      title="Performance: frames/sec, backend render ms, CPU% estimate"
    >
      <span>FPS: {fps}</span>
      <span>Render: {renderMs} ms</span>
      <span>CPU: {cpu}%</span>
    </div>
  );
}
