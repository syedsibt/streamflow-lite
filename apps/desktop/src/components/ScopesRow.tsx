import { useEffect, useRef } from "react";
import { useStudio } from "../lib/store";
import type { ScopeDataView } from "../lib/types";

type WaveformData = Extract<ScopeDataView, { kind: "waveform" }>;
type HistogramData = Extract<ScopeDataView, { kind: "histogram" }>;

const clamp01 = (v: number) => Math.min(1, Math.max(0, v));

// ---------------------------------------------------------------------------
// Waveform: filled luma band (white) + r/g/b max strokes, value 1 at top.
// ---------------------------------------------------------------------------
function WaveformSvg({ data }: { data: WaveformData }) {
  const W = 256;
  const H = 100;
  const y = (v: number) => H - clamp01(v) * H;
  const x = (i: number, n: number) => (i / (n - 1)) * W;

  const linePath = (arr: number[]) => {
    if (arr.length === 0) return "";
    let d = `M 0 ${y(arr[0]).toFixed(1)}`;
    for (let i = 1; i < arr.length; i++)
      d += ` L ${x(i, arr.length).toFixed(1)} ${y(arr[i]).toFixed(1)}`;
    return d;
  };

  const bandPath = () => {
    const { luma_max, luma_min } = data;
    const n = luma_max.length;
    if (n === 0) return "";
    let d = `M 0 ${y(luma_max[0]).toFixed(1)}`;
    for (let i = 1; i < n; i++)
      d += ` L ${x(i, n).toFixed(1)} ${y(luma_max[i]).toFixed(1)}`;
    for (let i = n - 1; i >= 0; i--)
      d += ` L ${x(i, n).toFixed(1)} ${y(luma_min[i] ?? 0).toFixed(1)}`;
    return d + " Z";
  };

  const channels = [
    { c: "r" as const, color: "#f87171" },
    { c: "g" as const, color: "#4ade80" },
    { c: "b" as const, color: "#60a5fa" },
  ];

  return (
    <svg viewBox="0 0 256 100" preserveAspectRatio="none">
      <path d={bandPath()} fill="rgba(255,255,255,0.16)" stroke="none" />
      {channels.map(({ c, color }) => (
        <path
          key={c}
          d={linePath(data[`${c}_max`])}
          fill="none"
          stroke={color}
          strokeWidth={1}
          opacity={0.85}
        />
      ))}
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Vectorscope: 128x128 bins rendered via ImageData. Dark circle background,
// faint skin-tone guide line at 123° (0° = up, clockwise), intensity-scaled
// white/blue pixels. Fixed 128x128 canvas scaled by CSS.
// ---------------------------------------------------------------------------
function VectorscopeCanvas({ bins }: { bins: number[] }) {
  const ref = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    const N = 128;
    if (bins.length !== N * N) return; // guard malformed/empty data
    const canvas = ref.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const img = ctx.createImageData(N, N);
    const d = img.data;
    const cx = (N - 1) / 2;
    const cy = (N - 1) / 2;
    const radius = N / 2 - 1;
    let peak = 0;
    for (let i = 0; i < N * N; i++) if (bins[i] > peak) peak = bins[i];

    const skinRad = (123 * Math.PI) / 180;
    for (let yy = 0; yy < N; yy++) {
      for (let xx = 0; xx < N; xx++) {
        const dx = xx - cx;
        const dy = cy - yy; // y up
        const dist = Math.hypot(dx, dy);
        const o = (yy * N + xx) * 4;
        if (dist > radius) {
          d[o + 3] = 0;
          continue;
        }
        const v = peak > 0 ? bins[yy * N + xx] / peak : 0;
        const b = Math.round(14 + v * 200);
        d[o] = Math.round(b * 0.92);
        d[o + 1] = Math.round(b * 0.96);
        d[o + 2] = b;
        d[o + 3] = 255;
        // Faint skin-tone line guide at 123° (forward half only).
        const ang = Math.atan2(dx, dy);
        let da = Math.abs(ang - skinRad);
        if (da > Math.PI) da = 2 * Math.PI - da;
        if (dist * Math.sin(da) < 0.9 && Math.cos(da) > 0) {
          d[o] = 110;
          d[o + 1] = 95;
          d[o + 2] = 70;
          d[o + 3] = 255;
        }
      }
    }
    ctx.putImageData(img, 0, 0);
  }, [bins]);

  return (
    <canvas
      ref={ref}
      width={128}
      height={128}
      style={{ width: "100%", height: "100%", display: "block" }}
    />
  );
}

// ---------------------------------------------------------------------------
// Histogram: 4 overlaid translucent bar series (r/g/b + luma in white).
// ---------------------------------------------------------------------------
function HistogramSvg({ data }: { data: HistogramData }) {
  const series = [
    { arr: data.r, color: "#f87171" },
    { arr: data.g, color: "#4ade80" },
    { arr: data.b, color: "#60a5fa" },
    { arr: data.luma, color: "#ffffff" },
  ];
  let peak = 1e-9;
  for (const s of series) for (const v of s.arr) if (v > peak) peak = v;

  return (
    <svg viewBox="0 0 256 100" preserveAspectRatio="none">
      {series.map((s, si) =>
        s.arr.map((v, i) => {
          const h = (v / peak) * 100;
          return (
            <rect
              key={`${si}-${i}`}
              x={i}
              y={100 - h}
              width={1}
              height={h}
              fill={s.color}
              opacity={0.45}
            />
          );
        }),
      )}
    </svg>
  );
}

export default function ScopesRow() {
  const scopes = useStudio((s) => s.scopes);
  const wf =
    scopes.waveform && scopes.waveform.kind === "waveform"
      ? scopes.waveform
      : null;
  const vs =
    scopes.vectorscope && scopes.vectorscope.kind === "vectorscope"
      ? scopes.vectorscope
      : null;
  const hg =
    scopes.histogram && scopes.histogram.kind === "histogram"
      ? scopes.histogram
      : null;

  return (
    <div className="scopes-row">
      <div className="scope-card">
        <div className="scope-title">Waveform</div>
        {wf ? (
          <WaveformSvg data={wf} />
        ) : (
          <div className="note">Waiting for frames…</div>
        )}
      </div>
      <div className="scope-card">
        <div className="scope-title">Vectorscope</div>
        {vs ? (
          <VectorscopeCanvas bins={vs.bins} />
        ) : (
          <div className="note">Waiting for frames…</div>
        )}
      </div>
      <div className="scope-card">
        <div className="scope-title">Histogram</div>
        {hg ? (
          <HistogramSvg data={hg} />
        ) : (
          <div className="note">Waiting for frames…</div>
        )}
      </div>
    </div>
  );
}
