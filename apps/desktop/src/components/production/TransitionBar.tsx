import { useProduction } from "../../lib/productionStore";

const DURATIONS = [250, 500, 1000, 2000];

/** Transition bar: CUT, AUTO, duration selector, kind selector ("Mix"). */
export default function TransitionBar() {
  const transitionCut = useProduction((s) => s.transitionCut);
  const transitionAuto = useProduction((s) => s.transitionAuto);
  const transitionDurationMs = useProduction((s) => s.transitionDurationMs);
  const setTransitionDurationMs = useProduction((s) => s.setTransitionDurationMs);
  const transitionKind = useProduction((s) => s.transitionKind);
  const setTransitionKind = useProduction((s) => s.setTransitionKind);
  const transitionActive = useProduction((s) => s.state?.transition.active ?? false);
  const progress = useProduction((s) => s.state?.transition.progress ?? 0);

  return (
    <div className="panel trans-bar">
      <button
        className="btn cut"
        onClick={() => void transitionCut()}
        title="Instant cut preview → program"
      >
        CUT
      </button>
      <button
        className="btn auto"
        onClick={() => void transitionAuto()}
        disabled={transitionActive}
        title="Animated transition preview → program"
      >
        {transitionActive ? `AUTO ${Math.round(progress * 100)}%` : "AUTO"}
      </button>
      <select
        value={transitionDurationMs}
        onChange={(e) => setTransitionDurationMs(parseInt(e.target.value, 10))}
        title="Transition duration"
      >
        {DURATIONS.map((d) => (
          <option key={d} value={d}>
            {d} ms
          </option>
        ))}
      </select>
      <select
        value={transitionKind}
        onChange={(e) => setTransitionKind(e.target.value)}
        title="Transition kind"
      >
        <option value="Mix">Mix</option>
      </select>
      {transitionActive && (
        <div className="trans-progress-track">
          <div
            className="trans-progress-fill"
            style={{ width: `${Math.round(progress * 100)}%` }}
          />
        </div>
      )}
    </div>
  );
}
