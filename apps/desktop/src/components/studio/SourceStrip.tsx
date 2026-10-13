import { useEffect, useRef, useState } from "react";
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
    if (d.paused) return { text: "Paused (not on Preview or Program, no decoding)", tone: "off" };
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
 * placement locked, bin = delete (click twice). Drag a block left/right to
 * change the layer order (left = drawn on top).
 */
export default function SourceStrip({ onAdd }: { onAdd: () => void }) {
  const state = useProduction((s) => s.state);
  const diag = useProduction((s) => s.diag);
  const selected = useProduction((s) => s.selectedSourceId);
  const select = useProduction((s) => s.selectSource);
  const setVisible = useProduction((s) => s.sourceSetVisible);
  const setLocked = useProduction((s) => s.sourceSetLocked);
  const remove = useProduction((s) => s.sourceRemove);
  const reorder = useProduction((s) => s.sourceReorder);
  const sceneId = useProduction((s) => s.sceneId());
  const scene = state?.scenes.find((s) => s.id === sceneId);
  const all = [...(scene?.sources ?? [])].sort((a, b) => b.order - a.order); // top first
  const sources = all.filter((s) => s.kind !== "audioinput");
  const target = diag?.target_fps ?? state?.video_settings.output_fps ?? 30;
  const [confirmId, setConfirmId] = useState<string | null>(null);
  const confirmTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [dragId, setDragId] = useState<string | null>(null);
  const dragIdRef = useRef<string | null>(null);
  const [drop, setDrop] = useState<{ id: string; after: boolean } | null>(null);

  useEffect(() => () => {
    if (confirmTimer.current) clearTimeout(confirmTimer.current);
  }, []);

  // Pointer drag (HTML5 drag-and-drop is taken over by the desktop shell's
  // file-drop handling, so it is done by hand): press, move > 6 px, release
  // over another block.
  const press = useRef<{ id: string; x: number; y: number } | null>(null);
  const justDragged = useRef(false);
  const dropRef = useRef<{ id: string; after: boolean } | null>(null);
  const moveToRef = useRef<(id: string, to: string, after: boolean) => void>(() => {});
  useEffect(() => {
    const onMove = (e: PointerEvent) => {
      const p = press.current;
      if (!p) return;
      if (!dragIdRef.current) {
        if (Math.abs(e.clientX - p.x) + Math.abs(e.clientY - p.y) < 6) return;
        dragIdRef.current = p.id;
        setDragId(p.id);
      }
      const el = (document.elementFromPoint(e.clientX, e.clientY) as HTMLElement | null)?.closest<HTMLElement>("[data-src-id]");
      const id = el?.dataset.srcId;
      if (!el || !id || id === p.id) {
        dropRef.current = null;
        setDrop(null);
        return;
      }
      const r = el.getBoundingClientRect();
      const next = { id, after: e.clientX > r.left + r.width / 2 };
      if (dropRef.current?.id !== next.id || dropRef.current.after !== next.after) {
        dropRef.current = next;
        setDrop(next);
      }
    };
    const onUp = () => {
      const p = press.current;
      press.current = null;
      if (p && dragIdRef.current) {
        justDragged.current = true;
        setTimeout(() => (justDragged.current = false), 0);
        const d = dropRef.current;
        if (d) moveToRef.current(p.id, d.id, d.after);
      }
      dragIdRef.current = null;
      dropRef.current = null;
      setDragId(null);
      setDrop(null);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, []);

  /** First press arms (3 s), second press deletes. */
  const askDelete = (id: string) => {
    if (confirmTimer.current) clearTimeout(confirmTimer.current);
    if (confirmId === id) {
      setConfirmId(null);
      if (selected === id) select(null);
      void remove(id);
      return;
    }
    setConfirmId(id);
    confirmTimer.current = setTimeout(() => setConfirmId(null), 3000);
  };

  /** Move `id` next to `to` (before or after it in the top-first list). */
  const moveTo = (id: string, to: string, after: boolean) => {
    if (id === to) return;
    const visual = sources.map((s) => s.id).filter((x) => x !== id);
    const at = visual.indexOf(to);
    if (at < 0) return;
    visual.splice(after ? at + 1 : at, 0, id);
    // Backend order is bottom first; audio-only sources keep the bottom.
    const audio = all.filter((s) => s.kind === "audioinput").map((s) => s.id);
    void reorder([...audio, ...visual.reverse()]);
  };

  moveToRef.current = moveTo;

  const moveBy = (id: string, dir: -1 | 1) => {
    const i = sources.findIndex((s) => s.id === id);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= sources.length) return;
    moveTo(id, sources[j].id, dir === 1);
  };

  return (
    <div className="source-strip" role="listbox" aria-label={`Sources in ${scene?.name ?? "scene"}`}>
      <div className="strip-title">
        <span>Sources</span>
        <small>{scene?.name}</small>
      </div>
      {sources.map((s) => {
        const d = diag?.sources.find((x) => x.id === s.id);
        const st = sourceStatus(s, d, target);
        const arming = confirmId === s.id;
        return (
          <div
            key={s.id}
            role="option"
            aria-selected={selected === s.id}
            tabIndex={0}
            data-src-id={s.id}
            className={
              "src-tile" +
              (selected === s.id ? " sel" : "") +
              (s.visible ? "" : " hidden") +
              " t-" +
              st.tone +
              (dragId === s.id ? " dragging" : "") +
              (drop?.id === s.id && dragId && dragId !== s.id ? (drop.after ? " drop-after" : " drop-before") : "")
            }
            title="Drag left or right to change the layer order (left is on top)"
            onClick={() => {
              if (justDragged.current) {
                justDragged.current = false;
                return;
              }
              select(s.id);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") select(s.id);
              else if (e.key === "Delete" || e.key === "Backspace") {
                e.preventDefault();
                askDelete(s.id);
              } else if (e.altKey && e.key === "ArrowLeft") moveBy(s.id, -1);
              else if (e.altKey && e.key === "ArrowRight") moveBy(s.id, 1);
            }}
            onPointerDown={(e) => {
              if (e.button !== 0 || (e.target as HTMLElement).closest("button")) return;
              press.current = { id: s.id, x: e.clientX, y: e.clientY };
            }}
          >
            <span className="src-glyph">
              <Glyph name={KIND_GLYPH[s.kind] ?? "pattern"} size={16} />
            </span>
            <span className="src-text">
              <span className="src-name" title={s.name}>
                {s.name}
              </span>
              <span className="src-status" title={st.text}>
                {arming ? "Click the bin again to delete" : st.text}
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
              <button
                type="button"
                className={"icon-btn del" + (arming ? " armed" : "")}
                title={arming ? "Click again to delete this source" : "Delete source"}
                aria-label={arming ? `Confirm delete ${s.name}` : `Delete ${s.name}`}
                onClick={(e) => {
                  e.stopPropagation();
                  askDelete(s.id);
                }}
              >
                <Glyph name="trash" size={14} />
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
