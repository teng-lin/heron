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

const invokeStub = mock(() => Promise.resolve({ kind: "ok", data: {} }));
mock.module("../lib/invoke", () => ({ invoke: invokeStub }));

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
      auto_record: false,
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

describe("useCalendarStore — auto-record toggle", () => {
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
      auto_record: false,
      ...overrides,
    };
  }

  beforeEach(() => {
    invokeStub.mockReset();
    invokeStub.mockImplementation(() =>
      Promise.resolve({
        kind: "ok",
        data: { calendar_event_id: "evt_target", enabled: true },
      }),
    );
  });

  test("optimistically patches the row and confirms the daemon ack", async () => {
    useCalendarStore.setState({ items: [event({ id: "evt_target" })] });

    const ok = await useCalendarStore
      .getState()
      .setEventAutoRecord("evt_target", true);

    expect(ok).toBe(true);
    expect(invokeStub).toHaveBeenCalledWith("heron_set_event_auto_record", {
      request: { calendar_event_id: "evt_target", enabled: true },
    });
    expect(useCalendarStore.getState().items[0].auto_record).toBe(true);
  });

  test("rolls the optimistic patch back when the daemon rejects", async () => {
    const evt = event({ id: "evt_target", auto_record: false });
    useCalendarStore.setState({ items: [evt] });
    invokeStub.mockImplementation(() =>
      Promise.resolve({ kind: "unavailable", detail: "daemon down" }),
    );

    const ok = await useCalendarStore
      .getState()
      .setEventAutoRecord("evt_target", true);

    expect(ok).toBe(false);
    expect(useCalendarStore.getState().items[0].auto_record).toBe(false);
    expect(useCalendarStore.getState().daemonDown).toBe(true);
    expect(useCalendarStore.getState().error).toBe("daemon down");
  });

  test("rollback preserves unrelated row changes from concurrent patches", async () => {
    const evt = event({ id: "evt_target", auto_record: false });
    useCalendarStore.setState({ items: [evt] });
    invokeStub.mockImplementation(async () => {
      useCalendarStore.setState((s) => ({
        items: s.items.map((row) =>
          row.id === "evt_target" ? { ...row, primed: true } : row,
        ),
      }));
      return { kind: "unavailable", detail: "daemon down" };
    });

    const ok = await useCalendarStore
      .getState()
      .setEventAutoRecord("evt_target", true);

    expect(ok).toBe(false);
    expect(useCalendarStore.getState().items[0].auto_record).toBe(false);
    expect(useCalendarStore.getState().items[0].primed).toBe(true);
  });

  test("stale (out-of-order) responses do not clobber the latest toggle", async () => {
    // Two concurrent toggles for the same event resolve in reverse
    // order (first call resolves *after* the second). Without the
    // per-event sequence guard, the older `false` rollback would
    // overwrite the newer `true` ack and the row would land on the
    // wrong value.
    useCalendarStore.setState({ items: [event({ id: "evt_target" })] });

    let resolveSlow!: (
      v:
        | { kind: "ok"; data: { calendar_event_id: string; enabled: boolean } }
        | { kind: "unavailable"; detail: string },
    ) => void;
    const slow = new Promise<
      | { kind: "ok"; data: { calendar_event_id: string; enabled: boolean } }
      | { kind: "unavailable"; detail: string }
    >((resolve) => {
      resolveSlow = resolve;
    });
    let call = 0;
    invokeStub.mockImplementation(() => {
      call += 1;
      if (call === 1) return slow;
      return Promise.resolve({
        kind: "ok",
        data: { calendar_event_id: "evt_target", enabled: false },
      });
    });

    const firstP = useCalendarStore
      .getState()
      .setEventAutoRecord("evt_target", true);
    const secondP = useCalendarStore
      .getState()
      .setEventAutoRecord("evt_target", false);
    // Second call (the latest user intent) lands first.
    const secondOk = await secondP;
    expect(secondOk).toBe(true);
    expect(useCalendarStore.getState().items[0].auto_record).toBe(false);
    // Now let the slower first call resolve as a daemon failure —
    // the rollback path must no-op because its seq is stale.
    resolveSlow({ kind: "unavailable", detail: "stale" });
    const firstOk = await firstP;
    expect(firstOk).toBe(false);
    expect(useCalendarStore.getState().items[0].auto_record).toBe(false);
    // And the daemon-down/error fields the stale response would have
    // set must not have leaked through either.
    expect(useCalendarStore.getState().daemonDown).toBe(false);
    expect(useCalendarStore.getState().error).toBeNull();
  });
});
