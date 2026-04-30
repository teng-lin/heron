/**
 * Unit tests for `dispatchBridgeStatus` — the exported handler that
 * maps `heron://bridge-status` payloads onto `useMeetingsStore`.
 *
 * Follows the same headless-Zustand pattern as `salvage.test.ts` and
 * `onboarding.test.ts`: no jsdom, no render, just `getState()` /
 * `setState()`. The Tauri IPC layer (`@tauri-apps/api/core::invoke`)
 * is never reached because we stub `load` on the store before calling
 * the handler.
 */

import { afterEach, describe, expect, test } from "bun:test";

import { dispatch, dispatchBridgeStatus } from "./useSseEvents";
import { useAudioLevelStore } from "../store/audioLevel";
import { useMeetingsStore } from "../store/meetings";
import { useSpeakerStore } from "../store/speaker";

const SAMPLE_MID = "mtg_01902a8e-7c4f-7000-8000-000000000001";

function envelope<E>(meeting_id: string | null, ev: E): E & {
  event_id: string;
  api_version: string;
  created_at: string;
  meeting_id: string | null;
} {
  return {
    event_id: "evt_test",
    api_version: "2026-04-25",
    created_at: "2026-04-29T00:00:00Z",
    meeting_id,
    ...ev,
  };
}

afterEach(() => {
  // Reset stores between tests so state doesn't leak across cases.
  useMeetingsStore.setState({
    items: [],
    nextCursor: null,
    loading: false,
    daemonDown: false,
    error: null,
    summaries: {},
  });
  useSpeakerStore.setState({ activeByMeeting: {} });
  useAudioLevelStore.setState({ latestByMeeting: {} });
});

describe("dispatchBridgeStatus", () => {
  test("down payload flips daemonDown to true immediately", () => {
    expect(useMeetingsStore.getState().daemonDown).toBe(false);

    dispatchBridgeStatus({ state: "down", reason: "auth_failed" });

    expect(useMeetingsStore.getState().daemonDown).toBe(true);
  });

  test("down payload clears items, nextCursor, summaries, loading to mirror load() failure shape", () => {
    // Pre-populate store with a non-empty state.
    useMeetingsStore.setState({
      items: [{ id: "m1" } as never],
      nextCursor: "cursor-abc",
      loading: true,
      summaries: { m1: "some summary" as never },
      daemonDown: false,
      error: null,
    });

    dispatchBridgeStatus({ state: "down", reason: "reconnect_exhausted" });

    const state = useMeetingsStore.getState();
    expect(state.daemonDown).toBe(true);
    expect(state.items).toEqual([]);
    expect(state.nextCursor).toBeNull();
    expect(state.loading).toBe(false);
    expect(state.summaries).toEqual({});
    expect(state.error).toBe("reconnect_exhausted");
  });

  test("down payload with reconnect_exhausted reason also flips daemonDown", () => {
    dispatchBridgeStatus({ state: "down", reason: "reconnect_exhausted" });
    expect(useMeetingsStore.getState().daemonDown).toBe(true);
  });

  test("up payload triggers load() to clear daemonDown via normal success path", () => {
    let loadCalled = false;
    // Stub load so it doesn't try to reach Tauri IPC and marks
    // `daemonDown: false` as the real success path would.
    useMeetingsStore.setState({
      daemonDown: true,
      load: async () => {
        loadCalled = true;
        useMeetingsStore.setState({ daemonDown: false });
      },
    });

    dispatchBridgeStatus({ state: "up", reason: "connected" });

    // `load` is async but dispatched with `void` — give the microtask
    // queue one tick to flush.
    return Promise.resolve().then(() => {
      expect(loadCalled).toBe(true);
    });
  });

  test("speaker.changed routes the data into useSpeakerStore", () => {
    expect(useSpeakerStore.getState().activeByMeeting[SAMPLE_MID]).toBeUndefined();

    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "speaker.changed",
        data: { t: 1.5, name: "Alice", started: true },
      } as const),
    );

    expect(useSpeakerStore.getState().activeByMeeting[SAMPLE_MID]).toBe(
      "Alice",
    );
  });

  test("speaker.changed without meeting_id is dropped", () => {
    // Defensive: the daemon stamps `meeting_id` per Invariant 12 but a
    // future bus replay path could in principle deliver an envelope
    // with `meeting_id: null`. The dispatcher must not key off an
    // empty meeting id.
    dispatch(
      envelope(null, {
        event_type: "speaker.changed",
        data: { t: 1.5, name: "Alice", started: true },
      } as const),
    );

    expect(useSpeakerStore.getState().activeByMeeting).toEqual({});
  });

  test("meeting.completed clears active speaker for that meeting", () => {
    useSpeakerStore.setState({ activeByMeeting: { [SAMPLE_MID]: "Alice" } });

    // Stub load so the test doesn't reach into Tauri IPC.
    useMeetingsStore.setState({
      load: async () => {},
    });

    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "meeting.completed",
        data: {
          meeting: { id: SAMPLE_MID } as never,
          outcome: "success",
          failure_reason: null,
        },
      } as const),
    );

    expect(
      useSpeakerStore.getState().activeByMeeting[SAMPLE_MID],
    ).toBeUndefined();
  });

  test("audio.level routes the data into useAudioLevelStore", () => {
    expect(
      useAudioLevelStore.getState().latestByMeeting[SAMPLE_MID],
    ).toBeUndefined();

    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "audio.level",
        data: { t: 1.5, channel: "mic_clean", peak_dbfs: -6.0, rms_dbfs: -18.0 },
      } as const),
    );

    const slot = useAudioLevelStore.getState().latestByMeeting[SAMPLE_MID];
    expect(slot?.mic_clean).toEqual({
      t: 1.5,
      channel: "mic_clean",
      peak_dbfs: -6.0,
      rms_dbfs: -18.0,
    });
    // Tap remains null — channels are independent.
    expect(slot?.tap).toBeNull();
  });

  test("audio.level latest envelope wins per channel without folding", () => {
    // The daemon-side coalescer already takes max over the tick window,
    // so the frontend is pure latest-wins — a follow-up envelope on the
    // same channel must replace the prior reading even when the new
    // peak is quieter. Pin so a future "smooth the meter on the JS side"
    // refactor can't ship a fold here without a deliberate review.
    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "audio.level",
        data: { t: 1.0, channel: "mic_clean", peak_dbfs: -3.0, rms_dbfs: -12.0 },
      } as const),
    );
    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "audio.level",
        data: { t: 1.1, channel: "mic_clean", peak_dbfs: -20.0, rms_dbfs: -40.0 },
      } as const),
    );

    expect(
      useAudioLevelStore.getState().latestByMeeting[SAMPLE_MID]?.mic_clean
        ?.peak_dbfs,
    ).toBe(-20.0);
  });

  test("audio.level without meeting_id is dropped", () => {
    // Same defensive guard as the speaker.changed counterpart — Invariant
    // 12 stamps the meeting id but a replay path could deliver null.
    dispatch(
      envelope(null, {
        event_type: "audio.level",
        data: { t: 1.5, channel: "tap", peak_dbfs: -6.0, rms_dbfs: -18.0 },
      } as const),
    );

    expect(useAudioLevelStore.getState().latestByMeeting).toEqual({});
  });

  test("meeting.completed clears audio levels for that meeting", () => {
    // Capture broadcast closes on `ended`/`completed`; without this
    // clear the meter would freeze a stale dBFS bar onto the post-
    // meeting review screen indefinitely.
    useAudioLevelStore.setState({
      latestByMeeting: {
        [SAMPLE_MID]: {
          mic_clean: { t: 1.0, channel: "mic_clean", peak_dbfs: -6, rms_dbfs: -18 },
          tap: null,
        },
      },
    });
    useMeetingsStore.setState({ load: async () => {} });

    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "meeting.completed",
        data: {
          meeting: { id: SAMPLE_MID } as never,
          outcome: "success",
          failure_reason: null,
        },
      } as const),
    );

    expect(
      useAudioLevelStore.getState().latestByMeeting[SAMPLE_MID],
    ).toBeUndefined();
  });

  test("meeting.ended also clears audio levels (capture broadcast closes)", () => {
    useAudioLevelStore.setState({
      latestByMeeting: {
        [SAMPLE_MID]: {
          mic_clean: { t: 1.0, channel: "mic_clean", peak_dbfs: -6, rms_dbfs: -18 },
          tap: null,
        },
      },
    });
    useMeetingsStore.setState({ load: async () => {} });

    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "meeting.ended",
        data: { id: SAMPLE_MID } as never,
      } as const),
    );

    expect(
      useAudioLevelStore.getState().latestByMeeting[SAMPLE_MID],
    ).toBeUndefined();
  });

  test("meeting.ended also clears active speaker (AX bridge stops mid-finalize)", () => {
    // Regression guard for the gap between `meeting.ended` (recording
    // stopped) and `meeting.completed` (transcribe + summarize done):
    // the AX bridge stops emitting on `ended`, so any unflushed
    // `started=true` would leak as a phantom badge during the
    // transcribe phase. Both events must clear the store.
    useSpeakerStore.setState({ activeByMeeting: { [SAMPLE_MID]: "Alice" } });
    useMeetingsStore.setState({ load: async () => {} });

    dispatch(
      envelope(SAMPLE_MID, {
        event_type: "meeting.ended",
        data: { id: SAMPLE_MID } as never,
      } as const),
    );

    expect(
      useSpeakerStore.getState().activeByMeeting[SAMPLE_MID],
    ).toBeUndefined();
  });

  test("down does not trigger a load()", () => {
    let loadCalled = false;
    useMeetingsStore.setState({
      load: async () => {
        loadCalled = true;
      },
    });

    dispatchBridgeStatus({ state: "down", reason: "stream_closed" });

    return Promise.resolve().then(() => {
      expect(loadCalled).toBe(false);
      // daemonDown must still be set even when load is stubbed.
      expect(useMeetingsStore.getState().daemonDown).toBe(true);
    });
  });
});
