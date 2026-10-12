/**
 * COLOR: grade the selected source. The preview is the exact graded frame
 * the program/encoder uses (live tap of the render — no second pipeline),
 * with Before / Split / After for comparison (preview only; the stream
 * always keeps the grade).
 */
import { useEffect } from "react";
import { useStudio, type TabId } from "../../lib/store";
import { useProduction } from "../../lib/productionStore";
import Header from "../Header";
import PreviewPane from "../PreviewPane";
import ScopesRow from "../ScopesRow";
import BasicTab from "../tabs/BasicTab";
import CreativeTab from "../tabs/CreativeTab";
import CurvesTab from "../tabs/CurvesTab";
import WheelsTab from "../tabs/WheelsTab";
import LutTab from "../tabs/LutTab";
import EffectsTab from "../tabs/EffectsTab";
import FiltersTab from "../tabs/FiltersTab";
import PresetsTab from "../tabs/PresetsTab";
import LooksStrip from "../LooksStrip";

const TABS: { id: TabId; label: string }[] = [
  { id: "basic", label: "Basic" },
  { id: "creative", label: "Creative" },
  { id: "curves", label: "Curves" },
  { id: "wheels", label: "Wheels" },
  { id: "lut", label: "LUT" },
  { id: "effects", label: "Glow & Skin" },
  { id: "filters", label: "Effects" },
  { id: "presets", label: "Presets" },
];

export default function ColorView() {
  const init = useStudio((s) => s.init);
  const activeTab = useStudio((s) => s.activeTab);
  const setTab = useStudio((s) => s.setTab);
  const pollPreview = useStudio((s) => s.pollPreview);
  const hasVideo = useProduction((s) =>
    (s.state?.scenes ?? []).some((sc) => sc.sources.some((src) => src.input_id)),
  );

  useEffect(() => {
    void (async () => {
      await init();
      // Target the first real source so grades affect the live video.
      const st = useStudio.getState();
      if (st.inputId === "cam1") {
        const first = (useProduction.getState().state?.scenes ?? [])
          .flatMap((sc) => sc.sources)
          .find((s) => s.input_id);
        if (first?.input_id) await st.setInputId(first.input_id, first.name);
      }
    })();
    let stop = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    // Display-rate preview with an in-flight guard (never queues).
    const loop = async () => {
      if (stop) return;
      if (!document.hidden) await pollPreview();
      if (!stop) timer = setTimeout(() => void loop(), 50);
    };
    void loop();
    return () => {
      stop = true;
      if (timer) clearTimeout(timer);
    };
  }, [init, pollPreview]);

  return (
    <div className="workspace color">
      <div className="stage-area">
        <Header />
        <PreviewPane />
        {!hasVideo && <div className="hint-line">Add a camera or media source in Input to grade it.</div>}
        <ScopesRow />
      </div>
      <aside className="inspector color-inspector">
        <div className="tabs" role="tablist" aria-label="Color tools">
          {TABS.map((t) => (
            <button
              key={t.id}
              type="button"
              role="tab"
              aria-selected={activeTab === t.id}
              title={t.label}
              className={activeTab === t.id ? "active" : ""}
              onClick={() => setTab(t.id)}
            >
              {t.label}
            </button>
          ))}
        </div>
        <div className="tab-panel">
          {activeTab === "basic" && <BasicTab />}
          {activeTab === "creative" && <CreativeTab />}
          {activeTab === "curves" && <CurvesTab />}
          {activeTab === "wheels" && <WheelsTab />}
          {activeTab === "lut" && <LutTab />}
          {activeTab === "effects" && <EffectsTab />}
          {activeTab === "filters" && <FiltersTab />}
          {activeTab === "presets" && <PresetsTab />}
        </div>
        <LooksStrip />
      </aside>
    </div>
  );
}
