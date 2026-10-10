export type ViewId = "production" | "studio";

interface TopBarProps {
  view: ViewId;
  setView: (v: ViewId) => void;
}

/** Slim top-level view switch: Production | Color Studio. */
export default function TopBar({ view, setView }: TopBarProps) {
  return (
    <div className="viewbar">
      <div className="viewbar-title">StreamFlow Lite</div>
      <div className="viewbar-tabs">
        <button
          className={view === "production" ? "active" : ""}
          onClick={() => setView("production")}
        >
          Production
        </button>
        <button
          className={view === "studio" ? "active" : ""}
          onClick={() => setView("studio")}
        >
          Color Studio
        </button>
      </div>
    </div>
  );
}
