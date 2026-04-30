/**
 * Unit tests for the calendar store's TTL guard and the
 * Tier 5 #25 auto-prime fan-out.
 *
 * Same pattern as `transcript.test.ts` / `salvage.test.ts` — exercise
 * the Zustand store via `getState()` and `setState()` without mocking
 * the underlying `invoke` (the Tauri proxy tests in
 * `apps/desktop/src-tauri/src/meetings.rs` cover the daemon round-trip
 * end-to-end). The auto-prime fan-out is exercised directly via the
 * exported `primeUnstagedEvents` helper with an `invoke` stub installed
 * via `mock.module` — the daemon round-trip itself is again covered
 * by the Tauri proxy tests.
 */

import {
  afterEach,
  beforeEach,
  describe,
  expect,
  mock,
  test,
} from "bun:test";

import type { AttendeeContext, CalendarEvent } from "../lib/types";

import {
  CALENDAR_TTL_MS,
  primeUnstagedEvents,
  useCalendarStore,
  type CalendarStoreState,
} from "./calendar";

// Capture the real `load` action once per test so the afterEach reset
// can restore it even if the test threw before reaching its own
// restore line. Without this, a failing test that swapped in a mock
// `load` would leak that mock into every subsequent test.
let originalLoad: CalendarStoreState["load"];

beforeEach(() => {
  originalLoad = useCalendarStore.getState().load;
});

afterEach(() => {
  useCalendarStore.setState({
    items: [],
    loading: false,
    daemonDown: false,
    error: null,
    lastFetchedAt: null,
    load: originalLoad,
  });
});

describe("useCalendarStore — TTL guard", () => {
  test("ensureFresh skips when cache is younger than TTL", async () => {
    let loadCalls = 0;
    useCalendarStore.setState({
      lastFetchedAt: Date.now() - (CALENDAR_TTL_MS - 5_000),
      load: async () => {
        loadCalls += 1;
      },
    });

    await useCalendarStore.getState().ensureFresh();

    expect(loadCalls).toBe(0);
  });

  test("ensureFresh falls through when cache is older than TTL", async () => {
    let loadCalls = 0;
    useCalendarStore.setState({
      lastFetchedAt: Date.now() - (CALENDAR_TTL_MS + 1_000),
      load: async () => {
        loadCalls += 1;
      },
    });

    await useCalendarStore.getState().ensureFresh();

    expect(loadCalls).toBe(1);
  });

  test("ensureFresh falls through when never fetched", async () => {
    let loadCalls = 0;
    useCalendarStore.setState({
      lastFetchedAt: null,
      load: async () => {
        loadCalls += 1;
      },
    });

    await useCalendarStore.getState().ensureFresh();

    expect(loadCalls).toBe(1);
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

describe("primeUnstagedEvents — auto-prime fan-out", () => {
  // Stub the Tauri `invoke` boundary so the helper's per-event call
  // is observable without a real daemon. Each test installs its own
  // implementation via `invokeStub.mockImplementation`; `mock.module`
  // is hoisted at module load so the helper picks up the stub on its
  // dynamic-import-free `import { invoke } from "../lib/invoke"`.
  const invokeStub = mock(() => Promise.resolve({ kind: "ok", data: {} }));
  mock.module("../lib/invoke", () => ({ invoke: invokeStub }));

  function attendee(name: string): AttendeeContext {
    return {
      name,
      email: null,
      last_seen_in: null,
      relationship: null,
      notes: null,
    };
  }

  function event(overrides: Partial<CalendarEvent>): CalendarEvent {
    return {
      id: "evt_default",
      title: "Default",
      start: "2026-04-29T15:00:00Z",
      end: "2026-04-29T15:30:00Z",
      attendees: [],
      meeting_url: null,
      related_meetings: [],
      primed: false,
      ...overrides,
    };
  }

  beforeEach(() => {
    invokeStub.mockReset();
    invokeStub.mockImplementation(() =>
      Promise.resolve({ kind: "ok", data: {} }),
    );
  });

  test("skips events that are already primed and events that already ended", async () => {
    // Past-end events are returned by `list_upcoming_calendar`'s
    // window query but the rail filters them out — the fan-out must
    // skip them too so the daemon's `pending_contexts` doesn't fill
    // up with cap-evictable orphans.
    const past = new Date(Date.now() - 60_000).toISOString();
    const future = new Date(Date.now() + 60_000).toISOString();
    const events = [
      event({ id: "evt_already_primed", end: future, primed: true }),
      event({ id: "evt_past", end: past, primed: false }),
      event({ id: "evt_target", end: future, primed: false }),
    ];

    await primeUnstagedEvents(events);

    expect(invokeStub).toHaveBeenCalledTimes(1);
    const [name, args] = invokeStub.mock.calls[0] as unknown as [
      string,
      { request: { calendar_event_id: string } },
    ];
    expect(name).toBe("heron_prepare_context");
    expect(args.request.calendar_event_id).toBe("evt_target");
  });

  test("optimistically patches store entries for events the daemon accepts", async () => {
    const future = new Date(Date.now() + 60_000).toISOString();
    const evt = event({
      id: "evt_target",
      end: future,
      attendees: [attendee("Alex")],
    });
    useCalendarStore.setState({ items: [evt] });

    await primeUnstagedEvents([evt]);

    const items = useCalendarStore.getState().items;
    expect(items).toHaveLength(1);
    expect(items[0].id).toBe("evt_target");
    expect(items[0].primed).toBe(true);
  });

  test("leaves the store untouched when the daemon returns unavailable", async () => {
    const future = new Date(Date.now() + 60_000).toISOString();
    const evt = event({ id: "evt_target", end: future });
    useCalendarStore.setState({ items: [evt] });
    invokeStub.mockImplementation(() =>
      Promise.resolve({ kind: "unavailable", detail: "daemon down" }),
    );

    await primeUnstagedEvents([evt]);

    expect(useCalendarStore.getState().items[0].primed).toBe(false);
  });
});
