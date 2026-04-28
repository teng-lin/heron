/**
 * Recording-session UI state, with the daemon-talking start/stop
 * surface bound to `POST /v1/meetings` + `POST /v1/meetings/{id}/end`
 * via Tauri commands.
 *
 * Before this store landed in its current form, `start()` / `stop()`
 * only flipped a Zustand flag and the audio capture pipeline was CLI-
 * only. Now `start()` runs platform detection → start_capture and
 * persists the returned `meetingId` so the matching `stop()` knows
 * which session to end. Capture itself happens daemon-side; this
 * store just owns the UI's view of "we asked the daemon to record."
 *
 * Fields:
 *
 *   - `meetingId` — the daemon's `MeetingId` for the in-progress
 *     capture. `null` when no session is active. Set on a successful
 *     `start()`, cleared by `stop()` (after the end_meeting call
 *     resolves) and by `cancel()`.
 *   - `recordingStart` — `Date.now()` when the daemon ack'd
 *     `start_capture`. The `<Recording>` page derives the elapsed-
 *     time display from this; `null` when no session is active.
 *     Tracked alongside `meetingId` rather than read from the
 *     `Meeting` envelope so the timer can render before the SSE
 *     `meeting.started` event lands.
 *   - `paused` — UI flag for the Pause button. The daemon does not
 *     yet expose pause/resume; this stays UI-only and the broadcast
 *     stream keeps capturing. Surfacing it would require a new
 *     daemon endpoint and an FSM extension; deferred.
 *
 * Actions:
 *
 *   - `start()` — async. Detects the running meeting platform, then
 *     calls `heron_start_capture`. Returns a result discriminator
 *     so callers can navigate / toast on the outcome. Does not
 *     navigate itself.
 *   - `stop()` — async. Calls `heron_end_meeting` for the current
 *     `meetingId`, then clears local state regardless of the
 *     outcome (a 4xx from the daemon usually means the FSM already
 *     terminated). Returns the daemon outcome for caller
 *     observability.
 *   - `cancel()` — clear local state without calling the daemon.
 *     Used when `start()` aborted before issuing the IPC call (the
 *     pre-flight disk gate or the consent gate's "Cancel" button).
 *   - `togglePause()` — flip the local `paused` flag. UI-only; the
 *     daemon keeps capturing.
 */

import { create } from "zustand";

import { invoke } from "../lib/invoke";
import type { MeetingId, Platform } from "../lib/types";

/**
 * Discriminated result of `start()`. `ok` carries the daemon's
 * `MeetingId` so the caller can correlate with the SSE event stream
 * and the meetings store; `unavailable` mirrors the Tauri-command
 * shape so the caller can surface a daemon-down toast without
 * parsing error strings.
 */
export type StartResult =
  | { kind: "ok"; meetingId: MeetingId }
  | { kind: "unavailable"; detail: string };

/**
 * Discriminated result of `stop()`. No `meetingId` on `ok` —
 * callers don't use it (the matching `Meeting` is in the SSE stream
 * and the meetings store), and synthesizing one for the empty-stop
 * short-circuit would have meant fabricating a fake `MeetingId`.
 */
export type StopResult =
  | { kind: "ok" }
  | { kind: "unavailable"; detail: string };

interface RecordingState {
  /** Daemon-side meeting id once the capture is running. */
  meetingId: MeetingId | null;
  /** `Date.now()` when the daemon ack'd start; `null` when idle. */
  recordingStart: number | null;
  /** UI-only pause flag — does not currently affect capture. */
  paused: boolean;
  start: () => Promise<StartResult>;
  /**
   * End the in-progress capture session.
   *
   * `fallbackId` is consulted when this store has no `meetingId` of
   * its own — for sessions started outside the renderer (CLI start,
   * a future tray/hotkey path, an app reload mid-recording where the
   * Recording page rehydrates from the meetings store + SSE).
   * Without this fallback, `stop()` would silently no-op and the
   * daemon would keep recording while the UI claimed otherwise.
   */
  stop: (fallbackId?: MeetingId) => Promise<StopResult>;
  cancel: () => void;
  togglePause: () => void;
}

/**
 * Fallback platform when `heron_detect_meeting_platform` returns
 * `null`. The orchestrator's `start_capture` requires a `Platform`
 * to pick `target_bundle_id` for the macOS process tap; with no
 * detection, `Zoom` is the safest default — it's the most common
 * heron user platform, and if Zoom isn't running the tap fails
 * gracefully and `mic.wav` still captures.
 */
const FALLBACK_PLATFORM: Platform = "zoom";

/**
 * The "no active session" projection of the store's session fields.
 * Used by `stop()` and `cancel()` (and on the empty-stop short
 * circuit) to reset back to the same idle shape the store mounts in.
 */
const IDLE_SESSION = {
  meetingId: null,
  recordingStart: null,
  paused: false,
} as const;

/**
 * Re-entrancy guard for `stop()`. Module-scoped (rather than store
 * state) because consumers don't need to subscribe to it; it just
 * suppresses the duplicate IPC + the misleading-toast path described
 * in `stop()`'s body. Plain `let` is fine — Zustand stores are
 * singleton in this app, and this guard is invariant across renders.
 */
let stopInFlight = false;

export const useRecordingStore = create<RecordingState>((set, get) => ({
  ...IDLE_SESSION,

  async start() {
    // Detect first; fall back to Zoom on null so `start_capture`
    // always has a valid `Platform`. The detect call is local-only
    // (no daemon round-trip), so the worst-case extra latency is
    // sub-millisecond.
    const platform =
      (await invoke("heron_detect_meeting_platform")) ?? FALLBACK_PLATFORM;
    const outcome = await invoke("heron_start_capture", {
      body: { platform },
    });
    if (outcome.kind === "unavailable") {
      return { kind: "unavailable", detail: outcome.detail };
    }
    set({
      meetingId: outcome.data.id,
      recordingStart: Date.now(),
      paused: false,
    });
    return { kind: "ok", meetingId: outcome.data.id };
  },

  async stop(fallbackId?: MeetingId) {
    // Re-entrancy guard: a hotkey + button double-fire (or a future
    // tray "Stop" + Recording-page click race) could otherwise issue
    // two `heron_end_meeting` calls for the same session. The second
    // hits a 4xx because the FSM already terminated, and the user
    // gets a confusing "Stop request failed" toast for what is
    // actually a successful stop. Short-circuit duplicate calls.
    if (stopInFlight) {
      return { kind: "ok" };
    }
    // Capture the id we're stopping at call entry. Two reasons:
    //   1. The daemon round-trip can take seconds (audio task drain
    //      + WAV finalize). If the user starts a *new* capture in
    //      that window, the late `set(IDLE_SESSION)` below would
    //      clobber the new session's `meetingId` / `recordingStart`.
    //      We guard the late `set` with `get().meetingId === id`
    //      so a remount-and-restart is safe.
    //   2. `fallbackId` covers sessions this renderer didn't start
    //      (CLI launch, future tray hotkey, app reload mid-capture
    //      with the Recording page rehydrating from the meetings
    //      store). Without it, those sessions would silently remain
    //      running daemon-side after a Stop click.
    const id = get().meetingId ?? fallbackId ?? null;
    if (id === null) {
      // Nothing to end daemon-side. Clear local state defensively
      // (no-op when already idle) so the caller can navigate away.
      set(IDLE_SESSION);
      return { kind: "ok" };
    }
    stopInFlight = true;
    try {
      const outcome = await invoke("heron_end_meeting", { meetingId: id });
      if (outcome.kind === "unavailable") {
        // Daemon may still be recording — keep `meetingId` set so
        // the sidebar's REC indicator stays honest and the user has
        // a chance to retry once connectivity recovers. The caller
        // surfaces the failure detail as a toast.
        return { kind: "unavailable", detail: outcome.detail };
      }
      // Only commit the idle reset if the store still points at the
      // id we just ended. A concurrent `start()` would have advanced
      // `meetingId` to a fresh value; clobbering it here would leave
      // the daemon recording with no UI handle to stop it.
      if (get().meetingId === id) {
        set(IDLE_SESSION);
      }
      return { kind: "ok" };
    } finally {
      stopInFlight = false;
    }
  },

  cancel() {
    set(IDLE_SESSION);
  },

  togglePause() {
    set((s) => ({ paused: !s.paused }));
  },
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
