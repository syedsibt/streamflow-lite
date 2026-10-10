import { useProduction } from "../../lib/productionStore";

/**
 * Audio mixer. Channel strips: name, gain slider 0–200%, mute button, meter
 * bar. Meters are real: the backend's sf-audio mixer computes per-source
 * peaks from decoded media audio (`meters_live` flips true once audio
 * flows); with no media sources they read 0 — never faked.
 */
export default function MixerPanel() {
  const mixer = useProduction((s) => s.state?.mixer ?? null);
  const setChannelGain = useProduction((s) => s.setChannelGain);
  const setChannelMute = useProduction((s) => s.setChannelMute);
  const setMasterGain = useProduction((s) => s.setMasterGain);
  const setMasterMute = useProduction((s) => s.setMasterMute);

  if (!mixer) return null;

  return (
    <div className="panel">
      <div className="panel-head">
        <span className="panel-title">Mixer</span>
      </div>
      <div className="panel-body mixer">
        {mixer.channels.length === 0 && (
          <div className="note">No mixer channels yet.</div>
        )}
        {mixer.channels.map((ch) => (
          <div key={ch.source_id} className="mixer-strip">
            <div className="mixer-name" title={ch.source_id}>
              {ch.name}
            </div>
            <div className="mixer-row">
              <input
                type="range"
                min={0}
                max={2}
                step={0.01}
                value={ch.gain}
                onChange={(e) =>
                  setChannelGain(ch.source_id, parseFloat(e.target.value))
                }
                title="Channel gain 0–200%"
              />
              <span className="value">{Math.round(ch.gain * 100)}%</span>
              <button
                className={ch.mute ? "icon-btn mute-on" : "icon-btn"}
                onClick={() => void setChannelMute(ch.source_id, !ch.mute)}
                title={ch.mute ? "Unmute" : "Mute"}
              >
                M
              </button>
            </div>
            <div
              className="meter"
              title={mixer.meters_live ? "Live level" : "No audio flowing"}
            >
              <div
                className="meter-fill"
                style={{
                  width: `${Math.min(
                    100,
                    (mixer.meters[ch.source_id] ?? 0) * 100
                  )}%`,
                }}
              />
            </div>
          </div>
        ))}
        <div className="mixer-strip master">
          <div className="mixer-name">Master</div>
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
          </div>
          <div className="meter" title="Meters pending audio engine">
            <div className="meter-fill" style={{ width: "0%" }} />
          </div>
        </div>
        <div className="note">
          {mixer.meters_live
            ? "Live levels from the audio mixer (media sources)."
            : "Meters idle — play a media source with audio to see levels."}
        </div>
      </div>
    </div>
  );
}
