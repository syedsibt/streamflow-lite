/**
 * Color Studio screen (spec §8, PDF p.11). Extracted verbatim from the
 * original App.tsx so the screen stays exactly as-is; App.tsx now switches
 * between this and the Production view.
 *
 *   Header: Color Studio | [CAM 1 ▾] | A | B | Before/After | Reset
 *   Left:   large preview, scopes row (waveform | vectorscope | histogram)
 *   Right:  tabs [Basic][Pro][Curves][LUT][Filters]
 *   Bottom: looks strip
 */
import { useEffect } from "react";
import { useStudio, type TabId } from "../lib/store";
import Header from "./Header";
import PreviewPane from "./PreviewPane";
import ScopesRow from "./ScopesRow";
import BasicTab from "./tabs/BasicTab";
import ProTab from "./tabs/ProTab";
import CurvesTab from "./tabs/CurvesTab";
import LutTab from "./tabs/LutTab";
import FiltersTab from "./tabs/FiltersTab";
import LooksStrip from "./LooksStrip";

const TABS: { id: TabId; label: string }[] = [
  { id: "basic", label: "Basic" },
  { id: "pro", label: "Pro" },
  { id: "curves", label: "Curves" },
  { id: "lut", label: "LUT" },
  { id: "filters", label: "Filters" },
];

export default function ColorStudioView() {
  const init = useStudio((s) => s.init);
  const activeTab = useStudio((s) => s.activeTab);
  const setTab = useStudio((s) => s.setTab);
  const pollPreview = useStudio((s) => s.pollPreview);

  useEffect(() => {
    void init();
    // Preview poll: the backend throttles rendering to ~10 Hz itself.
    const t = setInterval(() => void pollPreview(), 500);
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
            {activeTab === "pro" && <ProTab />}
            {activeTab === "curves" && <CurvesTab />}
            {activeTab === "lut" && <LutTab />}
            {activeTab === "filters" && <FiltersTab />}
          </div>
        </div>
      </div>
      <LooksStrip />
    </div>
  );
}
