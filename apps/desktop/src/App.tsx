import { useEffect, useState } from "react";
import TopBar, { type StepId } from "./components/TopBar";
import StatusBar from "./components/StatusBar";
import DiagnosticsSheet from "./components/DiagnosticsSheet";
import Toasts from "./components/Toasts";
import Workspace from "./components/studio/Workspace";
import ColorView from "./components/views/ColorView";
import { useProduction } from "./lib/productionStore";
import { isTauri } from "./lib/ipc";

/**
 * Root: one production workspace (Preview → Program, sources, scenes,
 * mixer, properties, output) plus the full colour-grading page, with the
 * health line always visible.
 */
export default function App() {
  const [step, setStep] = useState<StepId>("production");
  const [diagOpen, setDiagOpen] = useState(false);
  const init = useProduction((s) => s.init);
  const dispose = useProduction((s) => s.dispose);
  const refresh = useProduction((s) => s.refresh);
  const ready = useProduction((s) => s.ready);
  const recording = useProduction((s) => s.state?.record.state === "recording");

  useEffect(() => {
    void init();
    return () => dispose();
  }, [init, dispose]);

  // Lifecycle refresh (events also trigger refreshes); 1 s while recording
  // so the elapsed time ticks.
  useEffect(() => {
    const t = setInterval(() => {
      if (!document.hidden) void refresh();
    }, recording ? 1000 : 5000);
    return () => clearInterval(t);
  }, [refresh, recording]);

  // Ctrl+1 / Ctrl+2: Production / Color grading.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      if (e.key === "1" || e.key === "2") {
        e.preventDefault();
        setStep(e.key === "1" ? "production" : "color");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="app">
      <TopBar step={step} setStep={setStep} onDiagnostics={() => setDiagOpen(true)} />
      <main className="main-area">
        {!ready ? (
          <div className="boot">
            <div className="boot-title">{isTauri ? "Starting the engine…" : "StreamFlow runs inside the desktop app"}</div>
            <div className="boot-hint">{isTauri ? "Opening the GPU and audio devices." : "Open it with the StreamFlow desktop application."}</div>
          </div>
        ) : step === "production" ? (
          <Workspace onOpenColorPage={() => setStep("color")} />
        ) : (
          <ColorView />
        )}
      </main>
      <StatusBar onOpen={() => setDiagOpen(true)} />
      <DiagnosticsSheet open={diagOpen} onClose={() => setDiagOpen(false)} />
      <Toasts />
    </div>
  );
}
