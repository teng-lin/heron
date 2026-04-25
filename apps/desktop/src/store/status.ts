/**
 * Latest `HeronStatus` from the Rust backend.
 *
 * The Home page reads this store rather than holding the status in
 * `useState` so the pattern (Zustand for cross-component state, hooks
 * for view-local state) is in place before β/γ/δ start adding real
 * surface area. PR-β will reuse `refresh()` from the menubar tray
 * tick; PR-γ will surface `fsm_state` to the recording controls.
 */

import { create } from "zustand";

import { invoke, type HeronStatus } from "../lib/invoke";

interface StatusState {
  /** Latest snapshot. `null` until the first refresh resolves. */
  status: HeronStatus | null;
  /** Last error message from a failed refresh, or `null` on success. */
  error: string | null;
  /** True while a refresh is in flight. */
  loading: boolean;
  /** Fetch a fresh `HeronStatus` from the backend. */
  refresh: () => Promise<void>;
}

export const useStatusStore = create<StatusState>((set) => ({
  status: null,
  error: null,
  loading: false,
  refresh: async () => {
    set({ loading: true, error: null });
    try {
      const status = await invoke("heron_status");
      set({ status, loading: false });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      set({ error: message, loading: false });
    }
  },
}));
