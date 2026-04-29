/**
 * Calendar upcoming events for the Home page rail.
 *
 * Backed by `heron_list_calendar_upcoming`, which proxies the daemon's
 * `GET /v1/calendar/upcoming` (the orchestrator reads from EventKit
 * via `heron_vault::CalendarReader`). Same daemon-down handling as
 * `useMeetingsStore`: an `unavailable` outcome flips `daemonDown` and
 * empties `items` so the rail renders an empty state rather than a
 * stale week.
 *
 * TTL caching: the calendar window is stable on minute scales, and
 * EventKit reads are cheap but still cross a process boundary. A 60s
 * TTL guard means re-mounting the rail or focusing the window inside
 * a short window is a no-op. `forceRefresh: true` skips the guard.
 */

import { create } from "zustand";

import { invoke } from "../lib/invoke";
import type { CalendarEvent, CalendarPage, CalendarQuery } from "../lib/types";

/** Cache lifetime in ms before `ensureFresh()` triggers a re-fetch. */
export const CALENDAR_TTL_MS = 60_000;

interface CalendarState {
  items: CalendarEvent[];
  loading: boolean;
  daemonDown: boolean;
  error: string | null;
  /** Epoch ms of the last successful fetch. `null` until first load. */
  lastFetchedAt: number | null;
  /** Force a fetch. Coalesces overlapping calls. */
  load: (query?: CalendarQuery) => Promise<void>;
  /** Re-fetch only when the cache is older than `CALENDAR_TTL_MS`. */
  ensureFresh: (query?: CalendarQuery) => Promise<void>;
}

let inFlightLoad: Promise<void> | null = null;

export const useCalendarStore = create<CalendarState>((set, get) => ({
  items: [],
  loading: false,
  daemonDown: false,
  error: null,
  lastFetchedAt: null,
  load: async (query) => {
    if (inFlightLoad !== null) {
      return inFlightLoad;
    }
    set({ loading: true, error: null });
    inFlightLoad = (async () => {
      try {
        const result = await invoke("heron_list_calendar_upcoming", {
          query: query ?? {},
        });
        if (result.kind === "ok") {
          const page: CalendarPage = result.data;
          set({
            items: page.items,
            loading: false,
            daemonDown: false,
            error: null,
            lastFetchedAt: Date.now(),
          });
        } else {
          set({
            items: [],
            loading: false,
            daemonDown: true,
            error: result.detail,
          });
        }
      } catch (err) {
        const detail = err instanceof Error ? err.message : String(err);
        set({
          items: [],
          loading: false,
          daemonDown: true,
          error: detail,
        });
      } finally {
        inFlightLoad = null;
      }
    })();
    return inFlightLoad;
  },
  ensureFresh: async (query) => {
    const last = get().lastFetchedAt;
    if (last !== null && Date.now() - last < CALENDAR_TTL_MS) {
      return;
    }
    return get().load(query);
  },
}));
