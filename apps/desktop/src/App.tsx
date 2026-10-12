import { useEffect, useState } from "react";
import TopBar, { type StepId } from "./components/TopBar";
import StatusBar from "./components/StatusBar";
import DiagnosticsSheet from "./components/DiagnosticsSheet";
import Toasts from "./components/Toasts";
import { useMonitorPolling } from "./components/Monitor";
import InputView from "./components/views/InputView";
import PreviewView from "./components/views/PreviewView";
import ColorView from "./components/views/ColorView";
import AudioView from "./components/views/AudioView";
import StreamView from "./components/views/StreamView";
import { useProduction } from "./lib/productionStore";
import { isTauri } from "./lib/ipc";

/**
 * Root: one workflow — INPUT → PREVIEW → COLOR → AUDIO → STREAM — over a
 * single program output, with a status bar that is always visible.
 */
export default function App() {
  const [step, setStep] = useState<StepId>("input");
  const [diagOpen, setDiagOpen] = useState(false);
  const init = useProduction((s) => s.init);
  const dispose = useProduction((s) => s.dispose);
  const refresh = useProduction((s) => s.refresh);
  const ready = useProduction((s) => s.ready);
  const recording = useProduction((s) => s.state?.record.state === "recording");

  useMonitorPolling();

  useEffect(() => {
    void init();
    return () => dispose();
  }, [init, dispose]);

  // Lifecycle/meters refresh (events also trigger refreshes). Faster while
  // recording so the elapsed time ticks.
  useEffect(() => {
    const t = setInterval(() => {
      if (!document.hidden) void refresh();
    }, recording ? 1000 : 2000);
    return () => clearInterval(t);
  }, [refresh, recording]);

  // ⌘/Ctrl + 1…5 jumps between steps.
  useEffect(() => {
    const order: StepId[] = ["input", "preview", "color", "audio", "stream"];
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      const n = Number(e.key);
      if (n >= 1 && n <= 5) {
        e.preventDefault();
        setStep(order[n - 1]);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="app">
      <TopBar step={step} setStep={setStep} />
      <main className="main-area">
        {!ready ? (
          <div className="boot">
            <div className="boot-title">{isTauri ? "Starting the engine…" : "StreamFlow runs inside the desktop app"}</div>
            <div className="boot-hint">{isTauri ? "Opening the GPU and audio devices." : "Open it with the StreamFlow desktop application."}</div>
          </div>
        ) : step === "input" ? (
          <InputView />
        ) : step === "preview" ? (
          <PreviewView />
        ) : step === "color" ? (
          <ColorView />
        ) : step === "audio" ? (
          <AudioView />
        ) : (
          <StreamView />
        )}
      </main>
      <StatusBar onOpen={() => setDiagOpen(true)} />
      <DiagnosticsSheet open={diagOpen} onClose={() => setDiagOpen(false)} />
      <Toasts />
    </div>
  );
}
