import { useState } from "react";
import TopBar, { type ViewId } from "./components/TopBar";
import ProductionView from "./components/ProductionView";
import ColorStudioView from "./components/ColorStudioView";
import Toasts from "./components/Toasts";

/**
 * Root: slim top-level view switch (Production | Color Studio).
 * Color Studio stays exactly as it was; Production is the new live-production
 * workflow. The single toaster lives here so both views can surface errors.
 */
export default function App() {
  const [view, setView] = useState<ViewId>("production");

  return (
    <div className="app">
      <TopBar view={view} setView={setView} />
      {view === "production" ? <ProductionView /> : <ColorStudioView />}
      <Toasts />
    </div>
  );
}
