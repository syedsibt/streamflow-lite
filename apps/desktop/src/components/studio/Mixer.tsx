import { useEffect, useMemo, useState } from "react";
import { api } from "../../lib/ipc";
import { errText, useProduction } from "../../lib/productionStore";
import MeterCanvas from "./MeterCanvas";
import { FxDialog } from "./AudioFx";
import type { FxSettings } from "../../lib/types";

/** Number of effects switched on (0 when the chain is bypassed). */
export function fxCount(fx: FxSettings | undefined): number {
  if (!fx || !fx.enabled) return 0;
  return [fx.highpass_enabled, fx.lowpass_enabled, fx.denoise_enabled, fx.gate_enabled, fx.eq_enabled, fx.deesser_enabled, fx.compressor_enabled, fx.limiter_enabled, fx.delay_enabled, fx.invert_polarity || fx.mono].filter(Boolean).length;
}
import { subscribeMeters } from "./meters";

/** Source ids that currently have an audio stream (from the live meters). */
function useSignalPaths(): Set<string> {
  const [key, setKey] = useState("");
  useEffect(
    () =>
      subscribeMeters((m) => {
        const k = m.channels
          .filter((c) => c.has_signal_path)
          .map((c) => c.id)
          .sort()
          .join(",");
        setKey((prev) => (prev === k ? prev : k));
      }),
    [],
  );
  return useMemo(() => new Set(key ? key.split(",") : []), [key]);
}

const gainToDb = (g: number) => (g > 0.0001 ? 20 * Math.log10(g) : -Infinity);
const fmtDb = (db: number) => (db === -Infinity ? "−∞" : `${db > 0 ? "+" : ""}${db.toFixed(1)}`);

/**
 * AUDIO MIXER: one strip per audio source (microphones, camera / capture
 * audio, media files) plus the master. Meters are the mixer's real post-
 * fader levels; solo only affects the local monitor, never the program.
 */
export default function Mixer() {
  const state = useProduction((s) => s.state);
  const setGain = useProduction((s) => s.setChannelGain);
  const setMute = useProduction((s) => s.setChannelMute);
  const setPan = useProduction((s) => s.setChannelPan);
  const setSolo = useProduction((s) => s.setChannelSolo);
  const setTrim = useProduction((s) => s.setChannelTrim);
  const setMasterGain = useProduction((s) => s.setMasterGain);
  const setMasterMute = useProduction((s) => s.setMasterMute);
  const setMonitor = useProduction((s) => s.setMonitorEnabled);
  const sourceAdd = useProduction((s) => s.sourceAdd);
  const select = useProduction((s) => s.selectSource);
  const selected = useProduction((s) => s.selectedSourceId);
  const paths = useSignalPaths();
  const [adding, setAdding] = useState(false);
  const [mics, setMics] = useState<string[] | null>(null);
  const [micErr, setMicErr] = useState<string | null>(null);
  const [fxFor, setFxFor] = useState<string | null>(null);

  const mixer = state?.mixer;
  const strips = useMemo(() => {
    if (!state) return [];
    const seen = new Set<string>();
    return state.scenes
      .flatMap((sc) => sc.sources)
      .filter((s) => {
        if (seen.has(s.id)) return false;
        seen.add(s.id);
        return s.kind === "audioinput" || paths.has(s.id);
      });
  }, [state, paths]);

  useEffect(() => {
    if (!adding) return;
    setMics(null);
    setMicErr(null);
    void api
      .listAudioDevices()
      .then(setMics)
      .catch((e) => {
        setMics([]);
        setMicErr(errText(e));
      });
  }, [adding]);

  if (!mixer) return null;
  const anySolo = mixer.channels.some((c) => c.solo);

  return (
    <>
    <div className="panel-head">
      <h2>Audio mixer</h2>
      <button type="button" className="tool" onClick={() => setFxFor(strips.find((s) => mixer.channels.some((c) => c.source_id === s.id))?.id ?? "master")} title="Effects of every channel and the master, each with its own on/off">
        Audio settings
      </button>
    </div>
    <div className="mixer">
      {fxFor && <FxDialog initial={fxFor} channels={strips.filter((s) => mixer.channels.some((c) => c.source_id === s.id)).map((s) => ({ id: s.id, name: s.name }))} onClose={() => setFxFor(null)} />}
      <div className="mixer-chs">
        {strips.length === 0 && (
          <div className="panel-empty">
            No audio yet. Add a microphone, or a camera / file that has sound.
          </div>
        )}
        {strips.map((s) => {
          const ch = mixer.channels.find((c) => c.source_id === s.id);
          if (!ch) return null;
          const voice = mixer.voice[s.id];
          const status = voice
            ? voice.device === "running"
              ? "Receiving"
              : voice.device.replace(/^error: /, "")
            : s.kind === "media"
              ? "File audio"
              : "Receiving";
          const bad = voice?.device.startsWith("error");
          const db = gainToDb(ch.gain);
          return (
            <div
              key={s.id}
              className={"ch" + (selected === s.id ? " selected" : "") + (ch.mute ? " muted" : "")}
              onMouseDown={() => select(s.id)}
            >
              <div className="ch-name" title={s.name}>
                {s.name}
              </div>
              <div className={"ch-status" + (bad ? " bad" : "")} title={status}>
                {status}
              </div>
              <label className="ch-knob" title="Input trim (double-click resets)">
                <span>Trim</span>
                <input
                  type="range"
                  min={-20}
                  max={20}
                  step={0.5}
                  value={ch.trim_db}
                  onChange={(e) => setTrim(s.id, Number(e.target.value))}
                  onDoubleClick={() => setTrim(s.id, 0)}
                  aria-label={`${s.name} trim`}
                />
                <output>{fmtDb(ch.trim_db)}</output>
              </label>
              <div className="ch-body">
                <MeterCanvas channel={s.id} title={s.name} />
                <input
                  className="fader"
                  type="range"
                  min={0}
                  max={2}
                  step={0.005}
                  value={ch.gain}
                  onChange={(e) => setGain(s.id, Number(e.target.value))}
                  onDoubleClick={() => setGain(s.id, 1)}
                  aria-label={`${s.name} level`}
                  title="Level (double-click: 0 dB)"
                />
              </div>
              <div className="ch-db">{fmtDb(db)} dB</div>
              <label className="ch-pan" title="Balance (double-click centres)">
                <input
                  type="range"
                  min={-1}
                  max={1}
                  step={0.05}
                  value={ch.pan}
                  onChange={(e) => setPan(s.id, Number(e.target.value))}
                  onDoubleClick={() => setPan(s.id, 0)}
                  aria-label={`${s.name} balance`}
                />
                <output>{ch.pan === 0 ? "C" : ch.pan < 0 ? `L${Math.round(-ch.pan * 100)}` : `R${Math.round(ch.pan * 100)}`}</output>
              </label>
              <button type="button" className={"sq fx" + (fxCount(ch.fx) ? " on" : "")} onClick={() => setFxFor(s.id)} title="Audio settings: noise removal, gate, EQ, compressor, limiter…">
                FX{fxCount(ch.fx) ? ` · ${fxCount(ch.fx)}` : ""}
              </button>
              <div className="ch-buttons">
                <button type="button" className={"sq mute" + (ch.mute ? " on" : "")} aria-pressed={ch.mute} onClick={() => void setMute(s.id, !ch.mute)} title="Mute (stream and recording)">
                  Mute
                </button>
                <button type="button" className={"sq solo" + (ch.solo ? " on" : "")} aria-pressed={ch.solo} onClick={() => void setSolo(s.id, !ch.solo)} title="Solo — your speakers/headphones only; the stream is unchanged">
                  Solo
                </button>
              </div>
            </div>
          );
        })}
        <div className="ch add-ch">
          {!adding ? (
            <button type="button" className="add-mic" onClick={() => setAdding(true)}>
              Add microphone
            </button>
          ) : (
            <div className="add-mic-form">
              <select
                autoFocus
                defaultValue=""
                disabled={!mics?.length}
                onChange={async (e) => {
                  const d = e.target.value;
                  if (!d) return;
                  setAdding(false);
                  await sourceAdd({ audio_input: { device: d } }, d);
                }}
              >
                <option value="">{mics === null ? "Looking…" : mics.length ? "Choose…" : "None found"}</option>
                {mics?.map((d) => (
                  <option key={d} value={d}>
                    {d}
                  </option>
                ))}
              </select>
              {micErr && <div className="ch-status bad">{micErr}</div>}
              <button type="button" className="link" onClick={() => setAdding(false)}>
                Cancel
              </button>
            </div>
          )}
        </div>
      </div>
      <div className={"ch master" + (mixer.master.mute ? " muted" : "")}>
        <div className="ch-name">Master</div>
        <div className="ch-status">{anySolo ? "Monitor: solo" : mixer.monitor_enabled ? "Monitor on" : "Program mix"}</div>
        <div className="ch-body">
          <MeterCanvas channel="master" title="Master (program)" />
          <input
            className="fader"
            type="range"
            min={0}
            max={2}
            step={0.005}
            value={mixer.master.gain}
            onChange={(e) => setMasterGain(Number(e.target.value))}
            onDoubleClick={() => setMasterGain(1)}
            aria-label="Master level"
          />
        </div>
        <div className="ch-db">{fmtDb(gainToDb(mixer.master.gain))} dB</div>
        <button type="button" className={"sq fx" + (fxCount(mixer.master.fx) ? " on" : "")} onClick={() => setFxFor("master")} title="Master effects: limiter, compressor, EQ">
          FX{fxCount(mixer.master.fx) ? ` · ${fxCount(mixer.master.fx)}` : ""}
        </button>
        <div className="ch-buttons">
          <button type="button" className={"sq mute" + (mixer.master.mute ? " on" : "")} aria-pressed={mixer.master.mute} onClick={() => void setMasterMute(!mixer.master.mute)}>
            Mute
          </button>
          <button
            type="button"
            className={"sq mon" + (mixer.monitor_enabled ? " on" : "")}
            aria-pressed={mixer.monitor_enabled}
            onClick={() => void setMonitor(!mixer.monitor_enabled)}
            title="Listen on this computer's speakers/headphones (not sent to the stream)"
          >
            Listen
          </button>
        </div>
      </div>
    </div>
    </>
  );
}
