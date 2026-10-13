import { useProduction } from "../../lib/productionStore";
import type { SourceDiag, SourceView } from "../../lib/types";
import { Glyph, type GlyphName } from "../ui";

export const KIND_GLYPH: Record<string, GlyphName> = {
  camera: "camera",
  screen: "screen",
  media: "media",
  image: "image",
  text: "text",
  color: "color",
  testpattern: "pattern",
  audioinput: "mic",
};

export const KIND_NAME: Record<string, string> = {
  camera: "Camera",
  screen: "Screen",
  media: "Video file",
  image: "Image",
  text: "Text",
  color: "Color",
  testpattern: "Test signal",
  audioinput: "Microphone",
};

/** One-line live status for a source, from measured diagnostics. */
export function sourceStatus(s: SourceView, d: SourceDiag | undefined, targetFps: number): { text: string; tone: "ok" | "warn" | "bad" | "off" } {
  if ((s.status ?? "").includes("pending")) return { text: "Not available on this computer", tone: "off" };
  if (d?.error) return { text: d.error, tone: "bad" };
  if (s.kind === "camera" || s.kind === "screen" || s.kind === "media") {
    if (!d) return { text: "Starting…", tone: "warn" };
    if (!d.width) {
      // Never a bare "waiting": say what the device is (or isn't) doing.
      const sends = d.device_packet_fps;
      return {
        text: sends != null ? `No picture yet · device sends ${sends.toFixed(0)} fps · ${d.capture_errors} read errors` : "No picture yet",
        tone: "warn",
      };
    }
    const fps = d.fps;
    const low = fps < targetFps * 0.8 && s.kind !== "media";
    return { text: `${d.width}×${d.height} · ${fps.toFixed(fps < 10 ? 1 : 0)} fps${low && d.device_packet_fps != null ? ` (device ${d.device_packet_fps.toFixed(0)})` : ""}`, tone: low ? "warn" : "ok" };
  }
  if (s.kind === "image" || s.kind === "text") return { text: s.detail ? s.detail.split(/[\\/]/).pop()! : KIND_NAME[s.kind], tone: "ok" };
  return { text: KIND_NAME[s.kind] ?? s.kind, tone: "ok" };
}

/**
 * Sources of the scene being prepared (preview), top layer first. Click to
 * select (the preview box and Properties follow); eye = visible, lock =
 * placement locked.
 */
export default function SourceStrip({ onAdd }: { onAdd: () => void }) {
  const state = useProduction((s) => s.state);
  const diag = useProduction((s) => s.diag);
  const selected = useProduction((s) => s.selectedSourceId);
  const select = useProduction((s) => s.selectSource);
  const setVisible = useProduction((s) => s.sourceSetVisible);
  const setLocked = useProduction((s) => s.sourceSetLocked);
  const sceneId = useProduction((s) => s.sceneId());
  const scene = state?.scenes.find((s) => s.id === sceneId);
  const sources = [...(scene?.sources ?? [])].filter((s) => s.kind !== "audioinput").sort((a, b) => b.order - a.order);
  const target = diag?.target_fps ?? state?.video_settings.output_fps ?? 30;

  return (
    <div className="source-strip" role="listbox" aria-label={`Sources in ${scene?.name ?? "scene"}`}>
      <div className="strip-title">
        <span>Sources</span>
        <small>{scene?.name}</small>
      </div>
      {sources.map((s) => {
        const d = diag?.sources.find((x) => x.id === s.id);
        const st = sourceStatus(s, d, target);
        return (
          <div
            key={s.id}
            role="option"
            aria-selected={selected === s.id}
            tabIndex={0}
            className={"src-tile" + (selected === s.id ? " sel" : "") + (s.visible ? "" : " hidden") + " t-" + st.tone}
            onClick={() => select(s.id)}
            onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && select(s.id)}
          >
            <span className="src-glyph">
              <Glyph name={KIND_GLYPH[s.kind] ?? "pattern"} size={16} />
            </span>
            <span className="src-text">
              <span className="src-name" title={s.name}>
                {s.name}
              </span>
              <span className="src-status" title={st.text}>
                {st.text}
              </span>
            </span>
            <span className="src-tools">
              <button
                type="button"
                className={"icon-btn" + (s.visible ? "" : " off")}
                aria-pressed={s.visible}
                title={s.visible ? "Hide" : "Show"}
                onClick={(e) => {
                  e.stopPropagation();
                  void setVisible(s.id, !s.visible);
                }}
              >
                <Glyph name={s.visible ? "eye" : "eyeoff"} size={14} />
              </button>
              <button
                type="button"
                className={"icon-btn" + (s.locked ? " on" : "")}
                aria-pressed={s.locked}
                title={s.locked ? "Unlock position" : "Lock position"}
                onClick={(e) => {
                  e.stopPropagation();
                  void setLocked(s.id, !s.locked);
                }}
              >
                <Glyph name={s.locked ? "lock" : "unlock"} size={14} />
              </button>
            </span>
          </div>
        );
      })}
      <button type="button" className="src-add" onClick={onAdd}>
        Add source
      </button>
    </div>
  );
}
