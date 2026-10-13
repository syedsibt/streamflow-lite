/**
 * Live audio meters, outside React state: one poller (~25 Hz, in-flight
 * guarded) feeds subscribers that draw straight to canvas. Meters never
 * re-render the component tree.
 */
import { api, isTauri } from "../../lib/ipc";
import type { AudioMeters } from "../../lib/types";

type Listener = (m: AudioMeters) => void;
const listeners = new Set<Listener>();
let timer: ReturnType<typeof setTimeout> | null = null;
let inFlight = false;
export let latestMeters: AudioMeters | null = null;

/** Polls since the last non-silent reading (slows the poll when nothing plays). */
let silentPolls = 0;

async function tick() {
  timer = null;
  if (listeners.size === 0) return;
  if (!document.hidden && isTauri && !inFlight) {
    inFlight = true;
    try {
      latestMeters = await api.audioMeters();
      const m = latestMeters;
      const loud = m.master_peak > 1e-5 || m.channels.some((c) => c.peak > 1e-5);
      silentPolls = loud ? 0 : silentPolls + 1;
      listeners.forEach((l) => l(m));
    } catch {
      /* engine busy; next tick retries */
    } finally {
      inFlight = false;
    }
  }
  // 25 Hz while there is sound; after 2 s of silence 4 Hz (meters read
  // −∞ anyway) — the first sound brings 25 Hz back within 250 ms.
  timer = setTimeout(() => void tick(), silentPolls > 50 ? 250 : 40);
}

export function subscribeMeters(l: Listener): () => void {
  listeners.add(l);
  if (!timer) timer = setTimeout(() => void tick(), 0);
  return () => {
    listeners.delete(l);
  };
}

export const toDb = (lin: number) => (lin > 1e-6 ? 20 * Math.log10(lin) : -120);
/** −60 dBFS … 0 dBFS → 0 … 1 (meter scale). */
export const dbToPos = (db: number) => Math.max(0, Math.min(1, (db + 60) / 60));

/** Peak hold: holds the highest peak 1.5 s, then falls 20 dB/s. */
export class PeakHold {
  value = -120;
  private at = 0;
  update(db: number, now: number) {
    if (db >= this.value) {
      this.value = db;
      this.at = now;
    } else if (now - this.at > 1500) {
      this.value = Math.max(db, this.value - ((now - this.at - 1500) / 1000) * 20);
    }
    return this.value;
  }
}
