/**
 * Unit tests for the live-audio-level store (Tier 3 #15).
 *
 * Pin the apply/clear semantics independently of the SSE bridge so a
 * future refactor of `useSseEvents.dispatch` can't silently break
 * the per-channel last-value-wins contract the Recording page's
 * dBFS meter / waveform / equaliser depend on.
 *
 * Same headless-store pattern as `speaker.test.ts` — Zustand exposes
 * `getState()` / `setState()` directly so no jsdom or React testing
 * library is required.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";

import { useAudioLevelStore } from "./audioLevel";

const MID = "mtg_01902a8e-7c4f-7000-8000-000000000001";
const OTHER = "mtg_01902a8e-7c4f-7000-8000-000000000002";

beforeEach(() => {
  useAudioLevelStore.setState({ latestByMeeting: {} });
});

afterEach(() => {
  useAudioLevelStore.setState({ latestByMeeting: {} });
});

describe("useAudioLevelStore.apply", () => {
  test("first envelope per channel populates that slot, leaves the other null", () => {
    useAudioLevelStore.getState().apply(MID, {
      t: 1,
      channel: "mic_clean",
      peak_dbfs: -12,
      rms_dbfs: -18,
    });
    const state = useAudioLevelStore.getState().latestByMeeting[MID];
    expect(state?.mic_clean).toEqual({
      t: 1,
      channel: "mic_clean",
      peak_dbfs: -12,
      rms_dbfs: -18,
    });
    expect(state?.tap).toBeNull();
  });

  test("tap and mic_clean envelopes coexist on the same meeting", () => {
    useAudioLevelStore
      .getState()
      .apply(MID, { t: 1, channel: "mic_clean", peak_dbfs: -10, rms_dbfs: -15 });
    useAudioLevelStore
      .getState()
      .apply(MID, { t: 1, channel: "tap", peak_dbfs: -30, rms_dbfs: -40 });
    const state = useAudioLevelStore.getState().latestByMeeting[MID];
    expect(state?.mic_clean?.peak_dbfs).toBe(-10);
    expect(state?.tap?.peak_dbfs).toBe(-30);
  });

  test("a later envelope on the same channel replaces the prior reading", () => {
    useAudioLevelStore
      .getState()
      .apply(MID, { t: 1, channel: "mic_clean", peak_dbfs: -50, rms_dbfs: -55 });
    useAudioLevelStore
      .getState()
      .apply(MID, { t: 2, channel: "mic_clean", peak_dbfs: -8, rms_dbfs: -12 });
    const state = useAudioLevelStore.getState().latestByMeeting[MID];
    expect(state?.mic_clean?.t).toBe(2);
    expect(state?.mic_clean?.peak_dbfs).toBe(-8);
  });

  test("apply is per-meeting, not global", () => {
    useAudioLevelStore
      .getState()
      .apply(MID, { t: 1, channel: "mic_clean", peak_dbfs: -10, rms_dbfs: -15 });
    useAudioLevelStore
      .getState()
      .apply(OTHER, { t: 1, channel: "tap", peak_dbfs: -30, rms_dbfs: -40 });
    const all = useAudioLevelStore.getState().latestByMeeting;
    expect(all[MID]?.mic_clean?.peak_dbfs).toBe(-10);
    expect(all[MID]?.tap).toBeNull();
    expect(all[OTHER]?.tap?.peak_dbfs).toBe(-30);
    expect(all[OTHER]?.mic_clean).toBeNull();
  });
});

describe("useAudioLevelStore.clear", () => {
  test("drops only the named meeting", () => {
    useAudioLevelStore
      .getState()
      .apply(MID, { t: 1, channel: "mic_clean", peak_dbfs: -10, rms_dbfs: -15 });
    useAudioLevelStore
      .getState()
      .apply(OTHER, { t: 1, channel: "tap", peak_dbfs: -30, rms_dbfs: -40 });
    useAudioLevelStore.getState().clear(MID);
    const all = useAudioLevelStore.getState().latestByMeeting;
    expect(all[MID]).toBeUndefined();
    expect(all[OTHER]?.tap?.peak_dbfs).toBe(-30);
  });

  test("clear on an unknown meeting is a no-op", () => {
    const before = useAudioLevelStore.getState().latestByMeeting;
    useAudioLevelStore.getState().clear("mtg_unknown");
    expect(useAudioLevelStore.getState().latestByMeeting).toBe(before);
  });
});
