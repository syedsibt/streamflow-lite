import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api } from "../../lib/ipc";
import { errText, useProduction } from "../../lib/productionStore";
import type { SourceKind } from "../../lib/types";
import { Glyph, type GlyphName } from "../ui";

type Kind = "camera" | "capture" | "screen" | "image" | "media" | "text" | "color" | "mic" | "pattern";

const KINDS: { id: Kind; label: string; glyph: GlyphName; hint: string }[] = [
  { id: "camera", label: "Camera", glyph: "camera", hint: "USB webcam or phone camera app" },
  { id: "capture", label: "Capture card", glyph: "capture", hint: "HDMI capture: video and its audio" },
  { id: "screen", label: "Screen", glyph: "screen", hint: "The whole desktop" },
  { id: "image", label: "Image", glyph: "image", hint: "Logo, watermark, overlay, background (PNG keeps transparency)" },
  { id: "media", label: "Video file", glyph: "media", hint: "MP4, MOV, MKV… with its sound" },
  { id: "text", label: "Text", glyph: "text", hint: "Title or lower third" },
  { id: "color", label: "Color", glyph: "color", hint: "Solid background" },
  { id: "mic", label: "Microphone", glyph: "mic", hint: "Audio only — appears in the mixer" },
  { id: "pattern", label: "Test signal", glyph: "pattern", hint: "Bars, timecode and motion for checking the pipeline" },
];

/** Shown so nothing is hidden — but not offered as working. */
const PLANNED: { label: string; glyph: GlyphName }[] = [
  { label: "Window", glyph: "window" },
  { label: "Browser", glyph: "browser" },
  { label: "NDI", glyph: "network" },
  { label: "SRT / RTSP stream", glyph: "network" },
  { label: "WebRTC", glyph: "network" },
];

const SIZES = [
  { v: "1280x720", label: "720p" },
  { v: "1920x1080", label: "1080p" },
];
const baseName = (p: string) => p.split(/[\\/]/).pop() || p;

export default function AddSourceDialog({ onClose }: { onClose: () => void }) {
  const sourceAdd = useProduction((s) => s.sourceAdd);
  const busy = useProduction((s) => !!s.busy.add);
  const [kind, setKind] = useState<Kind>("camera");
  const [cameras, setCameras] = useState<string[] | null>(null);
  const [camErr, setCamErr] = useState<string | null>(null);
  const [mics, setMics] = useState<string[] | null>(null);
  const [micErr, setMicErr] = useState<string | null>(null);
  const [device, setDevice] = useState("");
  const [audio, setAudio] = useState("__auto");
  const [size, setSize] = useState("1280x720");
  const [fps, setFps] = useState(30);
  const [path, setPath] = useState("");
  const [text, setText] = useState("");
  const [color, setColor] = useState("#1d2733");
  const [name, setName] = useState("");
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let off = false;
    void api
      .listCameras()
      .then((l) => !off && setCameras(l))
      .catch((e) => !off && (setCameras([]), setCamErr(errText(e))));
    void api
      .listAudioDevices()
      .then((l) => !off && setMics(l))
      .catch((e) => !off && (setMics([]), setMicErr(errText(e))));
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => {
      off = true;
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  useEffect(() => {
    setErr(null);
    setPath("");
    if (kind === "mic") setDevice(mics?.[0] ?? "");
    else if (kind === "camera" || kind === "capture") setDevice(cameras?.[0] ?? "");
    setAudio(kind === "capture" ? "__auto" : "__auto");
  }, [kind, cameras, mics]);

  const browse = async (what: "image" | "media") => {
    try {
      const picked = await open({
        multiple: false,
        title: what === "image" ? "Choose an image" : "Choose a video file",
        filters:
          what === "image"
            ? [{ name: "Images", extensions: ["png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff"] }]
            : [
                { name: "Video / audio", extensions: ["mp4", "mkv", "mov", "webm", "m4v", "avi", "mp3", "wav", "m4a", "aac", "flac", "ogg"] },
                { name: "All files", extensions: ["*"] },
              ],
      });
      if (typeof picked === "string") setPath(picked);
    } catch (e) {
      setErr(`File picker unavailable: ${errText(e)}`);
    }
  };

  const submit = async () => {
    setErr(null);
    let k: SourceKind;
    let label = name.trim();
    if (kind === "camera" || kind === "capture") {
      if (!device) return setErr(camErr ?? (cameras && cameras.length === 0 ? "No camera or capture card found. Connect one, then reopen this dialog." : "Choose a device."));
      const [w, h] = size.split("x").map(Number);
      k = { camera: { device, width: w, height: h, fps, audio_device: audio === "__auto" ? null : audio } };
      label ||= device;
    } else if (kind === "screen") {
      k = { screen: {} };
      label ||= "Screen";
    } else if (kind === "image") {
      if (!path) return setErr("Choose an image file.");
      k = { image: { path } };
      label ||= baseName(path);
    } else if (kind === "media") {
      if (!path) return setErr("Choose a video file.");
      k = { media: { path } };
      label ||= baseName(path);
    } else if (kind === "mic") {
      if (!device) return setErr(micErr ?? (mics && mics.length === 0 ? "No microphone found. Connect one, then reopen this dialog." : "Choose a microphone."));
      k = { audio_input: { device } };
      label ||= device;
    } else if (kind === "text") {
      if (!text.trim()) return setErr("Type the text to show.");
      k = { text: { text: text.trim() } };
      label ||= text.trim().slice(0, 30);
    } else if (kind === "color") {
      const n = parseInt(color.slice(1), 16);
      k = { color: { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255 } };
      label ||= `Color ${color}`;
    } else {
      k = { test_pattern: {} };
      label ||= "Test signal";
    }
    if (await sourceAdd(k, label)) onClose();
  };

  const list = kind === "mic" ? mics : cameras;
  const listErr = kind === "mic" ? micErr : camErr;
  const info = KINDS.find((k) => k.id === kind)!;

  return (
    <div className="modal-scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal add-source" role="dialog" aria-modal="true" aria-label="Add source">
        <nav className="kind-list" aria-label="Source type">
          {KINDS.map((k) => (
            <button key={k.id} type="button" className={kind === k.id ? "on" : ""} onClick={() => setKind(k.id)}>
              <Glyph name={k.glyph} size={16} />
              {k.label}
            </button>
          ))}
          <div className="kind-planned">
            {PLANNED.map((p) => (
              <span key={p.label} title="Not built yet">
                <Glyph name={p.glyph} size={16} />
                {p.label}
                <em>Planned</em>
              </span>
            ))}
          </div>
        </nav>
        <div className="kind-form">
          <h2>{info.label}</h2>
          <p className="kind-hint">{info.hint}</p>
          {(kind === "camera" || kind === "capture" || kind === "mic") && (
            <label className="f">
              <span>{kind === "mic" ? "Microphone" : "Device"}</span>
              <select value={device} onChange={(e) => setDevice(e.target.value)} disabled={!list?.length}>
                {list === null && <option value="">Looking for devices…</option>}
                {list?.length === 0 && <option value="">{listErr ? "Device list failed" : "None found"}</option>}
                {list?.map((d) => (
                  <option key={d} value={d}>
                    {d}
                  </option>
                ))}
              </select>
              {listErr && <small className="bad">{listErr}</small>}
            </label>
          )}
          {(kind === "camera" || kind === "capture") && (
            <>
              <div className="f-row">
                <label className="f">
                  <span>Resolution</span>
                  <select value={size} onChange={(e) => setSize(e.target.value)}>
                    {SIZES.map((s) => (
                      <option key={s.v} value={s.v}>
                        {s.label} ({s.v.replace("x", "×")})
                      </option>
                    ))}
                  </select>
                </label>
                <label className="f">
                  <span>Frame rate</span>
                  <select value={fps} onChange={(e) => setFps(Number(e.target.value))}>
                    <option value={30}>30 fps</option>
                    <option value={60}>60 fps</option>
                  </select>
                </label>
              </div>
              <label className="f">
                <span>Audio</span>
                <select value={audio} onChange={(e) => setAudio(e.target.value)}>
                  <option value="__auto">Same device, if it has audio</option>
                  <option value="">No audio (use a separate microphone)</option>
                  {mics?.map((d) => (
                    <option key={d} value={d}>
                      {d}
                    </option>
                  ))}
                </select>
              </label>
            </>
          )}
          {(kind === "image" || kind === "media") && (
            <div className="f">
              <span>File</span>
              <div className="file-pick">
                <span className="file-name" title={path || undefined}>
                  {path ? baseName(path) : "No file chosen"}
                </span>
                <button type="button" className="btn small" onClick={() => void browse(kind)}>
                  Choose…
                </button>
              </div>
            </div>
          )}
          {kind === "text" && (
            <label className="f">
              <span>Text</span>
              <input type="text" value={text} maxLength={200} onChange={(e) => setText(e.target.value)} placeholder="e.g. Ali Khan — Host" />
            </label>
          )}
          {kind === "color" && (
            <label className="f">
              <span>Color</span>
              <input type="color" value={color} onChange={(e) => setColor(e.target.value)} />
            </label>
          )}
          <label className="f">
            <span>Name</span>
            <input type="text" value={name} maxLength={60} onChange={(e) => setName(e.target.value)} placeholder="Optional" />
          </label>
          {err && <div className="form-error">{err}</div>}
          <div className="form-actions">
            <button type="button" className="btn secondary" onClick={onClose}>
              Cancel
            </button>
            <button type="button" className="btn primary" disabled={busy} onClick={() => void submit()}>
              {busy ? (kind === "camera" || kind === "capture" ? "Opening device…" : "Adding…") : `Add ${info.label.toLowerCase()}`}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
