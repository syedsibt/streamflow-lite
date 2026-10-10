import { useProduction } from "../../lib/productionStore";

/**
 * Preview + Program monitors side-by-side (~2 Hz PNG polling, same
 * best-effort pattern as the studio preview). Auto transitions render a
 * progress bar overlay on both monitors.
 */
function Monitor({
  label,
  url,
  kind,
  transitionActive,
  progress,
}: {
  label: string;
  url: string | null;
  kind: "preview" | "program";
  transitionActive: boolean;
  progress: number;
}) {
  return (
    <div className={`monitor ${kind}`}>
      <div className={`monitor-badge ${kind}`}>{label}</div>
      {url ? (
        <img src={url} alt={`${label} monitor`} />
      ) : (
        <div className="preview-placeholder">
          <div className="note">
            {label} — frames appear when running in Tauri
          </div>
        </div>
      )}
      {transitionActive && (
        <div className="transition-overlay">
          <div
            className="transition-progress"
            style={{ width: `${Math.round(progress * 100)}%` }}
          />
        </div>
      )}
    </div>
  );
}

export default function Monitors() {
  const previewUrl = useProduction((s) => s.previewUrl);
  const programUrl = useProduction((s) => s.programUrl);
  const transition = useProduction((s) => s.state?.transition ?? null);

  const active = transition?.active ?? false;
  const progress = transition?.progress ?? 0;

  return (
    <div className="monitors">
      <Monitor
        label="PREVIEW"
        url={previewUrl}
        kind="preview"
        transitionActive={active}
        progress={progress}
      />
      <Monitor
        label="PROGRAM"
        url={programUrl}
        kind="program"
        transitionActive={active}
        progress={progress}
      />
    </div>
  );
}
