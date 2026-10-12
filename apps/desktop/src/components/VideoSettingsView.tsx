import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/ipc";
import { useStudio } from "../lib/store";
import type { CameraSourceInfo, VideoSettings } from "../lib/types";

const OUTPUT_SIZES = [
  { label: "720p (1280×720)", width: 1280, height: 720 },
  { label: "1080p (1920×1080)", width: 1920, height: 1080 },
];

const CAMERA_SIZES = [
  { label: "480p (854×480)", width: 854, height: 480 },
  { label: "720p (1280×720)", width: 1280, height: 720 },
  { label: "1080p (1920×1080)", width: 1920, height: 1080 },
];

const FPS_OPTIONS = [30, 60];

const ENCODER_OPTIONS = [
  { id: "auto", label: "Auto (best available)" },
  { id: "h264_amf", label: "AMF — AMD GPU" },
  { id: "h264_nvenc", label: "NVENC — NVIDIA GPU" },
  { id: "h264_qsv", label: "Quick Sync — Intel GPU" },
  { id: "libx264", label: "x264 — CPU (software)" },
];

function toast(text: string) {
  useStudio.getState().pushToast(text);
}

/**
 * Video menu panel: manual structure for video input (camera) and output
 * (program bus → stream/record).
 *
 * Input: pick a camera source, set its device/resolution/fps, and apply —
 * the backend closes and reopens the capture with the new settings and
 * reports what the camera actually negotiated.
 *
 * Output: program resolution/fps, encoder override, and bitrate. These apply
 * the next time streaming/recording starts (encoders open at start); the
 * media-tick cadence follows the output fps immediately.
 */
export default function VideoSettingsView() {
  const [settings, setSettings] = useState<VideoSettings | null>(null);
  const [cameras, setCameras] = useState<CameraSourceInfo[]>([]);
  const [devices, setDevices] = useState<string[]>([]);
  const [selCamId, setSelCamId] = useState<string>("");
  // Local input edits (initialized from the selected camera / defaults).
  const [inDevice, setInDevice] = useState<string>("");
  const [inSize, setInSize] = useState<string>("1280x720");
  const [inFps, setInFps] = useState<number>(30);
  const [inAudio, setInAudio] = useState<string>("");
  const [audioDevs, setAudioDevs] = useState<string[]>([]);
  const [applying, setApplying] = useState(false);
  const [saving, setSaving] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [s, cams, devs, adevs] = await Promise.all([
        api.videoSettingsGet(),
        api.cameraSources(),
        api.listCameras(),
        api.listAudioDevices().catch(() => [] as string[]),
      ]);
      setSettings(s);
      setCameras(cams);
      setDevices(devs);
      setAudioDevs(adevs);
      setSelCamId((prev) => {
        if (prev && cams.some((c) => c.id === prev)) return prev;
        return cams[0]?.id ?? "";
      });
    } catch (e) {
      toast(`video settings load failed: ${String(e)}`);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // When the selected camera (or settings) changes, seed the input editors.
  useEffect(() => {
    const cam = cameras.find((c) => c.id === selCamId);
    if (cam) {
      setInDevice(cam.device ?? "");
      setInSize(`${cam.width}x${cam.height}`);
      setInFps(cam.fps === 60 ? 60 : 30);
      void api
        .sourceAudioDevice(cam.id)
        .then((d) => setInAudio(d ?? ""))
        .catch(() => setInAudio(""));
    } else if (settings) {
      setInDevice(settings.camera_device ?? "");
      setInSize(`${settings.camera_width}x${settings.camera_height}`);
      setInFps(settings.camera_fps === 60 ? 60 : 30);
      setInAudio("");
    }
  }, [selCamId, cameras, settings]);

  const applyInput = async () => {
    if (!selCamId) {
      toast("No camera source — add one from Production first.");
      return;
    }
    const [w, h] = inSize.split("x").map(Number);
    setApplying(true);
    try {
      const negotiated = await api.cameraReconfigure({
        source_id: selCamId,
        device: inDevice || null,
        width: w,
        height: h,
        fps: inFps,
        audio_device: inAudio || null,
      });
      toast(`Camera applied — negotiated: ${negotiated}`);
      await refresh();
    } catch (e) {
      toast(`camera apply failed: ${String(e)}`);
    } finally {
      setApplying(false);
    }
  };

  const applyOutput = async () => {
    if (!settings) return;
    setSaving(true);
    try {
      await api.videoSettingsSet(settings);
      toast("Output settings saved — apply on next stream/record start.");
      await refresh();
    } catch (e) {
      toast(`output save failed: ${String(e)}`);
    } finally {
      setSaving(false);
    }
  };

  const selCam = cameras.find((c) => c.id === selCamId) ?? null;
  const outSize = settings ? `${settings.output_width}x${settings.output_height}` : "";

  return (
    <div className="video-settings">
      {/* ------------------------------ INPUT ------------------------------ */}
      <div className="panel">
        <div className="panel-head">
          <span className="panel-title">Video Input — Camera</span>
        </div>
        <div className="panel-body">
          <div className="form-grid">
            <label>Camera source</label>
            <select
              value={selCamId}
              onChange={(e) => setSelCamId(e.target.value)}
              disabled={cameras.length === 0}
            >
              {cameras.length === 0 && <option value="">No camera source</option>}
              {cameras.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </select>

            <label>Device</label>
            <select
              value={inDevice}
              onChange={(e) => setInDevice(e.target.value)}
              title="Capture device (blank = first available)"
            >
              <option value="">First available</option>
              {devices.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
              {inDevice && !devices.includes(inDevice) && (
                <option value={inDevice}>{inDevice}</option>
              )}
            </select>

            <label>Resolution</label>
            <select value={inSize} onChange={(e) => setInSize(e.target.value)}>
              {CAMERA_SIZES.map((r) => (
                <option
                  key={`${r.width}x${r.height}`}
                  value={`${r.width}x${r.height}`}
                >
                  {r.label}
                </option>
              ))}
            </select>

            <label>Frame rate</label>
            <select
              value={inFps}
              onChange={(e) => setInFps(Number(e.target.value))}
            >
              {FPS_OPTIONS.map((f) => (
                <option key={f} value={f}>
                  {f} fps
                </option>
              ))}
            </select>

            <label>Audio input</label>
            <select
              value={inAudio}
              onChange={(e) => setInAudio(e.target.value)}
              title="Capture audio together with this camera (optional)"
            >
              <option value="">No audio (video only)</option>
              {audioDevs.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
              {inAudio && !audioDevs.includes(inAudio) && (
                <option value={inAudio}>{inAudio}</option>
              )}
            </select>
          </div>

          <div className="note" style={{ marginTop: 10 }}>
            {selCam?.negotiated ? (
              <>
                Negotiated: <span className="mono">{selCam.negotiated}</span>
                {" "}— what the camera actually gave.
              </>
            ) : selCam ? (
              "Camera not opened yet — Apply to (re)open it."
            ) : (
              "No camera source in any scene. Add one from Production → Sources, then configure it here."
            )}
          </div>

          <div style={{ marginTop: 10 }}>
            <button
              className="btn"
              onClick={() => void applyInput()}
              disabled={applying || !selCamId}
            >
              {applying ? "Applying…" : "Apply to camera"}
            </button>
          </div>
        </div>
      </div>

      {/* ------------------------------ OUTPUT ------------------------------ */}
      <div className="panel">
        <div className="panel-head">
          <span className="panel-title">Video Output — Program</span>
        </div>
        <div className="panel-body">
          {settings ? (
            <>
              <div className="form-grid">
                <label>Resolution</label>
                <select
                  value={outSize}
                  onChange={(e) => {
                    const [w, h] = e.target.value.split("x").map(Number);
                    setSettings({ ...settings, output_width: w, output_height: h });
                  }}
                >
                  {OUTPUT_SIZES.map((r) => (
                    <option
                      key={`${r.width}x${r.height}`}
                      value={`${r.width}x${r.height}`}
                    >
                      {r.label}
                    </option>
                  ))}
                </select>

                <label>Frame rate</label>
                <select
                  value={settings.output_fps}
                  onChange={(e) =>
                    setSettings({ ...settings, output_fps: Number(e.target.value) })
                  }
                  title="Program tick rate (60 fps needs a fast PC)"
                >
                  {FPS_OPTIONS.map((f) => (
                    <option key={f} value={f}>
                      {f} fps
                    </option>
                  ))}
                </select>

                <label>Encoder</label>
                <select
                  value={settings.encoder}
                  onChange={(e) =>
                    setSettings({ ...settings, encoder: e.target.value })
                  }
                  title="Auto picks the best available (NVENC → Quick Sync → AMF → x264)"
                >
                  {ENCODER_OPTIONS.map((o) => (
                    <option key={o.id} value={o.id}>
                      {o.label}
                    </option>
                  ))}
                </select>

                <label>Bitrate</label>
                <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
                  <input
                    type="number"
                    min={500}
                    max={50000}
                    step={500}
                    value={settings.bitrate_kbps}
                    onChange={(e) =>
                      setSettings({
                        ...settings,
                        bitrate_kbps: Number(e.target.value),
                      })
                    }
                    style={{ width: 110 }}
                  />
                  <span className="note">kbps (stream + record)</span>
                </div>
              </div>

              <div className="note" style={{ marginTop: 10 }}>
                Frame rate applies <b>in real time</b> — the program tick
                follows immediately, and live stream/record encoders are
                rebuilt on the fly. Resolution, encoder, and bitrate apply
                the next time you start streaming/recording.
              </div>

              <div style={{ marginTop: 10 }}>
                <button
                  className="btn"
                  onClick={() => void applyOutput()}
                  disabled={saving}
                >
                  {saving ? "Saving…" : "Save output settings"}
                </button>
              </div>
            </>
          ) : (
            <div className="note">Loading…</div>
          )}
        </div>
      </div>
    </div>
  );
}
