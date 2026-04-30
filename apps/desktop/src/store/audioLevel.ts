/**
 * Live capture-loop dBFS state derived from `audio.level` envelopes
 * (Tier 3 #15).
 *
 * Last-value-sticks: a tick window with no frames publishes nothing
 * for that channel, so don't reset a slot back to `null` just because
 * a tick passed without an envelope — absence means "no new
 * information," not "silence."
 *
 * TODO(perf): once the meter UI lands, evaluate whether to short-
 * circuit `apply()` when peak/rms are unchanged from the prior
 * reading. Today every envelope reallocates `latestByMeeting`
 * (~20×/sec while recording); subscribers will re-render even on
 * identical values until then.
 */

import { create } from "zustand";

import type { AudioLevelData, MeetingId } from "../lib/types";

interface PerChannelLevels {
  mic_clean: AudioLevelData | null;
  tap: AudioLevelData | null;
}

const EMPTY_LEVELS: PerChannelLevels = { mic_clean: null, tap: null };

interface AudioLevelState {
  /**
   * Per-meeting, per-channel latest dBFS reading. Keyed by
   * `MeetingId` so concurrent meetings (manual capture + auto-detected
   * ad-hoc) don't bleed into each other; today only one is active at
   * a time but keying defensively keeps the store accurate if that
   * invariant relaxes.
   */
  latestByMeeting: Record<string, PerChannelLevels>;
  /**
   * Apply an `audio.level` envelope. Pure latest-wins per channel —
   * the daemon already coalesces max-peak / max-rms within the tick
   * window, so the frontend doesn't fold further.
   */
  apply: (meetingId: MeetingId, event: AudioLevelData) => void;
  /** Drop all per-meeting state. Used on `meeting.completed`. */
  clear: (meetingId: MeetingId) => void;
}

export const useAudioLevelStore = create<AudioLevelState>((set) => ({
  latestByMeeting: {},
  apply: (meetingId, event) =>
    set((state) => {
      const current = state.latestByMeeting[meetingId] ?? EMPTY_LEVELS;
      const next: PerChannelLevels = {
        ...current,
        [event.channel]: event,
      };
      return {
        latestByMeeting: {
          ...state.latestByMeeting,
          [meetingId]: next,
        },
      };
    }),
  clear: (meetingId) =>
    set((state) => {
      if (!(meetingId in state.latestByMeeting)) return state;
      const { [meetingId]: _, ...rest } = state.latestByMeeting;
      return { latestByMeeting: rest };
    }),
}));
