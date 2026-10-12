import { useStudio } from "../lib/store";

/** Live Color Studio preview: the same processed frame as the program/stream. */
export default function PreviewPane() {
  const previewUrl = useStudio((s) => s.previewUrl);
  const previewMode = useStudio((s) => s.previewMode);

  return (
    <div className="preview-pane">
      {previewUrl ? (
        <>
          <img src={previewUrl} alt="Color Studio preview" />
          <span className="preview-badge">
            {previewMode === "before" ? "BEFORE" : previewMode === "split" ? "BEFORE | AFTER" : "LIVE OUTPUT"}
          </span>
        </>
      ) : (
        <div className="preview-placeholder">
          <div>Preview — live frames stream here when running in the app</div>
          <div className="note">
            The preview shows the exact processed frame that goes to the main preview and the stream.
          </div>
        </div>
      )}
    </div>
  );
}
