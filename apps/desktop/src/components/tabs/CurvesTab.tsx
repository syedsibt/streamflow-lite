import {
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { useStudio } from "../../lib/store";
import { CURVE_NAMES, type CurveName } from "../../lib/types";

/** Fixed internal canvas resolution (CSS scales it via .curves-canvas). */
const W = 300;
const H = 220;
const PAD = 14;
const MAX_POINTS = 16;
const HIT_PX = 10;

const clamp01 = (v: number) => Math.min(1, Math.max(0, v));
const toPx = (x: number) => PAD + x * (W - 2 * PAD);
const toPy = (y: number) => H - PAD - y * (H - 2 * PAD);
const fromPx = (px: number) => clamp01((px - PAD) / (W - 2 * PAD));
const fromPy = (py: number) => clamp01((H - PAD - py) / (H - 2 * PAD));

const sortPts = (pts: [number, number][]): [number, number][] =>
  [...pts].sort((a, b) => a[0] - b[0] || a[1] - b[1]);

export default function CurvesTab() {
  const grade = useStudio((s) => s.grade);
  const setCurve = useStudio((s) => s.setCurve);

  const [selected, setSelected] = useState<CurveName>("master");
  const [points, setPoints] = useState<[number, number][]>([
    [0, 0],
    [1, 1],
  ]);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  // While dragging, ignore incoming grade snapshots so points aren't clobbered.
  const dragging = useRef(false);
  const dragIdx = useRef<number | null>(null);
  const changed = useRef(false);
  const pointsRef = useRef<[number, number][]>([
    [0, 0],
    [1, 1],
  ]);

  const updatePoints = (next: [number, number][]) => {
    pointsRef.current = next;
    setPoints(next);
  };

  // Initialize from the backend when the grade or selection changes.
  useEffect(() => {
    if (dragging.current) return;
    const pts = grade?.curves[selected]?.points;
    if (pts && pts.length >= 2)
      updatePoints(pts.map(([x, y]) => [x, y]) as [number, number][]);
  }, [grade, selected]);

  // Redraw the editor.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.fillStyle = "#0a0d13";
    ctx.fillRect(0, 0, W, H);
    // Grid lines at 0.25 / 0.5 / 0.75.
    ctx.strokeStyle = "rgba(255,255,255,0.08)";
    ctx.lineWidth = 1;
    for (const g of [0.25, 0.5, 0.75]) {
      ctx.beginPath();
      ctx.moveTo(toPx(g), toPy(0));
      ctx.lineTo(toPx(g), toPy(1));
      ctx.stroke();
      ctx.beginPath();
      ctx.moveTo(toPx(0), toPy(g));
      ctx.lineTo(toPx(1), toPy(g));
      ctx.stroke();
    }
    // Diagonal reference.
    ctx.strokeStyle = "rgba(255,255,255,0.18)";
    ctx.beginPath();
    ctx.moveTo(toPx(0), toPy(0));
    ctx.lineTo(toPx(1), toPy(1));
    ctx.stroke();
    // Curve polyline.
    const sorted = sortPts(points);
    ctx.strokeStyle = "#e8ecf4";
    ctx.lineWidth = 2;
    ctx.beginPath();
    sorted.forEach(([x, y], i) => {
      if (i === 0) ctx.moveTo(toPx(x), toPy(y));
      else ctx.lineTo(toPx(x), toPy(y));
    });
    ctx.stroke();
    // Points (accent #3B82F6).
    ctx.fillStyle = "#3B82F6";
    for (const [x, y] of points) {
      ctx.beginPath();
      ctx.arc(toPx(x), toPy(y), 5, 0, Math.PI * 2);
      ctx.fill();
    }
  }, [points]);

  const canvasPos = (e: ReactMouseEvent<HTMLCanvasElement>) => {
    const canvas = canvasRef.current as HTMLCanvasElement;
    const r = canvas.getBoundingClientRect();
    const px = ((e.clientX - r.left) / r.width) * W;
    const py = ((e.clientY - r.top) / r.height) * H;
    return { px, py, x: fromPx(px), y: fromPy(py) };
  };

  const hitIndex = (px: number, py: number) =>
    pointsRef.current.findIndex(
      ([x, y]) => Math.hypot(toPx(x) - px, toPy(y) - py) < HIT_PX,
    );

  const onMouseDown = (e: ReactMouseEvent<HTMLCanvasElement>) => {
    const { px, py, x, y } = canvasPos(e);
    const idx = hitIndex(px, py);
    if (idx >= 0) {
      // Drag an existing point.
      dragging.current = true;
      dragIdx.current = idx;
      changed.current = false;
    } else if (pointsRef.current.length < MAX_POINTS) {
      // Click empty area: add a point (clamped 0..1, kept sorted).
      updatePoints(sortPts([...pointsRef.current, [x, y]]));
      changed.current = true;
    }
  };

  const onMouseMove = (e: ReactMouseEvent<HTMLCanvasElement>) => {
    if (!dragging.current || dragIdx.current === null) return;
    const { x, y } = canvasPos(e);
    const idx = dragIdx.current;
    updatePoints(
      pointsRef.current.map((p, i) =>
        i === idx ? [clamp01(x), clamp01(y)] : p,
      ),
    );
    changed.current = true;
  };

  const endDrag = () => {
    if (!dragging.current && !changed.current) return;
    dragging.current = false;
    dragIdx.current = null;
    const sorted = sortPts(pointsRef.current);
    updatePoints(sorted);
    if (changed.current) {
      changed.current = false;
      void setCurve(selected, sorted); // commit on mouse-up after any change
    }
  };

  const onDoubleClick = (e: ReactMouseEvent<HTMLCanvasElement>) => {
    const { px, py } = canvasPos(e);
    const idx = hitIndex(px, py);
    if (idx >= 0 && pointsRef.current.length > 2) {
      const next = pointsRef.current.filter((_, i) => i !== idx);
      updatePoints(next);
      void setCurve(selected, next);
    }
  };

  return (
    <div>
      <div className="curve-chips">
        {CURVE_NAMES.map((c) => (
          <button
            key={c}
            className={selected === c ? "active" : ""}
            onClick={() => setSelected(c)}
          >
            {c}
          </button>
        ))}
      </div>
      <canvas
        ref={canvasRef}
        className="curves-canvas"
        width={W}
        height={H}
        onMouseDown={onMouseDown}
        onMouseMove={onMouseMove}
        onMouseUp={endDrag}
        onMouseLeave={endDrag}
        onDoubleClick={onDoubleClick}
      />
      <div className="btn-row" style={{ marginTop: 8 }}>
        <button
          className="btn ghost"
          onClick={() => void setCurve(selected, [[0, 0], [1, 1]])}
        >
          Reset curve
        </button>
      </div>
    </div>
  );
}
