import { useEffect, useRef, useState } from "react";
import { useProduction } from "../../lib/productionStore";
import AddSourceDialog from "./AddSourceDialog";
import Mixer from "./Mixer";
import { useMonitorPolling } from "../Monitor";
import Monitors from "./Monitors";
import OutputPanel from "./OutputPanel";
import PropertiesPanel from "./PropertiesPanel";
import ScenesPanel from "./ScenesPanel";
import SourceStrip from "./SourceStrip";

const load = (k: string, d: number) => {
  try {
    const v = Number(localStorage.getItem(k));
    return Number.isFinite(v) && v > 0 ? v : d;
  } catch {
    return d;
  }
};
const save = (k: string, v: number | string) => {
  try {
    localStorage.setItem(k, String(v));
  } catch {
    /* private mode */
  }
};

/**
 * PRODUCTION: Preview | switch | Program on top, the scene's sources
 * below, and the dock — Scenes, Audio mixer, Properties, Output — so every
 * live action is one click away. The dock height is draggable and the dock
 * can be hidden to give the monitors the whole window.
 */
export default function Workspace({ onOpenColorPage }: { onOpenColorPage: () => void }) {
  // Program monitor polling lives with the monitors: nothing is fetched
  // (or rendered by the engine) while another page is open.
  useMonitorPolling();
  const [adding, setAdding] = useState(false);
  const [width, setWidth] = useState(() => window.innerWidth);
  useEffect(() => {
    const on = () => setWidth(window.innerWidth);
    window.addEventListener("resize", on);
    return () => window.removeEventListener("resize", on);
  }, []);
  // Narrow windows: Output (and below 1000 px Scenes) fold into Properties
  // tabs instead of disappearing.
  const foldOutput = width < 1400;
  const foldScenes = width < 1000;
  const [dockH, setDockH] = useState(() => load("sf.dock.h", Math.round(Math.min(380, Math.max(220, window.innerHeight * 0.34)))));
  const [dockHidden, setDockHidden] = useState(() => load("sf.dock.hidden", 0) === 1);
  const drag = useRef<{ y: number; h: number } | null>(null);
  const state = useProduction((s) => s.state);
  const cut = useProduction((s) => s.cut);
  const auto = useProduction((s) => s.auto);
  const setPreview = useProduction((s) => s.setPreviewScene);
  const selected = useProduction((s) => s.selectedSourceId);
  const select = useProduction((s) => s.selectSource);

  // Operator keys (ignored while typing): Enter = cut, Space = auto,
  // 1–9 = scene to preview, Esc = deselect.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "SELECT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      if (e.ctrlKey || e.metaKey || e.altKey || adding) return;
      if (e.key === "Enter") {
        e.preventDefault();
        void cut();
      } else if (e.key === " ") {
        e.preventDefault();
        void auto();
      } else if (/^[1-9]$/.test(e.key)) {
        const sc = state?.scenes[Number(e.key) - 1];
        if (sc) void setPreview(sc.id);
      } else if (e.key === "Escape" && selected) {
        select(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cut, auto, setPreview, state?.scenes, adding, selected, select]);

  const onSplitDown = (e: React.PointerEvent) => {
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    drag.current = { y: e.clientY, h: dockH };
  };
  const onSplitMove = (e: React.PointerEvent) => {
    if (!drag.current) return;
    const h = Math.max(180, Math.min(window.innerHeight - 260, drag.current.h - (e.clientY - drag.current.y)));
    setDockH(h);
  };
  const onSplitUp = () => {
    if (drag.current) save("sf.dock.h", dockH);
    drag.current = null;
  };
  const toggleDock = () => {
    setDockHidden((v) => {
      save("sf.dock.hidden", v ? 0 : 1);
      return !v;
    });
  };

  return (
    <div className="studio-ws" style={{ ["--dock-h" as string]: dockHidden ? "0px" : `${dockH}px` }}>
      <Monitors />
      <SourceStrip onAdd={() => setAdding(true)} />
      <div className="splitter" role="separator" aria-orientation="horizontal" aria-label="Resize the dock" onPointerDown={onSplitDown} onPointerMove={onSplitMove} onPointerUp={onSplitUp} onDoubleClick={toggleDock}>
        <button type="button" className="dock-toggle" onPointerDown={(e) => e.stopPropagation()} onClick={toggleDock}>
          {dockHidden ? "Show mixer and properties" : "Hide dock"}
        </button>
      </div>
      {!dockHidden && (
        <div className={"dock" + (foldOutput ? " fold-out" : "") + (foldScenes ? " fold-scenes" : "")}>
          {!foldScenes && (
            <section className="panel scenes-panel" aria-label="Scenes">
              <h2>Scenes</h2>
              <ScenesPanel />
            </section>
          )}
          <section className="panel mixer-panel" aria-label="Audio mixer">
            <h2>Audio mixer</h2>
            <Mixer />
          </section>
          <section className="panel props-panel" aria-label="Properties">
            <PropertiesPanel
              onOpenColorPage={onOpenColorPage}
              extra={[
                ...(foldScenes ? [{ id: "scenes", label: "Scenes", node: <ScenesPanel /> }] : []),
                ...(foldOutput ? [{ id: "output", label: "Output", node: <OutputPanel /> }] : []),
              ]}
            />
          </section>
          {!foldOutput && (
            <section className="panel output-panel" aria-label="Output">
              <OutputPanel />
            </section>
          )}
        </div>
      )}
      {adding && <AddSourceDialog onClose={() => setAdding(false)} />}
    </div>
  );
}
