import { useEffect, useState, type ReactNode } from "react";
type OTab = "stream" | "record" | "format";
import { save } from "@tauri-apps/plugin-dialog";
import { api } from "../../lib/ipc";
import { errText, useProduction } from "../../lib/productionStore";
import type { PlatformInfo, VideoSettings } from "../../lib/types";
import { Banner, Field, Row, Section, Segmented } from "../ui";

const FALLBACK_PLATFORMS: PlatformInfo[] = [
  { id: "youtube", name: "YouTube", default_server: "rtmp://a.rtmp.youtube.com/live2", key_help: "YouTube Studio → Go live → Stream key", latency_note: "" },
  { id: "facebook", name: "Facebook", default_server: "rtmps://live-api-s.facebook.com:443/rtmp/", key_help: "Live Producer → Stream key", latency_note: "" },
  { id: "tiktok", name: "TikTok", default_server: null, key_help: "TikTok LIVE Center → Server URL + Stream key", latency_note: "" },
  { id: "custom_rtmp", name: "Custom", default_server: null, key_help: "rtmp:// or rtmps:// server and key", latency_note: "" },
];

const ENCODERS = [
  { id: "auto", label: "Automatic (best available)" },
  { id: "h264_nvenc", label: "NVIDIA NVENC" },
  { id: "h264_qsv", label: "Intel Quick Sync" },
  { id: "h264_amf", label: "AMD AMF" },
  { id: "libx264", label: "x264 (CPU)" },
];

/** Output sizes (all 16:9, even dimensions) — must match `VIDEO_OUTPUT_SIZES`. */
const OUTPUT_SIZES = [
  { label: "360p", w: 640, h: 360 },
  { label: "480p", w: 854, h: 480 },
  { label: "540p", w: 960, h: 540 },
  { label: "720p", w: 1280, h: 720 },
  { label: "1080p", w: 1920, h: 1080 },
  { label: "1440p", w: 2560, h: 1440 },
];

/** Recommended H.264 bitrate range (kbps) by height and frame rate (YouTube guidance). */
function bitrateRange(h: number, fps: number): [number, number] {
  const hi = fps > 30;
  if (h <= 360) return hi ? [600, 1500] : [400, 1000];
  if (h <= 480) return hi ? [800, 2500] : [500, 2000];
  if (h <= 540) return hi ? [1500, 3500] : [1000, 2500];
  if (h <= 720) return hi ? [3500, 6000] : [2500, 4000];
  if (h <= 1080) return hi ? [6000, 9000] : [4500, 9000];
  return hi ? [13000, 24000] : [9000, 18000];
}

/** Like `Field`, but a <div>: a <label> around several buttons would
 * forward clicks on its empty space to the first button. */
function FieldBox({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="field" role="group" aria-label={label}>
      <span className="field-label">{label}</span>
      <span className="field-control">{children}</span>
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}

const BITRATE_MIN = 300;
const BITRATE_MAX = 60000;

/** Bitrate: quick picks from the recommended range, or any custom value. */
function BitrateField({ vs, setVs, apply }: { vs: VideoSettings; setVs: (v: VideoSettings) => void; apply: (v: VideoSettings) => Promise<void> }) {
  const [lo, hi] = bitrateRange(vs.output_height, vs.output_fps);
  const mid = Math.round((lo + hi) / 2 / 100) * 100;
  const picks = [
    { label: "Low", v: lo },
    { label: "Recommended", v: mid },
    { label: "High", v: hi },
  ];
  const [text, setText] = useState(String(vs.bitrate_kbps));
  useEffect(() => setText(String(vs.bitrate_kbps)), [vs.bitrate_kbps]);
  const commit = () => {
    const n = Math.round(Number(text));
    if (!Number.isFinite(n) || n < BITRATE_MIN || n > BITRATE_MAX) {
      setText(String(vs.bitrate_kbps));
      return;
    }
    if (n !== vs.bitrate_kbps) void apply({ ...vs, bitrate_kbps: n });
  };
  const b = vs.bitrate_kbps;
  const note = b > hi * 1.25 ? `More than ${vs.output_height}p needs (${lo}–${hi}) — uses upload and encoder time for nothing.` : b < lo * 0.8 ? `Below the usual ${lo}–${hi} for ${vs.output_height}p${vs.output_fps > 30 ? "60" : "30"} — the picture may look blocky.` : null;
  return (
    <FieldBox label="Bitrate" hint={`Recommended for ${vs.output_height}p${vs.output_fps > 30 ? "60" : "30"}: ${lo}–${hi} kbps · any value ${BITRATE_MIN}–${BITRATE_MAX}`}>
      <div className="bitrate">
        <div className="bitrate-picks">
          {picks.map((p) => (
            <button key={p.label} type="button" className={"tool" + (b === p.v ? " on" : "")} onClick={() => void apply({ ...vs, bitrate_kbps: p.v })}>
              {p.label} <small>{p.v}</small>
            </button>
          ))}
        </div>
        <span className="inline-controls">
          <input
            type="number"
            aria-label="Custom bitrate"
            min={BITRATE_MIN}
            max={BITRATE_MAX}
            step={100}
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              const n = Number(e.target.value);
              if (Number.isFinite(n)) setVs({ ...vs, bitrate_kbps: n });
            }}
            onBlur={commit}
            onKeyDown={(e) => e.key === "Enter" && commit()}
          />
          <span className="unit">kbps (custom)</span>
        </span>
        {note && <small className="bitrate-note">{note}</small>}
      </div>
    </FieldBox>
  );
}

const fmtTime = (s: number) => {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = Math.floor(s % 60);
  return `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
};

/**
 * STREAM: where the show goes (YouTube, Facebook, TikTok or any RTMP(S)
 * server), the output format, and recording. Stream and recording share
 * one encoder — starting both costs no extra encoding.
 */
export default function OutputPanel() {
  const [otab, setOtab] = useState<OTab>("stream");
  const st = useProduction((s) => s.state);
  const platformsRaw = useProduction((s) => s.platforms);
  const platform = useProduction((s) => s.streamPlatform);
  const url = useProduction((s) => s.streamUrl);
  const key = useProduction((s) => s.streamKey);
  const streamError = useProduction((s) => s.streamError);
  const busyStream = useProduction((s) => !!s.busy.stream);
  const setPlatform = useProduction((s) => s.setStreamPlatform);
  const setUrl = useProduction((s) => s.setStreamUrl);
  const setKey = useProduction((s) => s.setStreamKey);
  const streamStart = useProduction((s) => s.streamStart);
  const streamStop = useProduction((s) => s.streamStop);
  const clearStreamError = useProduction((s) => s.clearStreamError);
  const recordPath = useProduction((s) => s.recordPath);
  const recordFormat = useProduction((s) => s.recordFormat);
  const recordError = useProduction((s) => s.recordError);
  const setRecordPath = useProduction((s) => s.setRecordPath);
  const setRecordFormat = useProduction((s) => s.setRecordFormat);
  const recordStart = useProduction((s) => s.recordStart);
  const recordStop = useProduction((s) => s.recordStop);
  const recordPause = useProduction((s) => s.recordPause);
  const recordResume = useProduction((s) => s.recordResume);
  const clearRecordError = useProduction((s) => s.clearRecordError);
  const refresh = useProduction((s) => s.refresh);
  const diag = useProduction((s) => s.diag);

  const [showKey, setShowKey] = useState(false);
  const [vs, setVs] = useState<VideoSettings | null>(null);
  const [outErr, setOutErr] = useState<string | null>(null);

  const platforms = platformsRaw.length ? platformsRaw : FALLBACK_PLATFORMS;
  const info = platforms.find((p) => p.id === platform) ?? platforms[0];
  const stream = st?.stream.state ?? "idle";
  const active = stream === "live" || stream === "connecting" || stream === "reconnecting";
  const rec = st?.record.state ?? "idle";
  const recActive = rec === "recording" || rec === "paused";

  useEffect(() => {
    if (st?.video_settings) setVs(st.video_settings);
  }, [st?.video_settings]);

  const applyOutput = async (next: VideoSettings) => {
    setVs(next);
    setOutErr(null);
    try {
      await api.videoSettingsSet(next);
      await refresh();
    } catch (e) {
      setOutErr(errText(e));
    }
  };

  const chooseFile = async () => {
    try {
      const ext = recordFormat === "mkv" ? "mkv" : "mp4";
      const picked = await save({
        title: "Save recording as",
        defaultPath: `StreamFlow ${new Date().toISOString().slice(0, 16).replace("T", " ").replace(":", "-")}.${ext}`,
        filters: [{ name: ext.toUpperCase(), extensions: [ext] }],
      });
      if (typeof picked === "string") setRecordPath(picked);
    } catch (e) {
      useProduction.setState({ recordError: `File picker unavailable: ${errText(e)}` });
    }
  };

  const outputLocked = active || recActive;

  return (
    <div className="output">
      <div className="ptabs" role="tablist" aria-label="Output">
        {(
          [
            ["stream", stream === "live" ? "Stream · live" : "Stream"],
            ["record", rec === "recording" ? "Record · on" : "Record"],
            ["format", "Video format"],
          ] as [OTab, string][]
        ).map(([id, label]) => (
          <button key={id} type="button" role="tab" aria-selected={otab === id} className={otab === id ? "on" : ""} onClick={() => setOtab(id)}>
            {label}
          </button>
        ))}
      </div>
      <div className="pbody">
        {otab === "stream" && (
        <Section footer={info?.latency_note || undefined}>
          <div className="pad">
            <Segmented
              label="Platform"
              value={platform}
              onChange={(v) => !active && setPlatform(v)}
              options={platforms.map((p) => ({ value: p.id, label: p.name === "Custom RTMP" ? "Custom" : p.name }))}
            />
          </div>
          <Field label="Server" hint={info?.default_server ? "Leave empty to use the default server." : undefined}>
            <input
              type="text"
              value={url}
              disabled={active}
              spellCheck={false}
              placeholder={info?.default_server ?? "rtmp://…"}
              onChange={(e) => setUrl(e.target.value)}
            />
          </Field>
          <Field label="Stream key" hint={info?.key_help}>
            <span className="inline-controls">
              <input
                type={showKey ? "text" : "password"}
                value={key}
                disabled={active}
                spellCheck={false}
                autoComplete="off"
                placeholder="Paste your stream key"
                onChange={(e) => setKey(e.target.value)}
              />
              <button type="button" className="btn small secondary" onClick={() => setShowKey((v) => !v)}>
                {showKey ? "Hide" : "Show"}
              </button>
            </span>
          </Field>
          {streamError && (
            <div className="pad">
              <Banner tone="error" onClose={clearStreamError}>
                {streamError}
              </Banner>
            </div>
          )}
          <div className="pad actions">
            {active ? (
              <button type="button" className="btn big danger" onClick={() => void streamStop()}>
                {stream === "live" ? "End stream" : "Cancel"}
              </button>
            ) : (
              <button type="button" className="btn big go" disabled={busyStream} onClick={() => void streamStart()}>
                {busyStream ? "Starting…" : "Go live"}
              </button>
            )}
          </div>
          {stream === "live" && diag && (
            <Row
              title="Live"
              subtitle={`${Math.round(diag.stream_kbps)} kbps · ${diag.encoder?.name ?? ""}`}
              trailing={<span className="mono">{fmtTime(diag.stream_uptime_s)}</span>}
            />
          )}
        </Section>
        )}

        {otab === "format" && (
        <Section footer={outputLocked ? "Resolution, encoder and bitrate apply the next time you go live or record. Frame rate applies immediately." : "Stream and recording share one encoder."}>
          {vs && (
            <>
              <FieldBox label="Resolution">
                <div className="res-grid" role="radiogroup" aria-label="Output resolution">
                  {OUTPUT_SIZES.map((r) => {
                    const on = vs.output_width === r.w && vs.output_height === r.h;
                    return (
                      <button
                        key={r.label}
                        type="button"
                        role="radio"
                        aria-checked={on}
                        className={on ? "on" : ""}
                        title={`${r.w}×${r.h}`}
                        onClick={() => void applyOutput({ ...vs, output_width: r.w, output_height: r.h })}
                      >
                        <b>{r.label}</b>
                        <small>
                          {r.w}×{r.h}
                        </small>
                      </button>
                    );
                  })}
                </div>
              </FieldBox>
              <Field label="Frame rate">
                <Segmented label="Frame rate" size="small" value={vs.output_fps} onChange={(v) => void applyOutput({ ...vs, output_fps: v })} options={[30, 60].map((f) => ({ value: f, label: `${f} fps` }))} />
              </Field>
              <Field label="Encoder">
                <select value={vs.encoder} onChange={(e) => void applyOutput({ ...vs, encoder: e.target.value })}>
                  {ENCODERS.map((o) => (
                    <option key={o.id} value={o.id}>
                      {o.label}
                    </option>
                  ))}
                </select>
              </Field>
              <BitrateField vs={vs} setVs={setVs} apply={applyOutput} />
              <Row title="Keyframe interval" subtitle="Fixed at 2 seconds — what YouTube, Facebook and TikTok require" trailing={<span className="mono">2 s</span>} />
              <Row title="Audio" subtitle="AAC, 48 kHz stereo" trailing={<span className="mono">128 kbps</span>} />
              {diag?.encoder && <Row title="Encoder in use" subtitle={diag.encoder.note ?? (diag.encoder.hardware ? "Hardware" : "Software (CPU)")} trailing={<span className="mono">{diag.encoder.name}</span>} />}
              {outErr && (
                <div className="pad">
                  <Banner tone="error" onClose={() => setOutErr(null)}>
                    {outErr}
                  </Banner>
                </div>
              )}
            </>
          )}
        </Section>
        )}

        {otab === "record" && (
        <Section>
          <Field label="File">
            <span className="file-pick">
              <span className="file-name" title={recordPath || undefined}>
                {recordPath ? recordPath.split(/[\\/]/).pop() : "Not chosen"}
              </span>
              <button type="button" className="btn small secondary" disabled={recActive} onClick={() => void chooseFile()}>
                Choose…
              </button>
            </span>
          </Field>
          <Field label="Format">
            <Segmented
              label="Format"
              size="small"
              value={recordFormat}
              onChange={(v) => !recActive && setRecordFormat(v)}
              options={[
                { value: "mp4", label: "MP4", title: "Fragmented MP4 — survives a crash" },
                { value: "mkv", label: "MKV" },
              ]}
            />
          </Field>
          {recordError && (
            <div className="pad">
              <Banner tone="error" onClose={clearRecordError}>
                {recordError}
              </Banner>
            </div>
          )}
          <div className="pad actions spread">
            {!recActive ? (
              <button type="button" className="btn rec-btn" onClick={() => void recordStart()}>
                Start recording
              </button>
            ) : (
              <>
                <span className="mono rec-time">
                  {rec === "paused" ? "Paused · " : ""}
                  {fmtTime(st?.record.elapsed_secs ?? 0)}
                </span>
                <span className="btn-group">
                  {rec === "recording" ? (
                    <button type="button" className="btn small secondary" onClick={() => void recordPause()}>
                      Pause
                    </button>
                  ) : (
                    <button type="button" className="btn small secondary" onClick={() => void recordResume()}>
                      Resume
                    </button>
                  )}
                  <button type="button" className="btn small danger" onClick={() => void recordStop()}>
                    Stop
                  </button>
                </span>
              </>
            )}
          </div>
        </Section>
        )}
      </div>
    </div>
  );
}
