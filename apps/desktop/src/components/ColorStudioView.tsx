/**
 * Color Studio screen (spec §8, PDF p.11). Extracted verbatim from the
 * original App.tsx so the screen stays exactly as-is; App.tsx now switches
 * between this and the Production view.
 *
 *   Header: Color Studio | [source ▾] | A | B | Before/Split/After | Reset
 *   Left:   live preview (= program/stream frame), scopes row
 *   Right:  tabs [Basic][Creative][Curves][Wheels][LUT][Glow & Skin][Effects][Presets]
 *   Bottom: filter strip (categories, one active filter, live intensity)
 */
import { useEffect } from "react";
import { useStudio, type TabId } from "../lib/store";
import { useProduction } from "../lib/productionStore";
import Header from "./Header";
import PreviewPane from "./PreviewPane";
import ScopesRow from "./ScopesRow";
import BasicTab from "./tabs/BasicTab";
import CreativeTab from "./tabs/CreativeTab";
import CurvesTab from "./tabs/CurvesTab";
import WheelsTab from "./tabs/WheelsTab";
import LutTab from "./tabs/LutTab";
import EffectsTab from "./tabs/EffectsTab";
import FiltersTab from "./tabs/FiltersTab";
import PresetsTab from "./tabs/PresetsTab";
import LooksStrip from "./LooksStrip";

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

export default function ColorStudioView() {
  const init = useStudio((s) => s.init);
  const activeTab = useStudio((s) => s.activeTab);
  const setTab = useStudio((s) => s.setTab);
  const pollPreview = useStudio((s) => s.pollPreview);

  useEffect(() => {
    void (async () => {
      await init();
      // Ensure production state is loaded so the source selector knows
      // the real inputs.
      await useProduction.getState().init();
      // Auto-target the first real production source: filters and grades
      // must affect the live video, not the disconnected default input.
      const st = useStudio.getState();
      if (st.inputId === "cam1") {
        const scenes = useProduction.getState().state?.scenes ?? [];
        const first = scenes
          .flatMap((sc) => sc.sources)
          .find((s) => s.input_id);
        if (first?.input_id) await st.setInputId(first.input_id, first.name);
      }
    })();
    // Live preview at ~10 fps. The backend serves the frame the media tick
    // already produced (no extra grade); pollPreview never queues requests.
    const t = setInterval(() => void pollPreview(), 100);
    return () => clearInterval(t);
  }, [init, pollPreview]);

  return (
    <div className="studio-view">
      <Header />
      <div className="main">
        <div className="left">
          <PreviewPane />
          <ScopesRow />
        </div>
        <div className="right">
          <div className="tabs">
            {TABS.map((t) => (
              <button
                key={t.id}
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
        </div>
      </div>
      <LooksStrip />
    </div>
  );
}
