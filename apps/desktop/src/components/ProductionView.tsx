/**
 * Production view: vMix-inspired live-production workflow.
 *
 *   Left:   Scenes panel + Sources panel (of the preview scene)
 *   Center: Preview + Program monitors, transition bar, multiview grid
 *   Right:  Mixer, macros, stream, record
 */
import { useEffect } from "react";
import { useProduction } from "../lib/productionStore";
import ScenesPanel from "./production/ScenesPanel";
import SourcesPanel from "./production/SourcesPanel";
import Monitors from "./production/Monitors";
import TransitionBar from "./production/TransitionBar";
import MultiviewGrid from "./production/MultiviewGrid";
import MixerPanel from "./production/MixerPanel";
import MacrosPanel from "./production/MacrosPanel";
import StreamPanel from "./production/StreamPanel";
import RecordPanel from "./production/RecordPanel";
import PerfPanel from "./PerfPanel";

export default function ProductionView() {
  const init = useProduction((s) => s.init);
  const dispose = useProduction((s) => s.dispose);
  const pollMonitors = useProduction((s) => s.pollMonitors);
  const pollMultiview = useProduction((s) => s.pollMultiview);
  const refresh = useProduction((s) => s.refresh);
  const ready = useProduction((s) => s.ready);

  useEffect(() => {
    void init();
    // Monitors at ~10 Hz: the program monitor is served from the media
    // tick's cached frame (no re-render), preview/multiview render on demand.
    // Monitors follow the media tick (~30fps): both program and preview
    // are served from the tick's cached JPEGs, so polling faster than the
    // tick only burns CPU. 33ms keeps perceived capture latency minimal.
    const t1 = setInterval(() => void pollMonitors(), 33);
    const t2 = setInterval(() => void pollMultiview(), 2000);
    const t3 = setInterval(() => void refresh(), 2000);
    return () => {
      clearInterval(t1);
      clearInterval(t2);
      clearInterval(t3);
      dispose();
    };
  }, [init, dispose, pollMonitors, pollMultiview, refresh]);

  if (!ready) {
    return (
      <div className="prod-view prod-placeholder">
        <div className="preview-placeholder">
          <div>Production — waiting for backend</div>
          <div className="note">
            production_state() is called on init; panels fill in when running
            in Tauri.
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="prod-view">
      <div className="prod-left">
        <ScenesPanel />
        <SourcesPanel />
      </div>
      <div className="prod-center">
        <div style={{ display: "flex", justifyContent: "flex-end", marginBottom: 8 }}>
          <PerfPanel />
        </div>
        <Monitors />
        <TransitionBar />
        <MultiviewGrid />
      </div>
      <div className="prod-right">
        <MixerPanel />
        <MacrosPanel />
        <StreamPanel />
        <RecordPanel />
      </div>
    </div>
  );
}
