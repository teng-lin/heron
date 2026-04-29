/**
 * Unit tests for the active-speaker store (Tier 0b #4).
 *
 * Pin the apply/clear semantics independently of the SSE bridge so a
 * future refactor of `useSseEvents.dispatch` can't silently break
 * idempotence on AX heartbeat re-emits.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";

import { useSpeakerStore } from "./speaker";

const MID = "mtg_01902a8e-7c4f-7000-8000-000000000001";
const OTHER = "mtg_01902a8e-7c4f-7000-8000-000000000002";

beforeEach(() => {
  useSpeakerStore.setState({ activeByMeeting: {} });
});

afterEach(() => {
  useSpeakerStore.setState({ activeByMeeting: {} });
});

describe("useSpeakerStore.apply", () => {
  test("started=true sets the active speaker for the meeting", () => {
    useSpeakerStore
      .getState()
      .apply(MID, { t: 1, name: "Alice", started: true });
    expect(useSpeakerStore.getState().activeByMeeting[MID]).toBe("Alice");
  });

  test("started=true overrides a different active speaker", () => {
    useSpeakerStore
      .getState()
      .apply(MID, { t: 1, name: "Alice", started: true });
    useSpeakerStore
      .getState()
      .apply(MID, { t: 2, name: "Bob", started: true });
    expect(useSpeakerStore.getState().activeByMeeting[MID]).toBe("Bob");
  });

  test("started=false for the active speaker clears them", () => {
    useSpeakerStore
      .getState()
      .apply(MID, { t: 1, name: "Alice", started: true });
    useSpeakerStore
      .getState()
      .apply(MID, { t: 2, name: "Alice", started: false });
    expect(useSpeakerStore.getState().activeByMeeting[MID]).toBeNull();
  });

  test("started=false for a non-active speaker is a no-op", () => {
    useSpeakerStore
      .getState()
      .apply(MID, { t: 1, name: "Alice", started: true });
    useSpeakerStore
      .getState()
      .apply(MID, { t: 2, name: "Bob", started: false });
    expect(useSpeakerStore.getState().activeByMeeting[MID]).toBe("Alice");
  });

  test("repeated started=true for the same speaker is idempotent", () => {
    // The AX bridge can re-emit the same on-edge; the store must not
    // churn React subscribers on a redundant write. We don't assert
    // on object identity here — Zustand short-circuits via the
    // strict-equal check we built into apply().
    useSpeakerStore
      .getState()
      .apply(MID, { t: 1, name: "Alice", started: true });
    const before = useSpeakerStore.getState().activeByMeeting;
    useSpeakerStore
      .getState()
      .apply(MID, { t: 2, name: "Alice", started: true });
    expect(useSpeakerStore.getState().activeByMeeting).toBe(before);
  });

  test("apply is per-meeting, not global", () => {
    useSpeakerStore
      .getState()
      .apply(MID, { t: 1, name: "Alice", started: true });
    useSpeakerStore
      .getState()
      .apply(OTHER, { t: 1, name: "Bob", started: true });
    const state = useSpeakerStore.getState().activeByMeeting;
    expect(state[MID]).toBe("Alice");
    expect(state[OTHER]).toBe("Bob");
  });
});

describe("useSpeakerStore.clear", () => {
  test("drops only the named meeting", () => {
    useSpeakerStore
      .getState()
      .apply(MID, { t: 1, name: "Alice", started: true });
    useSpeakerStore
      .getState()
      .apply(OTHER, { t: 1, name: "Bob", started: true });
    useSpeakerStore.getState().clear(MID);
    const state = useSpeakerStore.getState().activeByMeeting;
    expect(state[MID]).toBeUndefined();
    expect(state[OTHER]).toBe("Bob");
  });

  test("clear on an unknown meeting is a no-op", () => {
    const before = useSpeakerStore.getState().activeByMeeting;
    useSpeakerStore.getState().clear("mtg_unknown");
    expect(useSpeakerStore.getState().activeByMeeting).toBe(before);
  });
});
