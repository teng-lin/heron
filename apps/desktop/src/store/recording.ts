/**
 * Recording-session UI state.
 *
 * Owns the *frontend* affordances around an active capture — the timer
 * + Pause/Stop button row + the meeting handle returned by
 * `heron_start_capture`. The actual audio pipeline lives behind the
 * daemon (Gap #7 wired Start/Stop in the desktop UI to
 * `POST /v1/meetings` and `POST /v1/meetings/{id}/end`); this store
 * tracks "we asked the daemon to record, here's the meeting id it
 * gave us back" so the Stop button has a typed handle without
 * re-querying the meetings list.
 *
 * Fields:
 *
 *   - `recordingStart` — `Date.now()` when the user confirmed the
 *     consent gate. The `<Recording>` page derives the elapsed-time
 *     display from this; `null` when no session is active.
 *   - `meetingId` — id of the daemon-side meeting started for this
 *     session. `null` when idle, OR when a meeting started outside
 *     this app (e.g., CLI start) drove us to /recording — in that
 *     case the Stop handler falls back to the active-meeting id from
 *     `useMeetingsStore`.
 *   - `paused` — UI flag for the Pause button. Tier 3 #16 wired the
 *     daemon-side `POST /v1/meetings/{id}/pause` and `/resume` paths;
 *     `Recording.tsx`'s pause handler invokes them and only flips this
 *     flag on a successful daemon ack. The store action itself stays
 *     pure — it's the page-level wrapper that owns the network call.
 *
 * Actions:
 *
 *   - `start(meetingId)` — seed `recordingStart` + remember the
 *     daemon-issued meeting handle. Called from the Home page after
 *     `heron_start_capture` resolves Ok and the consent gate
 *     confirmed.
 *   - `stop()`  — clear `recordingStart` and `meetingId`. Called by
 *     the "Stop & Save" button after `heron_end_meeting` resolves.
 *   - `togglePause()` — flip the local `paused` flag. Tier 3 #16:
 *     callers MUST first hit the daemon's pause/resume HTTP endpoint
 *     (via `heron_pause_meeting` / `heron_resume_meeting`) and only
 *     invoke this on a successful ack. The store stays pure to keep
 *     it ergonomic in tests.
 */

import { create } from "zustand";

import type { MeetingId } from "../lib/types";

interface RecordingState {
  /** `Date.now()` when the consent gate confirmed; `null` when idle. */
  recordingStart: number | null;
  /**
   * Meeting id returned by the most recent `heron_start_capture`.
   * `null` when idle, or when the user navigated to /recording for a
   * meeting that wasn't started by this app (CLI / detector path).
   */
  meetingId: MeetingId | null;
  /** UI-only pause flag — does not currently affect capture. */
  paused: boolean;
  start: (meetingId: MeetingId | null) => void;
  stop: () => void;
  togglePause: () => void;
}

export const useRecordingStore = create<RecordingState>((set) => ({
  recordingStart: null,
  meetingId: null,
  paused: false,
  start: (meetingId) =>
    set({ recordingStart: Date.now(), meetingId, paused: false }),
  stop: () => set({ recordingStart: null, meetingId: null, paused: false }),
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
