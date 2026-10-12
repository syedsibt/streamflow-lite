import { useStudio } from "../lib/store";

/** Live Color preview: the same processed frame as the program/stream. */
export default function PreviewPane() {
  const previewUrl = useStudio((s) => s.previewUrl);
  const previewMode = useStudio((s) => s.previewMode);

  return (
    <div className="preview-pane">
      <div className="monitor-frame">
        {previewUrl ? (
          <>
            <img src={previewUrl} alt="Color preview" draggable={false} />
            <span className="preview-badge">
              {previewMode === "before" ? "BEFORE" : previewMode === "split" ? "BEFORE | AFTER" : "OUTPUT"}
            </span>
          </>
        ) : (
          <div className="monitor-empty">
            <div className="monitor-empty-title">No picture yet</div>
            <div className="monitor-empty-hint">The preview shows exactly what goes to the stream.</div>
          </div>
        )}
      </div>
    </div>
  );
}
