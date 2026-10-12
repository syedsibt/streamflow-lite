import { useEffect, useMemo, useState } from "react";
import { useProduction } from "../lib/productionStore";
import { useStudio } from "../lib/store";
import { api } from "../lib/ipc";
import type { EnhancerSettings } from "../lib/types";

/**
 * Dedicated Audio view: the complete audio control panel.
 *
 * - Every audio-capable source (audio inputs, cameras with audio, media
 *   files) gets a full channel strip: gain, mute, live meter, device label.
 * - Add/remove audio inputs right here (no need to visit Sources).
 * - Master strip: gain, mute, speaker monitoring toggle.
 *
 * Meters are real: they come from the backend's sf-audio mixer and read 0
 * until actual audio flows — never faked.
 */

const AUDIO_KINDS = new Set(["audio_input", "camera", "media"]);

export default function AudioView() {
  const state = useProduction((s) => s.state);
  const refresh = useProduction((s) => s.refresh);
  const sourceAdd = useProduction((s) => s.sourceAdd);
  const sourceRemove = useProduction((s) => s.sourceRemove);
  const setChannelGain = useProduction((s) => s.setChannelGain);
  const setChannelMute = useProduction((s) => s.setChannelMute);
  const setMasterGain = useProduction((s) => s.setMasterGain);
  const setMasterMute = useProduction((s) => s.setMasterMute);
  const setMonitorEnabled = useProduction((s) => s.setMonitorEnabled);
  const setEnhancerSettings = useProduction((s) => s.setEnhancerSettings);

  const [devices, setDevices] = useState<string[]>([]);
  const [device, setDevice] = useState<string>("");
  const [devLabels, setDevLabels] = useState<Record<string, string>>({});
  const [name, setName] = useState("");

  const mixer = state?.mixer ?? null;

  // All audio-capable sources across scenes.
  const audioSources = useMemo(() => {
    const out: { id: string; name: string; kind: string }[] = [];
    for (const sc of state?.scenes ?? []) {
      for (const src of sc.sources) {
        if (AUDIO_KINDS.has(src.kind)) {
          out.push({ id: src.id, name: src.name, kind: src.kind });
        }
      }
    }
    return out;
  }, [state?.scenes]);

  // Load input devices once.
  useEffect(() => {
    let cancelled = false;
    void api
      .listAudioDevices()
      .then((list) => {
        if (cancelled) return;
        setDevices(list);
        setDevice((prev) => prev || list[0] || "");
      })
      .catch(() => {
        if (!cancelled) setDevices([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Device labels for the listed sources.
  useEffect(() => {
    let cancelled = false;
    const ids = audioSources.map((s) => s.id).join(",");
    if (!ids) {
      setDevLabels({});
      return;
    }
    void (async () => {
      const labels: Record<string, string> = {};
      for (const s of audioSources) {
        try {
          const d = await api.sourceAudioDevice(s.id);
          if (d) labels[s.id] = d;
        } catch {
          /* leave blank */
        }
        if (cancelled) return;
      }
      if (!cancelled) setDevLabels(labels);
    })();
    return () => {
      cancelled = true;
    };
  }, [audioSources.map((s) => s.id).join(",")]); // eslint-disable-line react-hooks/exhaustive-deps

  const addInput = () => {
    const sceneId = state?.preview_scene_id;
    if (!sceneId) {
      useStudio.getState().pushToast("No preview scene selected");
      return;
    }
    const label = name.trim() || device || "Microphone";
    void sourceAdd(sceneId, { audio_input: { device: device || null } }, label).then(
      () => {
        setName("");
        void refresh();
      }
    );
  };

  const removeSource = (sourceId: string) => {
    const sc = sceneOf(sourceId);
    if (sc) void sourceRemove(sc, sourceId).then(() => void refresh());
  };

  const sceneOf = (sourceId: string) =>
    state?.scenes.find((sc) => sc.sources.some((s) => s.id === sourceId))?.id ?? null;

  if (!mixer) return null;

  const enh: EnhancerSettings = mixer.enhancer;
  const setEnh = (patch: Partial<EnhancerSettings>) =>
    void setEnhancerSettings({ ...enh, ...patch });

  return (
    <div className="view audio-view">
      <div className="panel">
        <div className="panel-head">
          <span className="panel-title">Audio Inputs</span>
          <span className="note">
            {mixer.meters_live ? "Live levels" : "Meters idle — add an input or play media with audio"}
          </span>
        </div>
        <div className="panel-body">
          {audioSources.length === 0 && (
            <div className="note">No audio sources yet — add one below.</div>
          )}
          {audioSources.map((src) => {
            const ch = mixer.channels.find((c) => c.source_id === src.id);
            const level = mixer.meters[src.id] ?? 0;
            return (
              <div key={src.id} className="mixer-strip audio-strip">
                <div className="mixer-name" title={src.id}>
                  {src.name}
                  <span className="kind-tag">{src.kind}</span>
                </div>
                {devLabels[src.id] && (
                  <div className="note" title="Audio device">
                    🎙 {devLabels[src.id]}
                  </div>
                )}
                <div className="mixer-row">
                  <input
                    type="range"
                    min={0}
                    max={2}
                    step={0.01}
                    value={ch?.gain ?? 1}
                    onChange={(e) => setChannelGain(src.id, parseFloat(e.target.value))}
                    title="Channel gain 0–200%"
                  />
                  <span className="value">{Math.round((ch?.gain ?? 1) * 100)}%</span>
                  <button
                    className={ch?.mute ? "icon-btn mute-on" : "icon-btn"}
                    onClick={() => void setChannelMute(src.id, !(ch?.mute ?? false))}
                    title={ch?.mute ? "Unmute" : "Mute"}
                  >
                    M
                  </button>
                  <button
                    className="icon-btn"
                    onClick={() => removeSource(src.id)}
                    title="Remove source"
                  >
                    ✕
                  </button>
                </div>
                <div className="meter" title={mixer.meters_live ? "Live level" : "No audio flowing"}>
                  <div
                    className="meter-fill"
                    style={{ width: `${Math.min(100, level * 100)}%` }}
                  />
                </div>
              </div>
            );
          })}
          <div className="add-row">
            <select
              value={device}
              onChange={(e) => setDevice(e.target.value)}
              title="Microphone / line-in device"
            >
              {devices.length === 0 && <option value="">No audio inputs found</option>}
              {devices.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
            <input
              type="text"
              placeholder="Name (optional)"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
            <button className="btn ghost" onClick={addInput}>
              Add input
            </button>
          </div>
        </div>
      </div>

      <div className="panel">
        <div className="panel-head">
          <span className="panel-title">Master</span>
        </div>
        <div className="panel-body">
          <div className="mixer-strip master">
            <div className="mixer-row">
              <input
                type="range"
                min={0}
                max={2}
                step={0.01}
                value={mixer.master.gain}
                onChange={(e) => setMasterGain(parseFloat(e.target.value))}
                title="Master gain 0–200%"
              />
              <span className="value">{Math.round(mixer.master.gain * 100)}%</span>
              <button
                className={mixer.master.mute ? "icon-btn mute-on" : "icon-btn"}
                onClick={() => void setMasterMute(!mixer.master.mute)}
                title={mixer.master.mute ? "Unmute" : "Mute"}
              >
                M
              </button>
              <button
                className={mixer.monitor_enabled ? "icon-btn mute-on" : "icon-btn"}
                onClick={() => void setMonitorEnabled(!mixer.monitor_enabled)}
                title={
                  mixer.monitor_enabled
                    ? "Stop monitoring on speakers"
                    : "Monitor the master mix on your speakers"
                }
              >
                🔊
              </button>
            </div>
            <div className="note">
              {mixer.monitor_enabled
                ? "Monitoring the master mix on your speakers."
                : "Speaker monitoring is off."}
            </div>
          </div>
        </div>
      </div>

      <div className="panel">
        <div className="panel-head">
          <span className="panel-title">Audio Enhancer</span>
          <button
            className={enh.enabled ? "icon-btn mute-on" : "icon-btn"}
            onClick={() => setEnh({ enabled: !enh.enabled })}
            title={enh.enabled ? "Bypass enhancer" : "Enable enhancer"}
          >
            {enh.enabled ? "ON" : "OFF"}
          </button>
        </div>
        <div className="panel-body">
          <div className="note" style={{ marginBottom: 8 }}>
            Free offline DSP — noise gate → 3-band EQ → compressor. No cloud, no keys.
          </div>
          <div className={`mixer-strip ${enh.enabled ? "" : "disabled"}`}>
            <div className="mixer-row">
              <span className="mixer-name">Noise gate</span>
              <button
                className={enh.gate_enabled ? "icon-btn mute-on" : "icon-btn"}
                onClick={() => setEnh({ gate_enabled: !enh.gate_enabled })}
                title="Silence background hiss when nobody is speaking"
              >
                {enh.gate_enabled ? "ON" : "OFF"}
              </button>
            </div>
            <div className="mixer-row">
              <span className="note">Threshold</span>
              <input
                type="range"
                min={-60}
                max={0}
                step={1}
                value={enh.gate_threshold_db}
                onChange={(e) => setEnh({ gate_threshold_db: parseFloat(e.target.value) })}
                title="Gate threshold dB"
              />
              <span className="value">{Math.round(enh.gate_threshold_db)} dB</span>
            </div>
            <div className="mixer-row" style={{ marginTop: 8 }}>
              <span className="mixer-name">EQ Low</span>
              <input
                type="range"
                min={-12}
                max={12}
                step={0.5}
                value={enh.eq_low_db}
                onChange={(e) => setEnh({ eq_low_db: parseFloat(e.target.value) })}
                title="Lowshelf 200 Hz"
              />
              <span className="value">{enh.eq_low_db.toFixed(1)} dB</span>
            </div>
            <div className="mixer-row">
              <span className="mixer-name">EQ Mid</span>
              <input
                type="range"
                min={-12}
                max={12}
                step={0.5}
                value={enh.eq_mid_db}
                onChange={(e) => setEnh({ eq_mid_db: parseFloat(e.target.value) })}
                title="Peaking 1 kHz — voice clarity"
              />
              <span className="value">{enh.eq_mid_db.toFixed(1)} dB</span>
            </div>
            <div className="mixer-row">
              <span className="mixer-name">EQ High</span>
              <input
                type="range"
                min={-12}
                max={12}
                step={0.5}
                value={enh.eq_high_db}
                onChange={(e) => setEnh({ eq_high_db: parseFloat(e.target.value) })}
                title="Highshelf 6 kHz"
              />
              <span className="value">{enh.eq_high_db.toFixed(1)} dB</span>
            </div>
            <div className="mixer-row" style={{ marginTop: 8 }}>
              <span className="mixer-name">Compressor</span>
              <button
                className={enh.compressor_enabled ? "icon-btn mute-on" : "icon-btn"}
                onClick={() => setEnh({ compressor_enabled: !enh.compressor_enabled })}
                title="Gentle 3:1 levelling (−18 dB threshold)"
              >
                {enh.compressor_enabled ? "ON" : "OFF"}
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
