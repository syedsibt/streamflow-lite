import { useState } from "react";
import TopBar, { type ViewId } from "./components/TopBar";
import ProductionView from "./components/ProductionView";
import ColorStudioView from "./components/ColorStudioView";
import VideoSettingsView from "./components/VideoSettingsView";
import Toasts from "./components/Toasts";

/**
 * Root: slim top-level view switch (Production | Color Studio | Video).
 * Color Studio stays exactly as it was; Production is the new live-production
 * workflow; Video holds the manual input/output structure. The single toaster
 * lives here so all views can surface errors.
 */
export default function App() {
  const [view, setView] = useState<ViewId>("production");

  return (
    <div className="app">
      <TopBar view={view} setView={setView} />
      {view === "production" ? (
        <ProductionView />
      ) : view === "video" ? (
        <VideoSettingsView />
      ) : (
        <ColorStudioView />
      )}
      <Toasts />
    </div>
  );
}
