import { useEffect, useState, type ReactNode } from "react";
import { useProduction } from "../../lib/productionStore";
import { FX_OFF, FX_VOICE, type FxSettings, type FxStats } from "../../lib/types";
import { subscribeMeters } from "./meters";

/** Live effect readings (gain reduction, gate, voice) at ~4 Hz from the meter feed. */
function useFxLive(target: string): FxStats | null {
  const [st, setSt] = useState<FxStats | null>(null);
  useEffect(() => {
    let last = 0;
    return subscribeMeters((m) => {
      const now = performance.now();
      if (now - last < 250) return;
      last = now;
      setSt(target === "master" ? (m.master_fx ?? null) : (m.channels.find((c) => c.id === target)?.fx ?? null));
    });
  }, [target]);
  return st;
}

/**
 * Audio settings of one mixer channel (or the master bus): every effect
 * with its own on/off switch and its controls. Changes are live; a switched
 * off effect costs no CPU.
 */

type Key = keyof FxSettings;

function Slider({ label, k, fx, set, min, max, step, fmt, off, neutral }: { label: string; k: Key; fx: FxSettings; set: (p: Partial<FxSettings>) => void; min: number; max: number; step: number; fmt: (v: number) => string; off: boolean; neutral?: number }) {
  const value = fx[k] as number;
  return (
    <label className={"hrange" + (off ? " disabled" : "")} onDoubleClick={() => neutral !== undefined && !off && set({ [k]: neutral } as Partial<FxSettings>)} title={neutral !== undefined ? "Double-click to reset" : undefined}>
      <span>{label}</span>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        disabled={off}
        onChange={(e) => set({ [k]: parseFloat(e.target.value) } as Partial<FxSettings>)}
        style={{ ["--pct" as string]: `${((value - min) / (max - min)) * 100}%` }}
      />
      <output>{fmt(value)}</output>
    </label>
  );
}

function Effect({ title, hint, on, onToggle, reading, disabled, children }: { title: string; hint: string; on: boolean; onToggle: (v: boolean) => void; reading?: string | null; disabled?: boolean; children?: ReactNode }) {
  return (
    <section className={"fx" + (on ? " on" : "") + (disabled ? " disabled" : "")}>
      <header>
        <label className="fx-switch" title={on ? "Turn off" : "Turn on"}>
          <input type="checkbox" checked={on} disabled={disabled} onChange={(e) => onToggle(e.target.checked)} />
          <i aria-hidden="true" />
          <b>{title}</b>
        </label>
        {reading && on && <span className="fx-reading">{reading}</span>}
      </header>
      <p className="fx-hint">{hint}</p>
      {children && <div className="fx-controls">{children}</div>}
    </section>
  );
}

const db = (v: number) => `${v > 0 ? "+" : ""}${v.toFixed(1)} dB`;
const hz = (v: number) => (v >= 1000 ? `${(v / 1000).toFixed(v >= 10000 ? 0 : 1)} kHz` : `${Math.round(v)} Hz`);
const ms = (v: number) => `${v < 10 ? v.toFixed(1) : Math.round(v)} ms`;

export function FxEditor({ target }: { target: string }) {
  const isMaster = target === "master";
  const fx = useProduction((s) => (isMaster ? s.state?.mixer.master.fx : s.state?.mixer.channels.find((c) => c.source_id === target)?.fx));
  const stats = useFxLive(target);
  const setFx = useProduction((s) => s.setChannelFx);
  if (!fx) return <div className="panel-empty">This channel has no audio settings.</div>;
  const set = (p: Partial<FxSettings>) => setFx(target, { ...fx, ...p });
  const off = !fx.enabled;
  const any = fx.invert_polarity || fx.mono || fx.highpass_enabled || fx.lowpass_enabled || fx.denoise_enabled || fx.gate_enabled || fx.eq_enabled || fx.deesser_enabled || fx.compressor_enabled || fx.limiter_enabled || fx.delay_enabled;

  return (
    <div className="fx-editor">
      <div className="fx-top">
        <label className="fx-switch big" title="Bypass every effect of this channel">
          <input type="checkbox" checked={fx.enabled} onChange={(e) => set({ enabled: e.target.checked })} />
          <i aria-hidden="true" />
          <b>{fx.enabled ? "Effects on" : "Effects bypassed"}</b>
        </label>
        <div className="fx-presets" role="group" aria-label="Presets">
          {!isMaster && (
            <button type="button" className="tool" onClick={() => setFx(target, { ...FX_VOICE })} title="AI noise removal, gate, presence EQ, compressor">
              Voice
            </button>
          )}
          {!isMaster && (
            <button type="button" className="tool" onClick={() => setFx(target, { ...FX_OFF, eq_enabled: false, limiter_enabled: true })} title="Clean: only a safety limiter">
              Music
            </button>
          )}
          {isMaster && (
            <button type="button" className="tool" onClick={() => setFx(target, { ...FX_OFF, limiter_enabled: true })} title="Only the −1 dB safety limiter">
              Default
            </button>
          )}
          <button type="button" className="tool" onClick={() => setFx(target, { ...FX_OFF })}>
            All off
          </button>
        </div>
        <span className="fx-stats">
          {stats && any && fx.enabled ? `${stats.latency_ms ? `adds ${Math.round(stats.latency_ms)} ms · ` : ""}${stats.process_us} µs CPU / block` : any ? "" : "No effect on — zero CPU"}
        </span>
      </div>

      <div className="fx-grid">
        <Effect title="Low cut" hint="Removes rumble, desk knocks and wind below the voice." on={fx.highpass_enabled} disabled={off} onToggle={(v) => set({ highpass_enabled: v })}>
          <Slider label="Below" k="highpass_hz" fx={fx} set={set} min={20} max={400} step={5} fmt={hz} off={off || !fx.highpass_enabled} neutral={80} />
        </Effect>

        <Effect title="High cut" hint="Removes hiss above the voice." on={fx.lowpass_enabled} disabled={off} onToggle={(v) => set({ lowpass_enabled: v })}>
          <Slider label="Above" k="lowpass_hz" fx={fx} set={set} min={2000} max={20000} step={100} fmt={hz} off={off || !fx.lowpass_enabled} neutral={16000} />
        </Effect>

        {!isMaster && (
          <Effect title="AI noise removal" hint="Fans, hum, keyboard and room noise; keeps speech. Adds a fixed 20 ms." on={fx.denoise_enabled} disabled={off} onToggle={(v) => set({ denoise_enabled: v })} reading={stats ? `voice ${Math.round(stats.vad * 100)}%` : null}>
            <Slider label="Strength" k="denoise_strength" fx={fx} set={set} min={0} max={1} step={0.05} fmt={(v) => `${Math.round(v * 100)}%`} off={off || !fx.denoise_enabled} neutral={1} />
          </Effect>
        )}

        {!isMaster && (
          <Effect title="Noise gate" hint="Silences the channel between phrases." on={fx.gate_enabled} disabled={off} onToggle={(v) => set({ gate_enabled: v })} reading={stats ? (stats.gate_closed ? "closed" : "open") : null}>
            <Slider label="Threshold" k="gate_threshold_db" fx={fx} set={set} min={-80} max={0} step={1} fmt={(v) => `${Math.round(v)} dB`} off={off || !fx.gate_enabled} neutral={-40} />
            <Slider label="Close after" k="gate_release_ms" fx={fx} set={set} min={20} max={1000} step={10} fmt={ms} off={off || !fx.gate_enabled} neutral={120} />
          </Effect>
        )}

        <Effect title="Equalizer" hint="Low shelf 200 Hz, adjustable mid, high shelf 6 kHz." on={fx.eq_enabled} disabled={off} onToggle={(v) => set({ eq_enabled: v })}>
          <Slider label="Low" k="eq_low_db" fx={fx} set={set} min={-12} max={12} step={0.5} fmt={db} off={off || !fx.eq_enabled} neutral={0} />
          <Slider label="Mid" k="eq_mid_db" fx={fx} set={set} min={-12} max={12} step={0.5} fmt={db} off={off || !fx.eq_enabled} neutral={0} />
          <Slider label="Mid at" k="eq_mid_hz" fx={fx} set={set} min={200} max={5000} step={50} fmt={hz} off={off || !fx.eq_enabled} neutral={1000} />
          <Slider label="High" k="eq_high_db" fx={fx} set={set} min={-12} max={12} step={0.5} fmt={db} off={off || !fx.eq_enabled} neutral={0} />
        </Effect>

        {!isMaster && (
          <Effect title="De-esser" hint="Softens sharp “s” and “sh” sounds." on={fx.deesser_enabled} disabled={off} onToggle={(v) => set({ deesser_enabled: v })} reading={stats ? `−${stats.deesser_reduction_db.toFixed(1)} dB` : null}>
            <Slider label="Listens at" k="deesser_hz" fx={fx} set={set} min={3000} max={10000} step={100} fmt={hz} off={off || !fx.deesser_enabled} neutral={6000} />
            <Slider label="Threshold" k="deesser_threshold_db" fx={fx} set={set} min={-60} max={0} step={1} fmt={(v) => `${Math.round(v)} dB`} off={off || !fx.deesser_enabled} neutral={-30} />
            <Slider label="Max cut" k="deesser_max_db" fx={fx} set={set} min={0} max={18} step={0.5} fmt={db} off={off || !fx.deesser_enabled} neutral={6} />
          </Effect>
        )}

        <Effect title="Compressor" hint="Evens out loud and quiet moments." on={fx.compressor_enabled} disabled={off} onToggle={(v) => set({ compressor_enabled: v })} reading={stats ? `−${stats.comp_reduction_db.toFixed(1)} dB` : null}>
          <Slider label="Threshold" k="comp_threshold_db" fx={fx} set={set} min={-60} max={0} step={1} fmt={(v) => `${Math.round(v)} dB`} off={off || !fx.compressor_enabled} neutral={-18} />
          <Slider label="Ratio" k="comp_ratio" fx={fx} set={set} min={1} max={20} step={0.5} fmt={(v) => `${v.toFixed(1)}:1`} off={off || !fx.compressor_enabled} neutral={3} />
          <Slider label="Attack" k="comp_attack_ms" fx={fx} set={set} min={0.1} max={200} step={0.1} fmt={ms} off={off || !fx.compressor_enabled} neutral={10} />
          <Slider label="Release" k="comp_release_ms" fx={fx} set={set} min={10} max={2000} step={10} fmt={ms} off={off || !fx.compressor_enabled} neutral={100} />
          <Slider label="Make-up" k="comp_makeup_db" fx={fx} set={set} min={0} max={24} step={0.5} fmt={db} off={off || !fx.compressor_enabled} neutral={0} />
        </Effect>

        <Effect title="Limiter" hint="A hard ceiling: no peak gets through, so nothing clips." on={fx.limiter_enabled} disabled={off} onToggle={(v) => set({ limiter_enabled: v })} reading={stats ? `−${stats.limiter_reduction_db.toFixed(1)} dB` : null}>
          <Slider label="Ceiling" k="limiter_ceiling_db" fx={fx} set={set} min={-20} max={0} step={0.5} fmt={db} off={off || !fx.limiter_enabled} neutral={-1} />
          <Slider label="Release" k="limiter_release_ms" fx={fx} set={set} min={10} max={1000} step={10} fmt={ms} off={off || !fx.limiter_enabled} neutral={50} />
        </Effect>

        {!isMaster && (
          <Effect title="Delay" hint="Holds this channel back to line it up with a late picture." on={fx.delay_enabled} disabled={off} onToggle={(v) => set({ delay_enabled: v })}>
            <Slider label="Delay" k="delay_ms" fx={fx} set={set} min={0} max={1000} step={5} fmt={(v) => `${Math.round(v)} ms`} off={off || !fx.delay_enabled} neutral={0} />
          </Effect>
        )}

        <Effect title="Polarity / mono" hint="Flip a mic wired out of phase, or put a one-sided mic in both ears." on={fx.invert_polarity || fx.mono} disabled={off} onToggle={(v) => set(v ? { mono: true } : { mono: false, invert_polarity: false })}>
          <label className={"check" + (off ? " disabled" : "")}>
            <input type="checkbox" checked={fx.invert_polarity} disabled={off} onChange={(e) => set({ invert_polarity: e.target.checked })} />
            <span>Invert polarity</span>
          </label>
          <label className={"check" + (off ? " disabled" : "")}>
            <input type="checkbox" checked={fx.mono} disabled={off} onChange={(e) => set({ mono: e.target.checked })} />
            <span>Mono (left + right)</span>
          </label>
        </Effect>
      </div>
    </div>
  );
}

/**
 * Audio settings window (from the mixer): every audio channel and the
 * master on the left, the selected one's effects on the right.
 */
export function FxDialog({ initial, channels, onClose }: { initial: string; channels: { id: string; name: string }[]; onClose: () => void }) {
  const [target, setTarget] = useState(initial);
  const fxOf = useProduction((s) => s.state?.mixer);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  const list = [...channels, { id: "master", name: "Master (program)" }];
  const activeCount = (id: string) => {
    const fx = id === "master" ? fxOf?.master.fx : fxOf?.channels.find((c) => c.source_id === id)?.fx;
    if (!fx || !fx.enabled) return 0;
    return [fx.highpass_enabled, fx.lowpass_enabled, fx.denoise_enabled, fx.gate_enabled, fx.eq_enabled, fx.deesser_enabled, fx.compressor_enabled, fx.limiter_enabled, fx.delay_enabled, fx.invert_polarity || fx.mono].filter(Boolean).length;
  };
  const name = list.find((c) => c.id === target)?.name ?? "Channel";
  return (
    <div className="modal-scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal fx-modal" role="dialog" aria-modal="true" aria-label="Audio settings">
        <nav className="kind-list" aria-label="Channels">
          <div className="fx-nav-title">Audio settings</div>
          {list.map((c) => {
            const n = activeCount(c.id);
            return (
              <button key={c.id} type="button" className={target === c.id ? "on" : ""} onClick={() => setTarget(c.id)}>
                <span className="fx-nav-name">{c.name}</span>
                <em className={n ? "lit" : ""}>{n ? `${n} on` : "off"}</em>
              </button>
            );
          })}
        </nav>
        <div className="fx-modal-main">
          <div className="fx-modal-head">
            <h2>{name}</h2>
            <button type="button" className="btn small secondary" onClick={onClose}>
              Done
            </button>
          </div>
          <div className="fx-modal-body">
            <FxEditor key={target} target={target} />
          </div>
        </div>
      </div>
    </div>
  );
}
