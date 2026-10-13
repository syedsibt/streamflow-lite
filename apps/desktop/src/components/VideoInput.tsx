import { useEffect, useMemo, useState } from "react";
import { api, isTauri } from "../lib/ipc";
import { errText, useProduction } from "../lib/productionStore";
import type { CameraMode, CameraSourceInfo } from "../lib/types";

/**
 * VIDEO INPUT: what each camera / capture card actually captures (device,
 * size, frame rate, device format) — not the stream/recording output.
 *
 * Choices come from the modes the device itself reports; StreamFlow asks the
 * device for exactly that mode (a smaller capture means less USB bandwidth,
 * less decoding and less scaling) and then shows what the device really
 * delivers. If the device reports nothing, standard sizes are offered and the
 * device picks the closest.
 */

const STD_SIZES: [number, number][] = [
  [3840, 2160],
  [2560, 1440],
  [1920, 1080],
  [1280, 720],
  [960, 540],
  [854, 480],
  [640, 480],
  [640, 360],
];
const STD_FPS = [15, 24, 25, 30, 50, 60];

const gcd = (a: number, b: number): number => (b ? gcd(b, a % b) : a);
function aspect(w: number, h: number): string {
  if (!w || !h) return "";
  const r = w / h;
  if (Math.abs(r - 16 / 9) < 0.02) return "16:9";
  if (Math.abs(r - 4 / 3) < 0.02) return "4:3";
  if (Math.abs(r - 16 / 10) < 0.02) return "16:10";
  const g = gcd(w, h);
  return `${w / g}:${h / g}`;
}

const fits = (m: CameraMode, w: number, h: number) => w >= m.min_width && w <= m.max_width && h >= m.min_height && h <= m.max_height;
const fitsFps = (m: CameraMode, f: number) => f >= m.min_fps - 0.6 && f <= m.max_fps + 0.6;
const FORMAT_NAME: Record<string, string> = {
  auto: "Automatic (MJPEG first, then raw)",
  mjpeg: "MJPEG (compressed)",
  h264: "H.264 (compressed)",
  hevc: "HEVC (compressed)",
  yuyv422: "YUY2 (raw)",
  nv12: "NV12 (raw)",
  bgr24: "RGB24 (raw)",
  bgr0: "RGB32 (raw)",
  rgb24: "RGB24 (raw)",
  yuv420p: "I420 (raw)",
  uyvy422: "UYVY (raw)",
};

/** "mjpeg 1280×720@30fps [via mjpeg] — not available: …" → parts. */
function parseNegotiated(s: string | null | undefined): { codec: string; w: number; h: number; fps: number; via: string; note: string | null } | null {
  if (!s) return null;
  const m = /^(\S+)\s+(\d+)×(\d+)@(\d+)fps(?:\s+\[via ([^\]]+)\])?/.exec(s);
  if (!m) return null;
  const note = s.includes(" — ") ? s.split(" — ").slice(1).join(" — ") : null;
  return { codec: m[1], w: Number(m[2]), h: Number(m[3]), fps: Number(m[4]), via: m[5] ?? "", note };
}

type Preset = "low" | "balanced" | "high" | "custom";

export default function VideoInputDialog({ onClose }: { onClose: () => void }) {
  const diag = useProduction((s) => s.diag);
  const vs = useProduction((s) => s.state?.video_settings);
  const refresh = useProduction((s) => s.refresh);
  const [cams, setCams] = useState<CameraSourceInfo[] | null>(null);
  const [devices, setDevices] = useState<string[]>([]);
  const [sid, setSid] = useState<string | null>(null);
  const [device, setDevice] = useState<string>("");
  const [modes, setModes] = useState<CameraMode[] | null>(null);
  const [modesErr, setModesErr] = useState<string | null>(null);
  const [format, setFormat] = useState("auto");
  const [w, setW] = useState(1280);
  const [h, setH] = useState(720);
  const [fps, setFps] = useState(30);
  const [preset, setPreset] = useState<Preset>("custom");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ tone: "ok" | "warn" | "bad"; text: string } | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const loadCams = async () => {
    if (!isTauri) {
      setCams([]);
      return;
    }
    try {
      const [c, d] = await Promise.all([api.cameraSources(), api.listCameras().catch(() => [] as string[])]);
      setCams(c);
      setDevices(d);
      return c;
    } catch (e) {
      setCams([]);
      setMsg({ tone: "bad", text: errText(e) });
    }
  };

  useEffect(() => {
    void loadCams().then((c) => {
      if (c && c.length) pick(c[0]);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const cam = cams?.find((c) => c.id === sid) ?? null;

  function pick(c: CameraSourceInfo) {
    setSid(c.id);
    setDevice(c.device ?? "");
    setFormat(c.format ?? "auto");
    setW(c.width || 1280);
    setH(c.height || 720);
    setFps(c.fps || 30);
    setPreset("custom");
    setMsg(null);
  }

  // The device's own mode list (no engine lock on the backend).
  const realDevice = device || devices[0] || "";
  useEffect(() => {
    if (!realDevice || !isTauri) {
      setModes([]);
      return;
    }
    let off = false;
    setModes(null);
    setModesErr(null);
    api
      .cameraModes(realDevice)
      .then((m) => !off && setModes(m))
      .catch((e) => {
        if (off) return;
        setModes([]);
        setModesErr(errText(e));
      });
    return () => {
      off = true;
    };
  }, [realDevice]);

  const reported = (modes?.length ?? 0) > 0;
  const formats = useMemo(() => ["auto", ...Array.from(new Set((modes ?? []).map((m) => m.format)))], [modes]);
  const fm = useMemo(() => (modes ?? []).filter((m) => format === "auto" || m.format === format), [modes, format]);

  const sizes = useMemo(() => {
    if (!reported) return STD_SIZES;
    const out: [number, number][] = [];
    const add = (a: number, b: number) => !out.some(([x, y]) => x === a && y === b) && out.push([a, b]);
    for (const m of fm) {
      if (m.min_width === m.max_width && m.min_height === m.max_height) add(m.max_width, m.max_height);
      else for (const [a, b] of STD_SIZES) if (fits(m, a, b)) add(a, b);
    }
    return out.sort((a, b) => b[0] * b[1] - a[0] * a[1]);
  }, [fm, reported]);

  const rates = useMemo(() => {
    if (!reported) return STD_FPS;
    const at = fm.filter((m) => fits(m, w, h));
    const set = new Set<number>();
    for (const f of STD_FPS) if (at.some((m) => fitsFps(m, f))) set.add(f);
    for (const m of at) if (!STD_FPS.some((f) => Math.abs(f - m.max_fps) < 0.6)) set.add(Math.round(m.max_fps));
    return [...set].sort((a, b) => a - b);
  }, [fm, w, h, reported]);

  const ranged = fm.some((m) => m.min_width !== m.max_width || m.min_height !== m.max_height);
  const sizeOk = !reported || fm.some((m) => fits(m, w, h));
  const fpsOk = !reported || fm.some((m) => fits(m, w, h) && fitsFps(m, fps));

  // Keep fps valid for the chosen size.
  useEffect(() => {
    if (reported && rates.length && !rates.includes(fps)) setFps(rates.includes(30) ? 30 : rates[rates.length - 1]);
  }, [rates, fps, reported]);

  const applyPreset = (p: Preset) => {
    setPreset(p);
    if (p === "custom" || !sizes.length) return;
    const list = [...sizes];
    let pickSize: [number, number];
    if (p === "low") pickSize = [...list].reverse().find(([, b]) => b >= 360) ?? list[list.length - 1];
    else if (p === "balanced") pickSize = list.find(([, b]) => b <= 720) ?? list[list.length - 1];
    else pickSize = list[0];
    setW(pickSize[0]);
    setH(pickSize[1]);
    setFormat("auto");
    const at = reported ? (modes ?? []).filter((m) => fits(m, pickSize[0], pickSize[1])) : [];
    const ok = (f: number) => !reported || at.some((m) => fitsFps(m, f));
    const avail = STD_FPS.filter(ok);
    if (p === "low") setFps(avail[0] ?? 15);
    else if (p === "balanced") setFps(avail.includes(30) ? 30 : [...avail].reverse().find((f) => f <= 30) ?? avail[0] ?? 30);
    else setFps(avail[avail.length - 1] ?? 30);
  };

  const apply = async () => {
    if (!cam) return;
    setBusy(true);
    setMsg(null);
    try {
      const audio = await api.sourceAudioDevice(cam.id).catch(() => null);
      const got = await api.cameraConfigure({ source_id: cam.id, device: device || null, width: w, height: h, fps, format: format === "auto" ? null : format, audio_device: audio ?? null });
      const n = parseNegotiated(got);
      if (n && (n.w !== w || n.h !== h || Math.abs(n.fps - fps) > 1)) {
        setMsg({ tone: "warn", text: `Asked for ${w}×${h} at ${fps} fps; the camera delivers ${n.w}×${n.h} at ${n.fps} fps (${n.codec}) — the closest it supports.` });
      } else if (n) {
        setMsg({ tone: "ok", text: `Camera now captures ${n.w}×${n.h} at ${n.fps} fps (${n.codec}).` });
      } else {
        setMsg({ tone: "ok", text: got });
      }
      await loadCams();
      await refresh();
    } catch (e) {
      setMsg({ tone: "bad", text: errText(e) });
    } finally {
      setBusy(false);
    }
  };

  const live = diag?.sources.find((x) => x.id === (cam?.id ?? ""));
  const neg = parseNegotiated(cam?.negotiated);

  return (
    <div className="modal-scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal vi-modal" role="dialog" aria-modal="true" aria-label="Video input">
        <div className="fx-modal-head">
          <h2>
            Video input <span>camera / capture card capture settings — not the stream output</span>
          </h2>
          <button type="button" className="btn small secondary" onClick={onClose}>
            Done
          </button>
        </div>
        <div className="vi-body">
          {cams === null ? (
            <div className="panel-empty">Looking for cameras…</div>
          ) : cams.length === 0 ? (
            <div className="panel-empty">No camera or capture card is in your scenes yet. Add one with “Add source → Camera”, then set how it captures here.</div>
          ) : (
            <>
              <div className="vi-cols">
                <div className="vi-form">
                  {cams.length > 1 && (
                    <label className="f">
                      <span>Camera source</span>
                      <select value={sid ?? ""} onChange={(e) => pick(cams.find((c) => c.id === e.target.value)!)}>
                        {cams.map((c) => (
                          <option key={c.id} value={c.id}>
                            {c.name}
                          </option>
                        ))}
                      </select>
                    </label>
                  )}
                  <label className="f">
                    <span>Device</span>
                    <select
                      value={device}
                      onChange={(e) => {
                        setDevice(e.target.value);
                        setFormat("auto");
                        setPreset("custom");
                      }}
                    >
                      <option value="">First available{devices[0] ? ` (${devices[0]})` : ""}</option>
                      {devices.map((d) => (
                        <option key={d} value={d}>
                          {d}
                        </option>
                      ))}
                      {device && !devices.includes(device) && <option value={device}>{device} (not connected)</option>}
                    </select>
                  </label>

                  <div className="f">
                    <span>Quality preset</span>
                    <div className="seg-row" role="radiogroup" aria-label="Quality preset">
                      {(["low", "balanced", "high", "custom"] as Preset[]).map((p) => (
                        <button key={p} type="button" role="radio" aria-checked={preset === p} className={"tool" + (preset === p ? " on" : "")} onClick={() => applyPreset(p)}>
                          {p === "low" ? "Low" : p === "balanced" ? "Balanced" : p === "high" ? "High" : "Custom"}
                        </button>
                      ))}
                    </div>
                  </div>

                  <label className="f">
                    <span>Resolution</span>
                    <select
                      value={`${w}x${h}`}
                      onChange={(e) => {
                        const [a, b] = e.target.value.split("x").map(Number);
                        setW(a);
                        setH(b);
                        setPreset("custom");
                      }}
                    >
                      {!sizes.some(([a, b]) => a === w && b === h) && (
                        <option value={`${w}x${h}`}>
                          {w}×{h}
                          {reported ? " (not offered by this format)" : ""}
                        </option>
                      )}
                      {sizes.map(([a, b]) => (
                        <option key={`${a}x${b}`} value={`${a}x${b}`}>
                          {b}p — {a}×{b} ({aspect(a, b)})
                        </option>
                      ))}
                    </select>
                  </label>

                  <div className="f-row">
                    <label className="f">
                      <span>Width</span>
                      <input type="number" min={16} max={7680} step={2} value={w} disabled={reported && !ranged} onChange={(e) => (setW(Math.round(Number(e.target.value) || 0)), setPreset("custom"))} />
                    </label>
                    <label className="f">
                      <span>Height</span>
                      <input type="number" min={16} max={4320} step={2} value={h} disabled={reported && !ranged} onChange={(e) => (setH(Math.round(Number(e.target.value) || 0)), setPreset("custom"))} />
                    </label>
                  </div>
                  {reported && !ranged && <p className="note">This device only offers the fixed sizes above (no arbitrary width × height).</p>}
                  {!sizeOk && <p className="note warn">The device does not list {w}×{h}; it will deliver its closest mode.</p>}

                  <div className="f-row">
                    <label className="f">
                      <span>Frame rate</span>
                      <select value={fps} onChange={(e) => (setFps(Number(e.target.value)), setPreset("custom"))}>
                        {!rates.includes(fps) && <option value={fps}>{fps} fps</option>}
                        {rates.map((f) => (
                          <option key={f} value={f}>
                            {f} fps
                          </option>
                        ))}
                      </select>
                    </label>
                    <label className="f">
                      <span>Aspect ratio</span>
                      <input type="text" readOnly value={aspect(w, h)} title="Follows the chosen size; the picture is never stretched — placement (Fill / Fit) is set per source in Transform." />
                    </label>
                  </div>
                  {!fpsOk && <p className="note warn">{fps} fps is not offered at {w}×{h}; the closest rate will be used.</p>}

                  <label className="f">
                    <span>Device format</span>
                    <select value={format} disabled={!reported} onChange={(e) => (setFormat(e.target.value), setPreset("custom"))}>
                      {formats.map((f) => (
                        <option key={f} value={f}>
                          {FORMAT_NAME[f] ?? f}
                        </option>
                      ))}
                      {!formats.includes(format) && <option value={format}>{FORMAT_NAME[format] ?? format}</option>}
                    </select>
                  </label>
                  <p className="note">
                    {modes === null
                      ? "Reading what this device supports…"
                      : reported
                        ? `${modes.length} modes reported by the device. MJPEG needs less USB bandwidth (high sizes at full rate) but is decoded on the CPU; raw formats need no decoding.`
                        : modesErr
                          ? `The device's modes could not be read (${modesErr}). Standard sizes are offered; the device picks its closest.`
                          : "This device does not report its modes. Standard sizes are offered; the device picks its closest and the real result is shown on the right."}
                  </p>

                  <div className="form-actions left">
                    <button type="button" className="btn small" disabled={busy || !cam} onClick={() => void apply()}>
                      {busy ? "Reopening the camera…" : "Apply to camera"}
                    </button>
                    <span className="note">The old capture stops before the new one starts.</span>
                  </div>
                  {msg && <div className={"form-msg " + msg.tone}>{msg.text}</div>}
                </div>

                <aside className="vi-active">
                  <h4 className="sub">Active input</h4>
                  {neg ? (
                    <>
                      <div className="vi-big">
                        {neg.w} × {neg.h} <small>@ {neg.fps} fps</small>
                      </div>
                      <dl className="kv">
                        <dt>Format</dt>
                        <dd>{FORMAT_NAME[neg.codec] ? FORMAT_NAME[neg.codec].split(" (")[0] : neg.codec}</dd>
                        <dt>Aspect</dt>
                        <dd>{aspect(neg.w, neg.h)}</dd>
                        {live?.device_packet_fps != null && (
                          <>
                            <dt>Camera sends</dt>
                            <dd className={live.device_packet_fps < neg.fps * 0.8 ? "warn" : ""}>{live.device_packet_fps.toFixed(1)} fps</dd>
                          </>
                        )}
                        {live?.decode_ms != null && (
                          <>
                            <dt>Decode</dt>
                            <dd>{live.decode_ms.toFixed(1)} ms</dd>
                          </>
                        )}
                        {live?.paused && (
                          <>
                            <dt>Now</dt>
                            <dd>Paused (not on Preview or Program)</dd>
                          </>
                        )}
                      </dl>
                      {neg.note && <p className="note">Not available: {neg.note.replace(/^not available: /, "")}</p>}
                    </>
                  ) : (
                    <p className="note">{cam?.negotiated ?? "Not capturing (no camera backend on this computer, or the device failed to open)."}</p>
                  )}
                  <h4 className="sub">Output (stream and recording)</h4>
                  <p className="note">
                    {vs ? `${vs.output_width} × ${vs.output_height} @ ${vs.output_fps} fps, ${vs.bitrate_kbps} kbps` : "—"} — set in Output → Video format. Separate from the input: a {neg ? `${neg.h}p` : "smaller"} camera in a larger output is upscaled, not sharper.
                  </p>
                </aside>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
