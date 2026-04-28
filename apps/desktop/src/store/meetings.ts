/**
 * Meetings list state for `/home` (and the chrome's sidebar counts).
 *
 * Backed by `heron_list_meetings`, which proxies the daemon's
 * `GET /v1/meetings` through Rust (browser-level fetch can't, see
 * `apps/desktop/src-tauri/src/meetings.rs`). The store also caches
 * per-meeting summaries fetched lazily on row hover.
 *
 * Daemon-down handling: when `heron_list_meetings` returns
 * `{ kind: "unavailable" }`, the store flips `daemonDown = true` and
 * leaves `items` empty. The Home page renders the daemon-down banner
 * + a Retry button that calls `load()` again. Settings/Salvage routes
 * keep working because they don't read this store.
 */

import { create } from "zustand";

import { invoke } from "../lib/invoke";
import type {
  ListMeetingsPage,
  ListMeetingsQuery,
  Meeting,
  MeetingId,
  Summary,
} from "../lib/types";

interface MeetingsState {
  /** Latest list-meetings result. Empty on first load and when the daemon is down. */
  items: Meeting[];
  /** Cursor for the next page; `null` when on the last page. */
  nextCursor: string | null;
  /** True while a list/refresh request is in flight. */
  loading: boolean;
  /** True iff the last call returned `unavailable`. */
  daemonDown: boolean;
  /** Last failure detail (for diagnostic logs / dev banner). */
  error: string | null;
  /**
   * Per-meeting summary cache keyed by `MeetingId`. `null` while a
   * summary is in flight; `string` once it lands. Drives the lazy
   * row-hover preview without needing to refetch on every hover.
   */
  summaries: Record<MeetingId, Summary | null | "unavailable">;
  /** Re-fetch the meetings list. Idempotent and cheap to spam. */
  load: (query?: ListMeetingsQuery) => Promise<void>;
  /**
   * Fetch the summary for a single meeting on demand. No-op when a
   * cached entry already exists (the store doesn't currently
   * invalidate — `summary.ready` SSE events in PR 4 will).
   */
  fetchSummary: (id: MeetingId) => Promise<void>;
}

/**
 * In-flight load promise, held outside the Zustand state to avoid
 * forcing a re-render on every subscriber when it changes. SSE event
 * bursts (e.g. a meeting transitioning Detected → Armed → Started in
 * quick succession) used to issue overlapping `heron_list_meetings`
 * calls; an older failure could resolve after a newer success and
 * flip `daemonDown` back to `true`. Coalescing here makes the store
 * monotonic with respect to wall-clock order: the freshest call wins.
 */
let inFlightLoad: Promise<void> | null = null;

export const useMeetingsStore = create<MeetingsState>((set, get) => ({
  items: [],
  nextCursor: null,
  loading: false,
  daemonDown: false,
  error: null,
  summaries: {},
  load: async (query) => {
    if (inFlightLoad !== null) {
      return inFlightLoad;
    }
    set({ loading: true, error: null });
    inFlightLoad = (async () => {
      try {
        const result = await invoke("heron_list_meetings", {
          query: query ?? {},
        });
        if (result.kind === "ok") {
          const page: ListMeetingsPage = result.data;
          set({
            items: page.items,
            nextCursor: page.next_cursor,
            loading: false,
            daemonDown: false,
            error: null,
          });
        } else {
          set({
            items: [],
            nextCursor: null,
            loading: false,
            daemonDown: true,
            error: result.detail,
          });
        }
      } catch (err) {
        const detail = err instanceof Error ? err.message : String(err);
        set({
          items: [],
          nextCursor: null,
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
  fetchSummary: async (id) => {
    const cached = get().summaries[id];
    // `undefined` → never fetched; `null` → fetch in flight; a Summary
    // object → cached. We DO retry on `"unavailable"` (the previous
    // attempt failed) so a transient daemon hiccup doesn't permanently
    // poison the row's preview.
    if (cached === null || (cached !== undefined && cached !== "unavailable")) {
      return;
    }
    set((state) => ({ summaries: { ...state.summaries, [id]: null } }));
    try {
      const result = await invoke("heron_meeting_summary", { meetingId: id });
      set((state) => ({
        summaries: {
          ...state.summaries,
          [id]: result.kind === "ok" ? result.data : "unavailable",
        },
      }));
    } catch {
      set((state) => ({
        summaries: { ...state.summaries, [id]: "unavailable" },
      }));
    }
  },
}));
