import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api } from "../../lib/ipc";
import { errText, useProduction } from "../../lib/productionStore";
import type { CameraSourceInfo, SourceKind, SourceView } from "../../lib/types";
import Monitor from "../Monitor";
import { Banner, Dot, Empty, Field, Glyph, Row, Section, Segmented, Switch } from "../ui";

type AddKind = "camera" | "capture" | "screen" | "media" | "mic" | "text";

const SIZES = [
  { v: "854x480", label: "480p" },
  { v: "1280x720", label: "720p" },
  { v: "1920x1080", label: "1080p" },
];

function glyphFor(kind: string) {
  switch (kind) {
    case "camera":
      return <Glyph name="camera" />;
    case "screen":
      return <Glyph name="screen" />;
    case "media":
      return <Glyph name="media" />;
    case "audioinput":
      return <Glyph name="mic" />;
    case "text":
      return <Glyph name="text" />;
    case "color":
      return <Glyph name="color" />;
    default:
      return <Glyph name="pattern" />;
  }
}

function kindName(kind: string) {
  return (
    {
      camera: "Camera",
      screen: "Screen",
      media: "Media file",
      audioinput: "Microphone",
      text: "Text",
      color: "Color",
      testpattern: "Test pattern",
    } as Record<string, string>
  )[kind] ?? kind;
}

const baseName = (p: string) => p.split(/[\\/]/).pop() || p;

/**
 * INPUT: what goes into the show. Cameras, capture cards, screen, media
 * files and microphones; per-source live status from diagnostics.
 */
export default function InputView() {
  const state = useProduction((s) => s.state);
  const diag = useProduction((s) => s.diag);
  const sceneId = useProduction((s) => s.sceneId());
  const sourceSetVisible = useProduction((s) => s.sourceSetVisible);
  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  const scene = state?.scenes.find((s) => s.id === sceneId) ?? null;
  // Top of the list = top of the picture.
  const sources = useMemo(() => [...(scene?.sources ?? [])].sort((a, b) => b.order - a.order), [scene]);
  const sel = sources.find((s) => s.id === selected) ?? null;

  const subtitle = (s: SourceView): string => {
    if ((s.status ?? "").toLowerCase().includes("pending")) return "Not available on this computer";
    const d = diag?.sources.find((x) => x.id === s.id);
    if (d?.error) return d.error;
    if (s.kind === "audioinput") {
      const v = state?.mixer.voice[s.id];
      return v ? (v.device === "running" ? "Receiving audio" : v.device) : "Microphone";
    }
    if (d) {
      if (d.fps <= 0) return "Waiting for frames…";
      return [`${d.width}×${d.height}`, `${Math.round(d.fps)} fps`, d.format?.split(" [")[0]].filter(Boolean).join(" · ");
    }
    return kindName(s.kind);
  };

  const tone = (s: SourceView) => {
    if ((s.status ?? "").toLowerCase().includes("pending")) return "off" as const;
    const d = diag?.sources.find((x) => x.id === s.id);
    if (d?.error) return "bad" as const;
    if (s.kind === "audioinput") {
      const v = state?.mixer.voice[s.id];
      return v?.device === "running" ? ("ok" as const) : v?.device.startsWith("error") ? ("bad" as const) : ("warn" as const);
    }
    if (d) return d.fps > 0 ? ("ok" as const) : ("warn" as const);
    return "ok" as const;
  };

  return (
    <div className="workspace">
      <div className="stage-area">
        <Monitor />
      </div>
      <aside className="inspector">
        <Section
          title="Sources"
          action={
            <button type="button" className="text-btn" onClick={() => setAdding((v) => !v)}>
              {adding ? "Cancel" : "Add"}
            </button>
          }
          footer="Top of the list is drawn on top. The first visible camera is the reference for A/V sync."
        >
          {sources.length === 0 && <Empty title="No sources yet" hint="Add a camera, capture card, screen or media file." />}
          {sources.map((s) => (
            <Row
              key={s.id}
              icon={
                <span className="row-glyph">
                  {glyphFor(s.kind)}
                  <Dot tone={tone(s)} />
                </span>
              }
              title={s.name}
              subtitle={subtitle(s)}
              selected={selected === s.id}
              dim={!s.visible}
              onClick={() => setSelected(selected === s.id ? null : s.id)}
              trailing={
                s.kind !== "audioinput" ? (
                  <Switch checked={s.visible} onChange={(v) => void sourceSetVisible(s.id, v)} label={s.visible ? "Visible" : "Hidden"} />
                ) : undefined
              }
            />
          ))}
        </Section>
        {adding && <AddSource onDone={() => setAdding(false)} />}
        {sel && !adding && <SourceDetail key={sel.id} source={sel} sources={sources} onRemoved={() => setSelected(null)} />}
      </aside>
    </div>
  );
}

function AddSource({ onDone }: { onDone: () => void }) {
  const sourceAdd = useProduction((s) => s.sourceAdd);
  const busy = useProduction((s) => !!s.busy.add);
  const [kind, setKind] = useState<AddKind>("camera");
  const [cameras, setCameras] = useState<string[] | null>(null);
  const [mics, setMics] = useState<string[] | null>(null);
  const [device, setDevice] = useState("");
  const [audio, setAudio] = useState("__auto");
  const [size, setSize] = useState("1280x720");
  const [fps, setFps] = useState(30);
  const [path, setPath] = useState("");
  const [text, setText] = useState("");
  const [name, setName] = useState("");
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let off = false;
    void api.listCameras().then((l) => !off && (setCameras(l), setDevice((d) => d || l[0] || ""))).catch(() => !off && setCameras([]));
    void api.listAudioDevices().then((l) => !off && setMics(l)).catch(() => !off && setMics([]));
    return () => {
      off = true;
    };
  }, []);

  useEffect(() => {
    setErr(null);
    if (kind === "mic") setDevice(mics?.[0] ?? "");
    else if (kind === "camera" || kind === "capture") setDevice(cameras?.[0] ?? "");
    // Default: pair the device's own audio by name (a webcam's built-in
    // mic, a capture card's HDMI audio). Nothing matching → no audio, so
    // a separately added microphone is never doubled.
    setAudio("__auto");
  }, [kind, cameras, mics]);

  const browse = async () => {
    try {
      const picked = await open({
        multiple: false,
        title: "Choose a media file",
        filters: [
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
      if (!device) return setErr(cameras && cameras.length === 0 ? "No camera or capture card found. Connect one and try again." : "Choose a device.");
      const [w, h] = size.split("x").map(Number);
      k = {
        camera: {
          device,
          width: w,
          height: h,
          fps,
          // "" = no audio, null = auto-pair by name, else the device.
          audio_device: audio === "__auto" ? null : audio,
        },
      };
      label ||= device;
    } else if (kind === "screen") {
      k = { screen: {} };
      label ||= "Screen";
    } else if (kind === "media") {
      if (!path.trim()) return setErr("Choose a media file first.");
      k = { media: { path: path.trim() } };
      label ||= baseName(path.trim());
    } else if (kind === "mic") {
      if (!device) return setErr(mics && mics.length === 0 ? "No microphone found. Connect one and try again." : "Choose a microphone.");
      k = { audio_input: { device } };
      label ||= device;
    } else {
      if (!text.trim()) return setErr("Type the text to show.");
      k = { text: { text: text.trim() } };
      label ||= "Text";
    }
    if (await sourceAdd(k, label)) onDone();
  };

  const deviceList = kind === "mic" ? mics : cameras;

  return (
    <Section title="Add source">
      <div className="pad">
        <Segmented<AddKind>
          label="Source type"
          value={kind}
          onChange={setKind}
          size="small"
          options={[
            { value: "camera", label: "Camera" },
            { value: "capture", label: "Capture card", title: "HDMI capture card (video + its audio)" },
            { value: "screen", label: "Screen" },
            { value: "media", label: "Media" },
            { value: "mic", label: "Mic" },
            { value: "text", label: "Text" },
          ]}
        />
      </div>
      {(kind === "camera" || kind === "capture" || kind === "mic") && (
        <Field label={kind === "mic" ? "Microphone" : "Device"}>
          <select value={device} onChange={(e) => setDevice(e.target.value)} disabled={!deviceList?.length}>
            {deviceList === null && <option value="">Looking for devices…</option>}
            {deviceList?.length === 0 && <option value="">None found</option>}
            {deviceList?.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        </Field>
      )}
      {(kind === "camera" || kind === "capture") && (
        <>
          <Field label="Resolution">
            <Segmented label="Resolution" size="small" value={size} onChange={setSize} options={SIZES.map((s) => ({ value: s.v, label: s.label }))} />
          </Field>
          <Field label="Frame rate">
            <Segmented label="Frame rate" size="small" value={fps} onChange={setFps} options={[30, 60].map((f) => ({ value: f, label: `${f}` }))} />
          </Field>
          <Field label="Audio" hint={audio === "__auto" ? "Uses the audio device with the same name, if there is one." : undefined}>
            <select value={audio} onChange={(e) => setAudio(e.target.value)}>
              <option value="">No audio (use a separate mic)</option>
              <option value="__auto">Automatic (same device)</option>
              {mics?.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
          </Field>
        </>
      )}
      {kind === "media" && (
        <Field label="File">
          <span className="file-pick">
            <span className="file-name" title={path || undefined}>
              {path ? baseName(path) : "No file chosen"}
            </span>
            <button type="button" className="btn small" onClick={() => void browse()}>
              Choose…
            </button>
          </span>
        </Field>
      )}
      {kind === "text" && (
        <Field label="Text">
          <input type="text" value={text} maxLength={200} onChange={(e) => setText(e.target.value)} placeholder="Lower third, title…" />
        </Field>
      )}
      <Field label="Name">
        <input type="text" value={name} maxLength={60} onChange={(e) => setName(e.target.value)} placeholder="Optional" />
      </Field>
      {err && (
        <div className="pad">
          <Banner tone="error" onClose={() => setErr(null)}>
            {err}
          </Banner>
        </div>
      )}
      <div className="pad actions">
        <button type="button" className="btn" disabled={busy} onClick={() => void submit()}>
          {busy ? "Opening device…" : "Add source"}
        </button>
      </div>
    </Section>
  );
}

function SourceDetail({ source, sources, onRemoved }: { source: SourceView; sources: SourceView[]; onRemoved: () => void }) {
  const sourceRemove = useProduction((s) => s.sourceRemove);
  const sourceReorder = useProduction((s) => s.sourceReorder);
  const refresh = useProduction((s) => s.refresh);
  const mediaPaused = useProduction((s) => s.mediaPaused[source.id] ?? false);
  const mediaSetPaused = useProduction((s) => s.mediaSetPaused);
  const [confirm, setConfirm] = useState(false);
  const [cam, setCam] = useState<CameraSourceInfo | null>(null);
  const [devices, setDevices] = useState<string[]>([]);
  const [mics, setMics] = useState<string[]>([]);
  const [device, setDevice] = useState("");
  const [size, setSize] = useState("1280x720");
  const [fps, setFps] = useState(30);
  const [audio, setAudio] = useState("");
  const [applying, setApplying] = useState(false);
  const [msg, setMsg] = useState<{ tone: "error" | "info"; text: string } | null>(null);

  useEffect(() => {
    if (source.kind !== "camera") return;
    let off = false;
    void (async () => {
      try {
        const [cams, devs, adevs, ad] = await Promise.all([
          api.cameraSources(),
          api.listCameras().catch(() => [] as string[]),
          api.listAudioDevices().catch(() => [] as string[]),
          api.sourceAudioDevice(source.id).catch(() => null),
        ]);
        if (off) return;
        const c = cams.find((x) => x.id === source.id) ?? null;
        setCam(c);
        setDevices(devs);
        setMics(adevs);
        if (c) {
          setDevice(c.device ?? "");
          setSize(`${c.width}x${c.height}`);
          setFps(c.fps === 60 ? 60 : 30);
        }
        setAudio(ad ?? "");
      } catch {
        /* outside the app */
      }
    })();
    return () => {
      off = true;
    };
  }, [source.id, source.kind]);

  // Reorder: list is shown top-first; backend order is bottom-first.
  const move = (dir: -1 | 1) => {
    const topFirst = sources.map((s) => s.id);
    const i = topFirst.indexOf(source.id);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= topFirst.length) return;
    [topFirst[i], topFirst[j]] = [topFirst[j], topFirst[i]];
    void sourceReorder([...topFirst].reverse());
  };

  const apply = async () => {
    const [w, h] = size.split("x").map(Number);
    setApplying(true);
    setMsg(null);
    try {
      const got = await api.cameraReconfigure({ source_id: source.id, device: device || null, width: w, height: h, fps, audio_device: audio || null });
      setMsg({ tone: "info", text: `Camera running: ${got}` });
      await refresh();
    } catch (e) {
      setMsg({ tone: "error", text: errText(e) });
    } finally {
      setApplying(false);
    }
  };

  const idx = sources.findIndex((s) => s.id === source.id);

  return (
    <Section title={source.name}>
      {source.kind === "camera" && (
        <>
          <Field label="Device">
            <select value={device} onChange={(e) => setDevice(e.target.value)}>
              <option value="">First available</option>
              {devices.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
              {device && !devices.includes(device) && <option value={device}>{device}</option>}
            </select>
          </Field>
          <Field label="Resolution">
            <Segmented label="Resolution" size="small" value={size} onChange={setSize} options={SIZES.map((s) => ({ value: s.v, label: s.label }))} />
          </Field>
          <Field label="Frame rate">
            <Segmented label="Frame rate" size="small" value={fps} onChange={setFps} options={[30, 60].map((f) => ({ value: f, label: `${f}` }))} />
          </Field>
          <Field label="Audio">
            <select value={audio} onChange={(e) => setAudio(e.target.value)}>
              <option value="">No audio</option>
              {mics.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
              {audio && !mics.includes(audio) && <option value={audio}>{audio}</option>}
            </select>
          </Field>
          {cam?.negotiated && <Row title="Camera delivers" subtitle={cam.negotiated} />}
          <div className="pad actions">
            <button type="button" className="btn" disabled={applying} onClick={() => void apply()}>
              {applying ? "Reopening camera…" : "Apply"}
            </button>
          </div>
        </>
      )}
      {source.kind === "media" && (
        <Row
          title="Playback"
          trailing={
            <button type="button" className="btn small secondary" onClick={() => void mediaSetPaused(source.id, !mediaPaused)}>
              {mediaPaused ? "Play" : "Pause"}
            </button>
          }
        />
      )}
      {msg && (
        <div className="pad">
          <Banner tone={msg.tone} onClose={() => setMsg(null)}>
            {msg.text}
          </Banner>
        </div>
      )}
      <div className="pad actions spread">
        {source.kind !== "audioinput" && (
          <span className="btn-group">
            <button type="button" className="btn small secondary" disabled={idx <= 0} onClick={() => move(-1)} title="Bring forward">
              Up
            </button>
            <button type="button" className="btn small secondary" disabled={idx < 0 || idx >= sources.length - 1} onClick={() => move(1)} title="Send backward">
              Down
            </button>
          </span>
        )}
        <button
          type="button"
          className={"btn small " + (confirm ? "danger" : "secondary")}
          onClick={() => {
            if (!confirm) return setConfirm(true);
            void sourceRemove(source.id).then(onRemoved);
          }}
          onBlur={() => setConfirm(false)}
        >
          {confirm ? "Tap again to remove" : "Remove"}
        </button>
      </div>
    </Section>
  );
}
