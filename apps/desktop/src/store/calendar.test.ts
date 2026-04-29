/**
 * Unit tests for the calendar store's TTL guard.
 *
 * Same pattern as `transcript.test.ts` / `salvage.test.ts` — exercise
 * the Zustand store via `getState()` and `setState()` without mocking
 * the underlying `invoke` (the Tauri proxy tests in
 * `apps/desktop/src-tauri/src/meetings.rs` cover the daemon round-trip
 * end-to-end). The novel behaviour worth pinning down at this layer is
 * the freshness check: `ensureFresh` must short-circuit when a recent
 * fetch landed, and must fall through to `load()` when the cache is
 * stale or untouched.
 */

import { afterEach, describe, expect, test } from "bun:test";

import { CALENDAR_TTL_MS, useCalendarStore } from "./calendar";

afterEach(() => {
  useCalendarStore.setState({
    items: [],
    loading: false,
    daemonDown: false,
    error: null,
    lastFetchedAt: null,
  });
});

describe("useCalendarStore — TTL guard", () => {
  test("ensureFresh skips when cache is younger than TTL", async () => {
    let loadCalls = 0;
    const originalLoad = useCalendarStore.getState().load;
    useCalendarStore.setState({
      lastFetchedAt: Date.now() - (CALENDAR_TTL_MS - 5_000),
      load: async () => {
        loadCalls += 1;
      },
    });

    await useCalendarStore.getState().ensureFresh();

    expect(loadCalls).toBe(0);

    // Restore so the afterEach reset doesn't leak the spy.
    useCalendarStore.setState({ load: originalLoad });
  });

  test("ensureFresh falls through when cache is older than TTL", async () => {
    let loadCalls = 0;
    const originalLoad = useCalendarStore.getState().load;
    useCalendarStore.setState({
      lastFetchedAt: Date.now() - (CALENDAR_TTL_MS + 1_000),
      load: async () => {
        loadCalls += 1;
      },
    });

    await useCalendarStore.getState().ensureFresh();

    expect(loadCalls).toBe(1);
    useCalendarStore.setState({ load: originalLoad });
  });

  test("ensureFresh falls through when never fetched", async () => {
    let loadCalls = 0;
    const originalLoad = useCalendarStore.getState().load;
    useCalendarStore.setState({
      lastFetchedAt: null,
      load: async () => {
        loadCalls += 1;
      },
    });

    await useCalendarStore.getState().ensureFresh();

    expect(loadCalls).toBe(1);
    useCalendarStore.setState({ load: originalLoad });
  });
});

describe("useCalendarStore — initial shape", () => {
  test("starts empty and not-loaded", () => {
    const s = useCalendarStore.getState();
    expect(s.items).toEqual([]);
    expect(s.loading).toBe(false);
    expect(s.daemonDown).toBe(false);
    expect(s.error).toBeNull();
    expect(s.lastFetchedAt).toBeNull();
  });
});
