import { useStudio } from "../../lib/store";
import { SKIN_PARAMS } from "../../lib/types";
import Slider from "../Slider";

/** Natural skin recipe scaled by the one-knob "Intensity". */
const SKIN_RECIPE = { skin_warmth: 8, skin_saturation: -6, skin_brightness: 10, smooth: 0.35 };

/** Glow + Skin / Portrait — both run on the GPU in the live pipeline. */
export default function EffectsTab() {
  const grade = useStudio((s) => s.grade);
  const stack = useStudio((s) => s.stack);
  const effectKinds = useStudio((s) => s.effectKinds);
  const setParam = useStudio((s) => s.setParam);
  const ensureEffect = useStudio((s) => s.ensureEffect);
  const removeEffect = useStudio((s) => s.removeEffect);
  const setEffectParams = useStudio((s) => s.setEffectParams);
  const setSlotIntensity = useStudio((s) => s.setSlotIntensity);
  const resetSection = useStudio((s) => s.resetSection);

  if (grade === null) return <div className="note">Waiting for backend…</div>;
  const g = grade as unknown as Record<string, number>;

  const glow = stack.find((s) => s.kind === "bloom");
  const glowDefaults = effectKinds.find((k) => k.kind === "bloom")?.default_params as
    | Record<string, number>
    | undefined;
  const gp = (glow?.params ?? glowDefaults ?? {}) as unknown as Record<string, number>;

  const smooth = stack.find((s) => s.kind === "skinsmooth");
  const skinOn = grade.skin_protect || !!smooth || SKIN_PARAMS.some((d) => g[d.param] !== 0);

  const applySkinIntensity = async (i: number) => {
    setParam("skin_warmth", SKIN_RECIPE.skin_warmth * i);
    setParam("skin_saturation", SKIN_RECIPE.skin_saturation * i);
    setParam("skin_brightness", SKIN_RECIPE.skin_brightness * i);
    if (i > 0) {
      const id = await ensureEffect("SkinSmooth");
      if (id) await setSlotIntensity(id, SKIN_RECIPE.smooth * i);
    } else if (smooth) {
      await removeEffect("SkinSmooth");
    }
  };
  // Current recipe position (from brightness, the dominant knob).
  const skinIntensity = Math.max(0, Math.min(1, g.skin_brightness / SKIN_RECIPE.skin_brightness));

  return (
    <div>
      <div className="fx-head">
        <span className="section-title">Glow</span>
        <button
          className={glow ? "icon-btn mute-on" : "icon-btn"}
          onClick={() => void (glow ? removeEffect("Bloom") : ensureEffect("Bloom"))}
          title="Soft light bloom from bright areas (GPU, ~2 passes)"
        >
          {glow ? "ON" : "OFF"}
        </button>
      </div>
      <div className={glow ? "" : "disabled-block"}>
        <Slider
          label="Intensity"
          min={0}
          max={1}
          step={0.01}
          neutral={1}
          value={glow?.intensity ?? 1}
          disabled={!glow}
          format={(v) => `${Math.round(v * 100)}%`}
          onChange={(v) => glow && void setSlotIntensity(glow.id, v)}
        />
        <Slider
          label="Threshold"
          min={0}
          max={1}
          step={0.01}
          neutral={0.8}
          value={gp.threshold ?? 0.8}
          disabled={!glow}
          format={(v) => v.toFixed(2)}
          onChange={(v) => void setEffectParams("Bloom", { threshold: v })}
          title="Only areas brighter than this glow"
        />
        <Slider
          label="Softness"
          min={0}
          max={1}
          step={0.01}
          neutral={0.5}
          value={gp.softness ?? 0.5}
          disabled={!glow}
          format={(v) => v.toFixed(2)}
          onChange={(v) => void setEffectParams("Bloom", { softness: v })}
          title="How gradually the glow fades in around the threshold"
        />
        <Slider
          label="Radius"
          min={1}
          max={48}
          step={0.5}
          neutral={6}
          value={gp.radius ?? 6}
          disabled={!glow}
          format={(v) => `${v.toFixed(1)} px`}
          onChange={(v) => void setEffectParams("Bloom", { radius: v })}
        />
      </div>

      <div className="fx-head" style={{ marginTop: 14 }}>
        <span className="section-title">Skin / Portrait</span>
        <button
          className={skinOn ? "icon-btn mute-on" : "icon-btn"}
          onClick={() => {
            if (skinOn) {
              setParam("skin_protect", 0);
              void resetSection("skin");
              void removeEffect("SkinSmooth");
            } else {
              setParam("skin_protect", 1);
              void applySkinIntensity(0.6);
            }
          }}
          title="Natural skin enhancement — protects skin from over-saturation and gently evens it"
        >
          {skinOn ? "ON" : "OFF"}
        </button>
      </div>
      <Slider
        label="Intensity"
        min={0}
        max={1}
        step={0.01}
        neutral={0}
        value={skinIntensity}
        format={(v) => `${Math.round(v * 100)}%`}
        onChange={(v) => void applySkinIntensity(v)}
        title="One knob: natural warmth + brightness + smoothing"
      />
      {SKIN_PARAMS.map((def) => (
        <Slider
          key={def.param}
          label={def.label}
          param={def.param}
          min={def.min}
          max={def.max}
          step={def.step}
          neutral={def.neutral}
          value={g[def.param] ?? 0}
          format={def.format}
        />
      ))}
      <Slider
        label="Smoothness"
        min={0}
        max={1}
        step={0.01}
        neutral={0}
        value={smooth?.intensity ?? 0}
        format={(v) => `${Math.round(v * 100)}%`}
        onChange={async (v) => {
          if (v <= 0) {
            await removeEffect("SkinSmooth");
            return;
          }
          const id = await ensureEffect("SkinSmooth");
          if (id) await setSlotIntensity(id, v);
        }}
        title="Skin-masked smoothing (GPU). Keep it low for a natural look."
      />
      <label className="check-row">
        <input
          type="checkbox"
          checked={grade.skin_protect}
          onChange={(e) => setParam("skin_protect", e.target.checked ? 1 : 0)}
        />
        Protect skin when boosting saturation / vibrance
      </label>
      <div className="note">
        Skin controls act only inside the detected skin tones — background and clothes stay as they are.
      </div>
    </div>
  );
}
