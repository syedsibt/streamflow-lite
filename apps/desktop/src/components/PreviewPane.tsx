import { useStudio } from "../lib/store";

export default function PreviewPane() {
  const previewUrl = useStudio((s) => s.previewUrl);

  return (
    <div className="preview-pane">
      {previewUrl ? (
        <img src={previewUrl} alt="Program preview" />
      ) : (
        <div className="preview-placeholder">
          <div>Preview — frames stream here when running in Tauri</div>
          <div className="note">
            pollPreview() runs every 500 ms; the backend throttles rendering to
            ~10 Hz.
          </div>
        </div>
      )}
    </div>
  );
}
