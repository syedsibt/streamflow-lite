import { useStudio } from "../lib/store";

interface SliderProps {
  label: string;
  param: string;
  min: number;
  max: number;
  step: number;
  value: number;
  format: (v: number) => string;
}

/**
 * Spec §8: slider drags sync to the backend throttled to 60 Hz (handled in
 * the store's setParam); double-clicking a slider row resets that parameter.
 */
export default function Slider({
  label,
  param,
  min,
  max,
  step,
  value,
  format,
}: SliderProps) {
  const grade = useStudio((s) => s.grade);
  const setParam = useStudio((s) => s.setParam);
  const resetParam = useStudio((s) => s.resetParam);
  const disabled = grade === null;

  return (
    <div
      className="slider-row"
      onDoubleClick={() => {
        if (!disabled) void resetParam(param);
      }}
      title={disabled ? "Waiting for backend…" : "Double-click to reset"}
    >
      <label>{label}</label>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        disabled={disabled}
        onChange={(e) => setParam(param, parseFloat(e.target.value))}
      />
      <span className="value">{format(value)}</span>
    </div>
  );
}
