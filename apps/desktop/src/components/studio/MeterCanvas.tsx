import { useEffect, useRef } from "react";
import { dbToPos, PeakHold, subscribeMeters, toDb } from "./meters";

/**
 * Vertical (or horizontal) level meter: RMS bar, instantaneous peak, 1.5 s
 * peak-hold line and a latched clip light (click to reset). Draws on a
 * canvas from the live meter feed; values are the mixer's real levels.
 */
export default function MeterCanvas({
  channel,
  orientation = "vertical",
  title,
}: {
  /** Source id, or "master". */
  channel: string;
  orientation?: "vertical" | "horizontal";
  title?: string;
}) {
  const ref = useRef<HTMLCanvasElement>(null);
  const clipRef = useRef<HTMLButtonElement>(null);
  const hold = useRef(new PeakHold());
  const clipped = useRef(false);
  const smooth = useRef(-120);

  useEffect(() => {
    const draw = (peakDb: number, rmsDb: number) => {
      const c = ref.current;
      if (!c) return;
      const dpr = window.devicePixelRatio || 1;
      const w = c.clientWidth;
      const h = c.clientHeight;
      if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
        c.width = Math.round(w * dpr);
        c.height = Math.round(h * dpr);
      }
      const g = c.getContext("2d");
      if (!g) return;
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      g.clearRect(0, 0, w, h);
      const vert = orientation === "vertical";
      const len = vert ? h : w;
      const thick = vert ? w : h;
      // Track
      g.fillStyle = "#0b0c0e";
      g.fillRect(0, 0, w, h);
      // Segments: green to -18, amber to -6, red above.
      const seg = (fromDb: number, toDbv: number, color: string, upto: number) => {
        const a = dbToPos(fromDb) * len;
        const b = Math.min(dbToPos(toDbv), dbToPos(upto)) * len;
        if (b <= a) return;
        g.fillStyle = color;
        if (vert) g.fillRect(0, h - b, thick, b - a);
        else g.fillRect(a, 0, b - a, thick);
      };
      // RMS body (bright) + peak extension (dim).
      const body = (db: number, dim: boolean) => {
        seg(-60, -18, dim ? "#1d5c38" : "#2eae63", db);
        seg(-18, -6, dim ? "#6b5420" : "#e2a63b", db);
        seg(-6, 0, dim ? "#6e2224" : "#e0393e", db);
      };
      body(peakDb, true);
      body(rmsDb, false);
      // Peak hold line.
      const hp = dbToPos(hold.current.value) * len;
      if (hold.current.value > -60) {
        g.fillStyle = hold.current.value > -6 ? "#ff6b6e" : "#e8eaed";
        if (vert) g.fillRect(0, h - hp - 1, thick, 2);
        else g.fillRect(hp - 1, 0, 2, thick);
      }
      // Scale ticks.
      g.fillStyle = "rgba(255,255,255,0.18)";
      for (const t of [-6, -12, -18, -24, -36, -48]) {
        const p = dbToPos(t) * len;
        if (vert) g.fillRect(0, h - p, thick, 1);
        else g.fillRect(p, 0, 1, thick);
      }
    };
    draw(-120, -120);
    return subscribeMeters((m) => {
      const ch = channel === "master" ? { peak: m.master_peak, rms: m.master_rms } : m.channels.find((c) => c.id === channel);
      const peakDb = toDb(ch?.peak ?? 0);
      // RMS ballistics: fast attack, ~300 ms release (display only).
      const rms = toDb(ch?.rms ?? 0);
      smooth.current = rms > smooth.current ? rms : Math.max(rms, smooth.current - 3);
      hold.current.update(peakDb, performance.now());
      if (peakDb >= -0.5 && !clipped.current) {
        clipped.current = true;
        clipRef.current?.classList.add("on");
      }
      draw(peakDb, smooth.current);
      const el = ref.current;
      if (el) el.title = `${title ?? ""}${title ? " · " : ""}peak ${peakDb > -100 ? peakDb.toFixed(1) : "−∞"} dBFS · hold ${hold.current.value > -100 ? hold.current.value.toFixed(1) : "−∞"} dBFS`;
    });
  }, [channel, orientation, title]);

  return (
    <div className={"meter-wrap " + orientation}>
      <button
        ref={clipRef}
        type="button"
        className="clip-led"
        title="Clip (0 dBFS reached) — click to reset"
        onClick={() => {
          clipped.current = false;
          clipRef.current?.classList.remove("on");
          hold.current.value = -120;
        }}
      />
      <canvas ref={ref} className="meter-canvas" />
    </div>
  );
}
