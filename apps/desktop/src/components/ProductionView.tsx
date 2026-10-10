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

export default function ProductionView() {
  const init = useProduction((s) => s.init);
  const dispose = useProduction((s) => s.dispose);
  const pollMonitors = useProduction((s) => s.pollMonitors);
  const pollMultiview = useProduction((s) => s.pollMultiview);
  const refresh = useProduction((s) => s.refresh);
  const ready = useProduction((s) => s.ready);

  useEffect(() => {
    void init();
    // Monitors at ~2 Hz (the PNG renders are best-effort), multiview +
    // full state refresh slower.
    const t1 = setInterval(() => void pollMonitors(), 500);
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
