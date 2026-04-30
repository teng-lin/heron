/**
 * Tier 3 #16 contract pin for the recording-store pause flag.
 *
 * Two assertions cover the load-bearing behaviour the
 * `<Recording />` page depends on:
 *
 *   1. `togglePause()` is a pure local flip — it must NOT call the
 *      daemon. The page-level handler in `Recording.tsx` is the
 *      single owner of the network round-trip, and only invokes
 *      `togglePause()` after a successful `204` from the daemon's
 *      pause/resume HTTP endpoint. A test that called `invoke` from
 *      inside the store would be the regression we are guarding
 *      against.
 *   2. `start(meetingId)` resets `paused` to `false` so a fresh
 *      capture never inherits a stale paused flag from the previous
 *      session (e.g. the user paused, stopped, then started a new
 *      meeting on Home — the new session must come up un-paused).
 *
 * Runs under `bun test` (see `apps/desktop/package.json`); same
 * headless-store pattern as `store/salvage.test.ts` — no jsdom or
 * @testing-library required because Zustand exposes `getState()` /
 * `setState()` directly.
 */

import { afterEach, describe, expect, test } from "bun:test";

import { useRecordingStore } from "./recording";

afterEach(() => {
  // Restore the initial-state snapshot so tests in this file don't
  // leak `paused: true` / a fake meeting id into each other. `bun
  // test` shares the module cache across tests in a file, so without
  // a manual reset the first test's writes persist into the second.
  useRecordingStore.setState({
    recordingStart: null,
    meetingId: null,
    paused: false,
  });
});

describe("recording store pause flag", () => {
  test("togglePause flips the local paused flag in place", () => {
    expect(useRecordingStore.getState().paused).toBe(false);
    useRecordingStore.getState().togglePause();
    expect(useRecordingStore.getState().paused).toBe(true);
    useRecordingStore.getState().togglePause();
    expect(useRecordingStore.getState().paused).toBe(false);
  });

  test("start clears a stale paused flag from a prior session", () => {
    // Simulate a paused session that was never explicitly resumed
    // before being torn down (the prior session ended via Stop while
    // paused). The next `start()` must come up un-paused so the new
    // meeting's Pause button reflects the un-paused daemon state.
    useRecordingStore.setState({
      recordingStart: 1_700_000_000_000,
      meetingId: "mtg_01234567-89ab-7def-8000-000000000001",
      paused: true,
    });

    useRecordingStore.getState().start("mtg_01234567-89ab-7def-8000-000000000002");
    const next = useRecordingStore.getState();
    expect(next.paused).toBe(false);
    expect(next.meetingId).toBe(
      "mtg_01234567-89ab-7def-8000-000000000002",
    );
  });

  test("stop also clears the paused flag", () => {
    useRecordingStore.setState({
      recordingStart: 1_700_000_000_000,
      meetingId: "mtg_01234567-89ab-7def-8000-000000000001",
      paused: true,
    });
    useRecordingStore.getState().stop();
    const next = useRecordingStore.getState();
    expect(next.paused).toBe(false);
    expect(next.meetingId).toBeNull();
    expect(next.recordingStart).toBeNull();
  });
});
