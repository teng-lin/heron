/**
 * Consent-gate state.
 *
 * Phase 64 (PR-β) introduces a "did you tell the room?" modal that
 * gates the start of every recording. Three terminal dispositions:
 *
 *   - `confirmed` — user clicked "Yes, go" → caller proceeds to
 *     `/recording`.
 *   - `snoozed`   — user clicked "Remind me in 30s" → modal
 *     auto-reopens once `snoozeUntil` elapses; the original
 *     `requestConsent()` promise stays pending across the snooze.
 *   - `cancelled` — user clicked "Cancel" → caller aborts.
 *
 * `pending` is the "modal currently visible, waiting on input" state.
 * `idle` is "no consent flow active".
 *
 * The store exposes a single async entry point: `requestConsent()`.
 * It opens the modal and resolves with the terminal disposition once
 * the user picks. Concurrent callers share the same promise — there's
 * only ever one consent flow on screen at a time.
 *
 * The 30 s snooze interval is hard-coded (`SNOOZE_MS`); the brief
 * names "30s" explicitly, and exposing it as a setting is a future
 * concern (PR-δ owns the settings surface).
 */

import { create } from "zustand";

export type ConsentDisposition =
  | "idle"
  | "pending"
  | "confirmed"
  | "snoozed"
  | "cancelled";

/** 30 seconds, matching the brief's "Remind me in 30s" copy. */
export const SNOOZE_MS = 30_000;

interface ConsentState {
  /** Current state of the consent flow. */
  disposition: ConsentDisposition;
  /**
   * If `disposition === "snoozed"`, the wall-clock timestamp (ms since
   * epoch) at which the modal should auto-reopen. `undefined` in every
   * other state.
   */
  snoozeUntil?: number;
  /**
   * True iff the modal should be visible. Tracks `disposition` mostly
   * 1:1 (`pending`), with the addition of the "snooze elapsed → re-
   * prompt" flip handled inside the snooze timer.
   */
  open: boolean;

  /**
   * Open the modal and resolve with the terminal disposition.
   *
   * Concurrent calls share a single in-flight promise. The promise
   * stays pending across snoozes (the user effectively says "ask me
   * again later"; the caller waits).
   */
  requestConsent: () => Promise<"confirmed" | "cancelled">;

  /** Yes, go. Resolves the in-flight promise with `confirmed`. */
  confirm: () => void;

  /** Remind me in 30s. Schedules the auto-reopen and stays pending. */
  snooze: () => void;

  /** Cancel. Resolves the in-flight promise with `cancelled`. */
  cancel: () => void;

  /**
   * Force the modal closed without resolving (test escape hatch). Not
   * exposed to the UI — the buttons go through confirm/snooze/cancel.
   */
  reset: () => void;
}

/**
 * Module-scoped resolver shared across `requestConsent()` calls. We
 * keep it outside the Zustand state because functions don't survive
 * `JSON.stringify` and we never want this leaking into devtools state.
 */
let pendingResolver: ((value: "confirmed" | "cancelled") => void) | null =
  null;
let pendingPromise: Promise<"confirmed" | "cancelled"> | null = null;
let snoozeTimer: ReturnType<typeof setTimeout> | null = null;

function clearSnoozeTimer() {
  if (snoozeTimer !== null) {
    clearTimeout(snoozeTimer);
    snoozeTimer = null;
  }
}

export const useConsentStore = create<ConsentState>((set, get) => ({
  disposition: "idle",
  open: false,
  snoozeUntil: undefined,

  requestConsent: () => {
    if (pendingPromise) {
      return pendingPromise;
    }
    pendingPromise = new Promise<"confirmed" | "cancelled">((resolve) => {
      pendingResolver = resolve;
    });
    set({ disposition: "pending", open: true, snoozeUntil: undefined });
    return pendingPromise;
  },

  confirm: () => {
    clearSnoozeTimer();
    set({ disposition: "confirmed", open: false, snoozeUntil: undefined });
    const resolver = pendingResolver;
    pendingResolver = null;
    pendingPromise = null;
    resolver?.("confirmed");
  },

  snooze: () => {
    clearSnoozeTimer();
    const snoozeUntil = Date.now() + SNOOZE_MS;
    set({ disposition: "snoozed", open: false, snoozeUntil });
    snoozeTimer = setTimeout(() => {
      snoozeTimer = null;
      // Only re-open if the flow is still snoozed (the user might
      // have cancelled in the meantime, or another caller may have
      // already settled the promise).
      if (get().disposition === "snoozed" && pendingResolver) {
        set({ disposition: "pending", open: true, snoozeUntil: undefined });
      }
    }, SNOOZE_MS);
  },

  cancel: () => {
    clearSnoozeTimer();
    set({ disposition: "cancelled", open: false, snoozeUntil: undefined });
    const resolver = pendingResolver;
    pendingResolver = null;
    pendingPromise = null;
    resolver?.("cancelled");
  },

  reset: () => {
    clearSnoozeTimer();
    pendingResolver = null;
    pendingPromise = null;
    set({ disposition: "idle", open: false, snoozeUntil: undefined });
  },
}));
