import { useEffect, useState } from "react";
import { useStudio } from "../lib/store";

interface SliderProps {
  label: string;
  /** Grade parameter name (used when no custom onChange/onReset is given). */
  param?: string;
  min: number;
  max: number;
  step: number;
  value: number;
  format: (v: number) => string;
  /** Value the reset button returns to (custom handlers only). */
  neutral?: number;
  onChange?: (v: number) => void;
  onReset?: () => void;
  disabled?: boolean;
  title?: string;
}

/**
 * One professional control row: slider + exact numeric entry + reset.
 *
 * - Drag syncs live (the store throttles grade params to 60 Hz).
 * - Hold **Shift** while dragging or using arrow keys for fine steps (1/10).
 * - Type an exact value in the number box (Enter / blur commits; clamped).
 * - ↺ (or double-click the row) resets to neutral.
 */
export default function Slider({
  label,
  param,
  min,
  max,
  step,
  value,
  format,
  neutral,
  onChange,
  onReset,
  disabled: disabledProp,
  title,
}: SliderProps) {
  const grade = useStudio((s) => s.grade);
  const setParam = useStudio((s) => s.setParam);
  const resetParam = useStudio((s) => s.resetParam);
  const disabled = disabledProp ?? (grade === null && !onChange);
  const [fine, setFine] = useState(false);
  const [text, setText] = useState<string | null>(null);

  // Drop an in-progress text edit when the value changes from elsewhere.
  useEffect(() => setText(null), [value]);

  const change = (v: number) => {
    if (!Number.isFinite(v)) return;
    const c = Math.min(max, Math.max(min, v));
    if (onChange) onChange(c);
    else if (param) setParam(param, c);
  };
  const reset = () => {
    if (disabled) return;
    if (onReset) onReset();
    else if (onChange && neutral !== undefined) onChange(neutral);
    else if (param) void resetParam(param);
  };
  const decimals = Math.max(0, Math.min(4, Math.ceil(-Math.log10(step / (fine ? 10 : 1)))));
  const atNeutral = neutral !== undefined && Math.abs(value - neutral) < step / 20;

  return (
    <div
      className="slider-row pro"
      onDoubleClick={reset}
      title={title ?? (disabled ? "Waiting for backend…" : "Shift = fine · double-click = reset")}
    >
      <label>{label}</label>
      <input
        type="range"
        min={min}
        max={max}
        step={fine ? step / 10 : step}
        value={value}
        disabled={disabled}
        onPointerDown={(e) => setFine(e.shiftKey)}
        onKeyDown={(e) => setFine(e.shiftKey)}
        onKeyUp={(e) => setFine(e.shiftKey)}
        onChange={(e) => change(parseFloat(e.target.value))}
        aria-label={label}
      />
      <input
        className="num-input"
        type="number"
        min={min}
        max={max}
        step={step}
        disabled={disabled}
        value={text ?? Number(value.toFixed(decimals)).toString()}
        title={format(value)}
        onChange={(e) => setText(e.target.value)}
        onBlur={() => {
          if (text !== null) change(parseFloat(text));
          setText(null);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            if (text !== null) change(parseFloat(text));
            setText(null);
            (e.target as HTMLInputElement).blur();
          } else if (e.key === "Escape") {
            setText(null);
          }
        }}
        aria-label={`${label} value`}
      />
      <button
        className="reset-btn"
        onClick={reset}
        disabled={disabled || atNeutral}
        title={`Reset ${label}`}
        aria-label={`Reset ${label}`}
      >
        ↺
      </button>
    </div>
  );
}
