import { useEffect, useMemo, useState } from "react";
import { api } from "../../lib/ipc";
import { useProduction } from "../../lib/productionStore";
import type { EnhancerSettings } from "../../lib/types";
import Monitor from "../Monitor";
import { Banner, Dot, Empty, Field, Row, Section, Switch } from "../ui";

const AUDIO_KINDS = new Set(["audioinput", "camera", "media"]);

function Range({
  label,
  value,
  min,
  max,
  step,
  format,
  onChange,
  disabled,
  neutral,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  format: (v: number) => string;
  onChange: (v: number) => void;
  disabled?: boolean;
  neutral?: number;
}) {
  return (
    <div className={"range-row" + (disabled ? " disabled" : "")} onDoubleClick={() => neutral !== undefined && !disabled && onChange(neutral)}>
      <span className="range-label">{label}</span>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        disabled={disabled}
        aria-label={label}
        onChange={(e) => onChange(parseFloat(e.target.value))}
        style={{ ["--pct" as string]: `${((value - min) / (max - min)) * 100}%` }}
      />
      <span className="range-value">{format(value)}</span>
    </div>
  );
}

function Meter({ level }: { level: number }) {
  const db = level > 0 ? 20 * Math.log10(level) : -90;
  // −60 dB … 0 dB → 0 … 100 %
  const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  return (
    <div className="meter" title={level > 0 ? `${db.toFixed(0)} dBFS` : "Silence"}>
      <div className={"meter-fill" + (db > -3 ? " hot" : "")} style={{ width: `${pct}%` }} />
    </div>
  );
}

const db = (v: number) => `${v > 0 ? "+" : ""}${v.toFixed(1)} dB`;
const pct = (v: number) => `${Math.round(v * 100)}%`;

/**
 * AUDIO: levels, voice enhancer and A/V sync. Meters are the real mixer
 * peaks (they stay flat when no audio flows — never faked).
 */
export default function AudioView() {
  const state = useProduction((s) => s.state);
  const sourceAdd = useProduction((s) => s.sourceAdd);
  const sourceRemove = useProduction((s) => s.sourceRemove);
  const setChannelGain = useProduction((s) => s.setChannelGain);
  const setChannelMute = useProduction((s) => s.setChannelMute);
  const setMasterGain = useProduction((s) => s.setMasterGain);
  const setMasterMute = useProduction((s) => s.setMasterMute);
  const setMonitorEnabled = useProduction((s) => s.setMonitorEnabled);
  const setEnhancerSettings = useProduction((s) => s.setEnhancerSettings);
  const setAvSync = useProduction((s) => s.setAvSync);
  const busy = useProduction((s) => !!s.busy.add);

  const [mics, setMics] = useState<string[] | null>(null);
  const [mic, setMic] = useState("");
  const [labels, setLabels] = useState<Record<string, string>>({});
  const [err, setErr] = useState<string | null>(null);

  const mixer = state?.mixer ?? null;
  const sources = useMemo(
    () => (state?.scenes ?? []).flatMap((sc) => sc.sources).filter((s) => AUDIO_KINDS.has(s.kind)),
    [state?.scenes],
  );
  const idsKey = sources.map((s) => s.id).join(",");

  useEffect(() => {
    let off = false;
    void api
      .listAudioDevices()
      .then((l) => !off && (setMics(l), setMic((m) => m || l[0] || "")))
      .catch(() => !off && setMics([]));
    return () => {
      off = true;
    };
  }, []);

  useEffect(() => {
    let off = false;
    void (async () => {
      const out: Record<string, string> = {};
      for (const s of sources) {
        const d = await api.sourceAudioDevice(s.id).catch(() => null);
        if (d) out[s.id] = d;
      }
      if (!off) setLabels(out);
    })();
    return () => {
      off = true;
    };
  }, [idsKey]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!mixer) return null;
  const enh = mixer.enhancer;
  const setEnh = (p: Partial<EnhancerSettings>) => void setEnhancerSettings({ ...enh, ...p });
  const av = mixer.av_sync;

  const addMic = async () => {
    setErr(null);
    if (!mic) {
      setErr(mics && mics.length === 0 ? "No microphone found. Connect one and try again." : "Choose a microphone.");
      return;
    }
    await sourceAdd({ audio_input: { device: mic } }, mic);
  };

  return (
    <div className="workspace">
      <div className="stage-area">
        <Monitor />
      </div>
      <aside className="inspector">
        <Section title="Levels">
          {sources.length === 0 && <Empty title="No audio yet" hint="Add a microphone below, or a camera / media file with sound." />}
          {sources.map((s) => {
            const ch = mixer.channels.find((c) => c.source_id === s.id);
            const voice = mixer.voice[s.id];
            const sub =
              voice && voice.device !== "running"
                ? voice.device.startsWith("error")
                  ? voice.device.replace(/^error: /, "")
                  : voice.device === "starting"
                    ? "Starting…"
                    : voice.device
                : labels[s.id] ?? (s.kind === "media" ? "Media file audio" : "No audio device");
            const tone = voice ? (voice.device === "running" ? "ok" : voice.device.startsWith("error") ? "bad" : "warn") : "ok";
            return (
              <div key={s.id} className="strip">
                <Row
                  icon={<Dot tone={tone} />}
                  title={s.name}
                  subtitle={sub}
                  trailing={
                    <>
                      <button
                        type="button"
                        className={"btn small " + (ch?.mute ? "danger" : "secondary")}
                        onClick={() => void setChannelMute(s.id, !(ch?.mute ?? false))}
                        aria-pressed={ch?.mute ?? false}
                      >
                        {ch?.mute ? "Muted" : "Mute"}
                      </button>
                      {s.kind === "audioinput" && (
                        <button type="button" className="btn small secondary" onClick={() => void sourceRemove(s.id)} title="Remove microphone">
                          Remove
                        </button>
                      )}
                    </>
                  }
                />
                <div className="pad strip-body">
                  <Meter level={mixer.meters[s.id] ?? 0} />
                  <Range label="Volume" value={ch?.gain ?? 1} min={0} max={2} step={0.01} neutral={1} format={pct} onChange={(v) => setChannelGain(s.id, v)} />
                </div>
              </div>
            );
          })}
          <Field label="Add microphone">
            <span className="inline-controls">
              <select value={mic} onChange={(e) => setMic(e.target.value)} disabled={!mics?.length}>
                {mics === null && <option value="">Looking…</option>}
                {mics?.length === 0 && <option value="">None found</option>}
                {mics?.map((d) => (
                  <option key={d} value={d}>
                    {d}
                  </option>
                ))}
              </select>
              <button type="button" className="btn small" disabled={busy || !mic} onClick={() => void addMic()}>
                Add
              </button>
            </span>
          </Field>
          {err && (
            <div className="pad">
              <Banner tone="error" onClose={() => setErr(null)}>
                {err}
              </Banner>
            </div>
          )}
        </Section>

        <Section title="Master">
          <div className="pad">
            <Range label="Volume" value={mixer.master.gain} min={0} max={2} step={0.01} neutral={1} format={pct} onChange={setMasterGain} />
          </div>
          <Row title="Mute output" trailing={<Switch checked={mixer.master.mute} onChange={(v) => void setMasterMute(v)} label="Mute output" />} />
          <Row title="Listen on speakers" subtitle="Local monitoring only — not sent to the stream" trailing={<Switch checked={mixer.monitor_enabled} onChange={(v) => void setMonitorEnabled(v)} label="Listen on speakers" />} />
        </Section>

        <Section
          title="A/V sync"
          footer={
            av.auto
              ? "Automatic: each microphone is delayed to match the measured delay of its camera picture. The offset fine-tunes on top (+ = audio later)."
              : "Manual: audio is delayed by the offset only (negative values have no effect)."
          }
        >
          <Row title="Automatic sync" trailing={<Switch checked={av.auto} onChange={(v) => setAvSync(v, av.offset_ms)} label="Automatic sync" />} />
          <div className="pad">
            <Range
              label="Offset"
              value={av.offset_ms}
              min={-100}
              max={100}
              step={1}
              neutral={0}
              format={(v) => `${v > 0 ? "+" : ""}${Math.round(v)} ms`}
              onChange={(v) => setAvSync(av.auto, Math.round(v))}
            />
          </div>
          {Object.entries(mixer.voice).map(([id, v]) =>
            v.audio_age_ms != null ? (
              <Row
                key={id}
                title={sources.find((s) => s.id === id)?.name ?? "Microphone"}
                subtitle={v.target_age_ms != null ? `Delayed to match picture: ${Math.round(v.target_age_ms)} ms` : "No picture to match"}
                trailing={<span className="mono">{Math.round(v.audio_age_ms)} ms</span>}
              />
            ) : null,
          )}
        </Section>

        <Section title="Voice enhancer" footer="Runs on this computer in real time (no internet, ~20 ms). Applies to microphones and camera audio; media files are left untouched.">
          <Row title="Enhance voices" trailing={<Switch checked={enh.enabled} onChange={(v) => setEnh({ enabled: v })} label="Enhance voices" />} />
          <div className={enh.enabled ? "" : "disabled-block"}>
            <Row title="AI noise removal" subtitle="Fans, hum, keyboard and room noise" trailing={<Switch checked={enh.ai_denoise} disabled={!enh.enabled} onChange={(v) => setEnh({ ai_denoise: v })} label="AI noise removal" />} />
            <div className="pad">
              <Range label="Strength" value={enh.ai_strength} min={0} max={1} step={0.05} neutral={1} format={pct} disabled={!enh.enabled || !enh.ai_denoise} onChange={(v) => setEnh({ ai_strength: v })} />
            </div>
            <Row title="Noise gate" trailing={<Switch checked={enh.gate_enabled} disabled={!enh.enabled} onChange={(v) => setEnh({ gate_enabled: v })} label="Noise gate" />} />
            <div className="pad">
              <Range label="Threshold" value={enh.gate_threshold_db} min={-60} max={0} step={1} neutral={-40} format={(v) => `${Math.round(v)} dB`} disabled={!enh.enabled || !enh.gate_enabled} onChange={(v) => setEnh({ gate_threshold_db: v })} />
              <Range label="Low" value={enh.eq_low_db} min={-12} max={12} step={0.5} neutral={0} format={db} disabled={!enh.enabled} onChange={(v) => setEnh({ eq_low_db: v })} />
              <Range label="Mid" value={enh.eq_mid_db} min={-12} max={12} step={0.5} neutral={0} format={db} disabled={!enh.enabled} onChange={(v) => setEnh({ eq_mid_db: v })} />
              <Range label="High" value={enh.eq_high_db} min={-12} max={12} step={0.5} neutral={0} format={db} disabled={!enh.enabled} onChange={(v) => setEnh({ eq_high_db: v })} />
            </div>
            <Row title="Compressor" subtitle="Gentle levelling" trailing={<Switch checked={enh.compressor_enabled} disabled={!enh.enabled} onChange={(v) => setEnh({ compressor_enabled: v })} label="Compressor" />} />
          </div>
        </Section>
      </aside>
    </div>
  );
}
