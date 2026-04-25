/**
 * Crash-recovery prompt state.
 *
 * Phase 69 (PR-η). The first-run salvage prompt fires once per app
 * launch; this store remembers whether the user has already seen the
 * "N sessions need recovery" Sonner banner so React 19's StrictMode
 * double-mount and Vite's hot-reload don't pop the toast on every
 * remount.
 *
 * The promptedThisSession flag is intentionally module-scoped (rather
 * than persisted) — a future launch should re-prompt once, and a
 * crash mid-session should not silently bypass the prompt on the
 * next manual launch.
 */

import { create } from "zustand";

interface SalvagePromptState {
  /** True iff the first-run salvage banner has been shown this run. */
  promptedThisSession: boolean;
  /** Mark the banner as shown (idempotent). */
  markPrompted: () => void;
  /**
   * Reset the flag (test-only escape hatch — production callers go
   * through `markPrompted()` exactly once per app launch).
   */
  reset: () => void;
}

export const useSalvagePromptStore = create<SalvagePromptState>((set) => ({
  promptedThisSession: false,
  markPrompted: () => set({ promptedThisSession: true }),
  reset: () => set({ promptedThisSession: false }),
}));
