import { useRef } from "react";
import type { Vec4 } from "../lib/types";
import Slider from "./Slider";

/**
 * Lumetri-style color wheel driving one engine wheel (lift / gamma / gain /
 * offset). The puck sets hue + saturation of the tint, the slider below sets
 * luminance; both map onto the engine's existing `[r, g, b, master]` wheel
 * parameters, so the GPU path is unchanged.
 *
 * Mapping (zero-mean RGB direction for hue h, red at 0°):
 *   rgb[c] = neutral + sat · chroma · cos(h − c·120°)
 *   master = neutral + lum · lumScale
 * and the exact inverse is used to place the puck from the current values,
 * so presets / Auto Color / per-channel edits show up on the wheel.
 */
export interface WheelSpec {
  key: "lift" | "gamma_wheel" | "gain" | "offset";
  label: string;
  hint: string;
  neutral: number;
  chroma: number;
  lumScale: number;
}

const SIZE = 132;

function toParams(spec: WheelSpec, hue: number, sat: number): [number, number, number] {
  const a = sat * spec.chroma;
  return [0, 1, 2].map(
    (c) => spec.neutral + a * Math.cos(hue - (c * 2 * Math.PI) / 3),
  ) as [number, number, number];
}

function fromParams(spec: WheelSpec, v: Vec4): { hue: number; sat: number; lum: number } {
  const d = [v[0] - spec.neutral, v[1] - spec.neutral, v[2] - spec.neutral];
  const x = d[0] - (d[1] + d[2]) / 2;
  const y = ((d[1] - d[2]) * Math.sqrt(3)) / 2;
  const amp = Math.hypot(x, y) / 1.5;
  return {
    hue: Math.atan2(y, x),
    sat: Math.min(1, amp / spec.chroma),
    lum: (v[3] - spec.neutral) / spec.lumScale,
  };
}

export default function ColorWheel({
  spec,
  value,
  onChannels,
  disabled,
}: {
  spec: WheelSpec;
  value: Vec4;
  /** Set any of `_r`, `_g`, `_b`, `_master`. */
  onChannels: (patch: Partial<Record<"r" | "g" | "b" | "master", number>>) => void;
  disabled?: boolean;
}) {
  const ref = useRef<HTMLDivElement | null>(null);
  const dragging = useRef(false);
  const { hue, sat, lum } = fromParams(spec, value);
  const r = SIZE / 2;
  const px = r + Math.cos(hue) * sat * (r - 8);
  const py = r - Math.sin(hue) * sat * (r - 8);

  const setFromPointer = (clientX: number, clientY: number, fine: boolean) => {
    const el = ref.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const scale = SIZE / rect.width;
    let dx = (clientX - rect.left) * scale - r;
    let dy = r - (clientY - rect.top) * scale;
    if (fine) {
      // Fine: move the puck at 1/4 speed relative to its current spot.
      dx = (px - r) + (dx - (px - r)) * 0.25;
      dy = (r - py) + (dy - (r - py)) * 0.25;
    }
    const s = Math.min(1, Math.hypot(dx, dy) / (r - 8));
    const h = Math.atan2(dy, dx);
    const [cr, cg, cb] = toParams(spec, h, s < 0.02 ? 0 : s);
    onChannels({ r: cr, g: cg, b: cb });
  };

  return (
    <div className={`color-wheel ${disabled ? "disabled" : ""}`}>
      <div className="wheel-head">
        <span className="wheel-label">{spec.label}</span>
        <button
          className="reset-btn"
          title={`Reset ${spec.label}`}
          disabled={disabled}
          onClick={() =>
            onChannels({ r: spec.neutral, g: spec.neutral, b: spec.neutral, master: spec.neutral })
          }
        >
          ↺
        </button>
      </div>
      <div
        ref={ref}
        className="wheel-ring"
        style={{ width: SIZE, height: SIZE }}
        title={`${spec.hint} — drag to tint, Shift = fine, double-click = neutral`}
        onPointerDown={(e) => {
          if (disabled) return;
          dragging.current = true;
          (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
          setFromPointer(e.clientX, e.clientY, e.shiftKey);
        }}
        onPointerMove={(e) => {
          if (dragging.current) setFromPointer(e.clientX, e.clientY, e.shiftKey);
        }}
        onPointerUp={() => (dragging.current = false)}
        onPointerCancel={() => (dragging.current = false)}
        onDoubleClick={() =>
          !disabled && onChannels({ r: spec.neutral, g: spec.neutral, b: spec.neutral })
        }
      >
        <div className="puck" style={{ left: px - 7, top: py - 7 }} />
      </div>
      <div className="note wheel-readout">
        Hue {Math.round(((hue * 180) / Math.PI + 360) % 360)}° · Sat {Math.round(sat * 100)}%
      </div>
      <Slider
        label="Lum"
        min={-1}
        max={1}
        step={0.01}
        value={Math.max(-1, Math.min(1, lum))}
        neutral={0}
        format={(v) => `${v >= 0 ? "+" : ""}${Math.round(v * 100)}`}
        disabled={disabled}
        onChange={(v) => onChannels({ master: spec.neutral + v * spec.lumScale })}
      />
    </div>
  );
}
