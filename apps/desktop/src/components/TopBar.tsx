import { useProduction } from "../lib/productionStore";

export type StepId = "input" | "preview" | "color" | "audio" | "stream";

export const STEPS: { id: StepId; label: string }[] = [
  { id: "input", label: "Input" },
  { id: "preview", label: "Preview" },
  { id: "color", label: "Color" },
  { id: "audio", label: "Audio" },
  { id: "stream", label: "Stream" },
];

/** Title, the five-step workflow switch, and the live state. */
export default function TopBar({ step, setStep }: { step: StepId; setStep: (s: StepId) => void }) {
  const stream = useProduction((s) => s.state?.stream.state ?? "idle");
  const record = useProduction((s) => s.state?.record.state ?? "idle");
  const live = stream === "live";
  const busy = stream === "connecting" || stream === "reconnecting";

  return (
    <header className="topbar">
      <div className="brand">StreamFlow</div>
      <nav className="steps" role="tablist" aria-label="Workflow">
        {STEPS.map((s, i) => (
          <button
            key={s.id}
            type="button"
            role="tab"
            aria-selected={step === s.id}
            className={step === s.id ? "active" : ""}
            onClick={() => setStep(s.id)}
            title={`${i + 1}. ${s.label}`}
          >
            {s.label}
          </button>
        ))}
      </nav>
      <div className="topbar-right">
        {record === "recording" && <span className="pill rec">REC</span>}
        <button
          type="button"
          className={"pill" + (live ? " live" : busy ? " warn" : "")}
          onClick={() => setStep("stream")}
          title="Stream settings"
        >
          {live ? "LIVE" : busy ? (stream === "connecting" ? "Connecting…" : "Reconnecting…") : stream === "error" ? "Stream error" : "Offline"}
        </button>
      </div>
    </header>
  );
}
