/**
 * Recording-session UI state.
 *
 * Phase 64 (PR-β) only owns the *frontend* affordances — the actual
 * audio capture pipeline still lives behind `heron-cli` and gets wired
 * in a later phase. This store tracks the UI's view of "we think we
 * are recording", so the timer + Stop & Save button can render
 * without round-tripping every tick to Rust.
 *
 * Fields:
 *
 *   - `recordingStart` — `Date.now()` when the user confirmed the
 *     consent gate. The `<Recording>` page derives the elapsed-time
 *     display from this; `null` when no session is active.
 *   - `paused` — UI flag for the Pause button. Stub-only in this PR;
 *     the real audio pipeline doesn't yet expose pause/resume.
 *
 * Actions:
 *
 *   - `start()` — seed `recordingStart` with `Date.now()` and reset
 *     `paused`. Called from the consent-gate's confirm handler.
 *   - `stop()`  — clear `recordingStart`. Called by the
 *     "Stop & Save" button after dispatching the (currently absent)
 *     `heron_stop_recording` Tauri command.
 *   - `togglePause()` — flip the local `paused` flag. UI-only.
 */

import { create } from "zustand";

interface RecordingState {
  /** `Date.now()` when the consent gate confirmed; `null` when idle. */
  recordingStart: number | null;
  /** UI-only pause flag — does not currently affect capture. */
  paused: boolean;
  start: () => void;
  stop: () => void;
  togglePause: () => void;
}

export const useRecordingStore = create<RecordingState>((set) => ({
  recordingStart: null,
  paused: false,
  start: () => set({ recordingStart: Date.now(), paused: false }),
  stop: () => set({ recordingStart: null, paused: false }),
  togglePause: () => set((s) => ({ paused: !s.paused })),
}));

/**
 * Format an elapsed-millisecond delta as `HH:MM:SS`.
 *
 * Pulled out of the React tree so a unit test can pin the formatting
 * (zero-padding + the carry behaviour at hour 100). We deliberately
 * accept the deg-of-freedom that very long sessions (≥ 100 hours) keep
 * growing the hours field — the alternative (clamping to "99:59:59")
 * would silently corrupt durations on the rare overnight regression.
 */
export function formatElapsed(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) {
    return "00:00:00";
  }
  const totalSeconds = Math.floor(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  const pad = (n: number) => n.toString().padStart(2, "0");
  return `${pad(hours)}:${pad(minutes)}:${pad(seconds)}`;
}
