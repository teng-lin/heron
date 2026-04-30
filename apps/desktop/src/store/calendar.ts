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
 *
 * Tier 5 #25: every successful `load()` fans out
 * `heron_prepare_context` for events that arrive un-primed, so the
 * daemon stages a default `PreMeetingContext` (attendees lifted into
 * `attendees_known`) before the user clicks "Start with context". On
 * each successful prepare we patch the local `primed` flag on the
 * matching event so the rail's indicator flips without a refetch.
 *
 * Tier 5 #26: `setEventAutoRecord()` toggles the daemon's per-event
 * auto-record registry and optimistically mirrors the flag onto the
 * matching rail row.
 */

import { create } from "zustand";

import { invoke } from "../lib/invoke";
import type { CalendarEvent, CalendarPage, CalendarQuery } from "../lib/types";

/** Cache lifetime in ms before `ensureFresh()` triggers a re-fetch. */
export const CALENDAR_TTL_MS = 60_000;

export interface CalendarStoreState {
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
  /** Toggle the daemon's per-event auto-record registry. */
  setEventAutoRecord: (
    calendarEventId: string,
    enabled: boolean,
  ) => Promise<boolean>;
}

let inFlightLoad: Promise<void> | null = null;
// Per-event monotonic sequence so out-of-order
// `setEventAutoRecord` responses can't clobber the latest user
// intent. Bumped synchronously before each invoke; the response
// path no-ops when its captured seq isn't the latest one.
const autoRecordMutationSeq = new Map<string, number>();

export const useCalendarStore = create<CalendarStoreState>((set, get) => ({
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
          const items = page.items.map(normalizeCalendarEvent);
          set({
            items,
            loading: false,
            daemonDown: false,
            error: null,
            lastFetchedAt: Date.now(),
          });
          // Auto-prime out of band — never block the rail render on
          // the prepare fan-out. Each successful prepare patches the
          // matching event's `primed` flag in place; failures stay
          // silent (the next `ensureFresh` retries them anyway).
          void primeUnstagedEvents(items);
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
  setEventAutoRecord: async (calendarEventId, enabled) => {
    const seq = (autoRecordMutationSeq.get(calendarEventId) ?? 0) + 1;
    autoRecordMutationSeq.set(calendarEventId, seq);
    const isStale = () => autoRecordMutationSeq.get(calendarEventId) !== seq;

    const previous = get().items.find((evt) => evt.id === calendarEventId);
    set({
      items: get().items.map((evt) =>
        evt.id === calendarEventId ? { ...evt, auto_record: enabled } : evt,
      ),
      error: null,
    });
    try {
      const result = await invoke("heron_set_event_auto_record", {
        request: {
          calendar_event_id: calendarEventId,
          enabled,
        },
      });
      if (result.kind === "ok") {
        if (isStale()) return false;
        const ackId = result.data.calendar_event_id;
        set((s) => ({
          daemonDown: false,
          error: null,
          items: s.items.map((evt) =>
            evt.id === ackId
              ? { ...evt, auto_record: result.data.enabled }
              : evt,
          ),
        }));
        return true;
      }
      if (isStale()) return false;
      rollbackAutoRecord(calendarEventId, previous?.auto_record ?? false);
      set({ daemonDown: true, error: result.detail });
      return false;
    } catch (err) {
      if (isStale()) return false;
      const detail = err instanceof Error ? err.message : String(err);
      rollbackAutoRecord(calendarEventId, previous?.auto_record ?? false);
      set({ daemonDown: true, error: detail });
      return false;
    }
  },
}));

function rollbackAutoRecord(calendarEventId: string, previous: boolean): void {
  useCalendarStore.setState((s) => ({
    items: s.items.map((evt) =>
      evt.id === calendarEventId ? { ...evt, auto_record: previous } : evt,
    ),
  }));
}

function normalizeCalendarEvent(event: CalendarEvent): CalendarEvent {
  return {
    ...event,
    primed: event.primed ?? false,
    auto_record: event.auto_record ?? false,
  };
}

/**
 * Fire `heron_prepare_context` for each event that came back un-primed
 * and patch the store entry on success. Runs after the rail render so
 * the user never waits on the fan-out; uses `Promise.allSettled` so a
 * single failure (or daemon-down mid-fan-out) doesn't strand the rest.
 *
 * Skips events that already ended — `list_upcoming_calendar` is a
 * window query (`[from, to]`), not a future-only one, so a meeting
 * that finished five minutes ago is still in the page. There's no
 * value in priming it: the rail already filters past events out, and
 * the daemon would just queue a cap-evictable entry for nothing.
 *
 * Exported for tests; not part of the store's public API.
 */
export async function primeUnstagedEvents(
  events: CalendarEvent[],
): Promise<void> {
  const now = Date.now();
  const targets = events.filter((evt) => {
    if (evt.primed) return false;
    const end = Date.parse(evt.end);
    return Number.isFinite(end) ? end > now : true;
  });
  if (targets.length === 0) return;
  await Promise.allSettled(
    targets.map(async (evt) => {
      const result = await invoke("heron_prepare_context", {
        request: {
          calendar_event_id: evt.id,
          attendees: evt.attendees,
        },
      });
      if (result.kind !== "ok") return;
      // Optimistic patch: set `primed: true` on the matching event in
      // the store. Done per-event (not as a single bulk replace) so an
      // overlapping `load()` doesn't stomp the unrelated entries the
      // user is interacting with.
      useCalendarStore.setState((s) => ({
        items: s.items.map((e) =>
          e.id === evt.id ? { ...e, primed: true } : e,
        ),
      }));
    }),
  );
}
