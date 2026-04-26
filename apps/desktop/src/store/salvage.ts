/**
 * Crash-recovery prompt state.
 *
 * Phase 69 (PR-η). The first-run salvage prompt fires once per app
 * launch; this store remembers whether the user has already seen the
 * "N sessions need recovery" surface so React 19's StrictMode
 * double-mount and Vite's hot-reload don't pop it on every remount.
 *
 * Phase 75 (PR-ν). The Sonner toast was replaced by a sticky banner
 * mounted at the top of the app shell (see
 * `components/SalvageBanner.tsx`). The banner needs three bits of
 * state, all persisted in this store:
 *
 *   - `promptedThisSession` — has the scan run yet? Guards against
 *     React 19 StrictMode + Vite HMR running the scan twice on mount.
 *   - `unfinalizedCount` — how many sessions the scan turned up. The
 *     banner's "N sessions need recovery" copy reads from this; the
 *     banner only renders when the count is non-zero.
 *   - `dismissed` — has the user clicked "Dismiss" on the banner this
 *     launch? Per-app-launch only (not persisted to disk) so a future
 *     launch with the same unfinalized sessions still nags the user
 *     until they actually act on them.
 *
 * The flags are intentionally module-scoped (rather than persisted) —
 * a future launch should re-prompt once, and a crash mid-session
 * should not silently bypass the prompt on the next manual launch.
 */

import { create } from "zustand";

interface SalvagePromptState {
  /** True iff the first-run salvage scan has run this app launch. */
  promptedThisSession: boolean;
  /**
   * Number of unfinalized sessions the most recent scan turned up.
   * `0` is the steady-state (no banner); a positive value drives the
   * banner copy "N session(s) need recovery". The number can shift to
   * `0` mid-session if the user purges every row from `/salvage` —
   * the banner reads the live value, so it disappears as soon as the
   * count drops to zero.
   */
  unfinalizedCount: number;
  /**
   * True iff the user clicked "Dismiss" on the banner this launch.
   * Per-launch only; on the next app launch the flag resets and the
   * banner re-appears (assuming there are still unfinalized sessions).
   */
  dismissed: boolean;
  /**
   * Mark the scan as having run this launch and record how many
   * sessions it found. Idempotent — the flag is already `true` after
   * the first call, but the count is updated each time so a `/salvage`
   * page view that purges rows can call back in to drop the banner.
   */
  markPrompted: (count: number) => void;
  /** Adjust the unfinalized count (e.g., after a per-row purge). */
  setUnfinalizedCount: (count: number) => void;
  /** User clicked "Dismiss"; the banner hides for the rest of the run. */
  dismiss: () => void;
  /**
   * Reset every flag (test-only escape hatch — production callers
   * never need to clear the state mid-launch).
   */
  reset: () => void;
}

export const useSalvagePromptStore = create<SalvagePromptState>((set) => ({
  promptedThisSession: false,
  unfinalizedCount: 0,
  dismissed: false,
  markPrompted: (count) =>
    set({ promptedThisSession: true, unfinalizedCount: count }),
  setUnfinalizedCount: (count) => set({ unfinalizedCount: count }),
  dismiss: () => set({ dismissed: true }),
  reset: () =>
    set({
      promptedThisSession: false,
      unfinalizedCount: 0,
      dismissed: false,
    }),
}));
