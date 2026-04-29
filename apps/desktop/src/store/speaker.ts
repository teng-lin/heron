/**
 * Active-speaker state derived from `speaker.changed` envelopes on
 * the daemon SSE stream (Tier 0b #4).
 *
 * The daemon's AX observer (in `heron-zoom`) emits `SpeakerEvent`s as
 * Zoom's accessibility tree reports mute-state transitions; the
 * `heron-cli` capture pipeline forwards each one onto the canonical
 * `SessionEventBus` as a `speaker.changed` envelope. `useSseEvents`
 * pipes those envelopes into this store; UI components subscribe to
 * read who is currently flagged as potentially-speaking without
 * polling.
 *
 * **Semantic caveat (carried from `SpeakerChangedData`).** Zoom's AX
 * tree does not expose the active-speaker frame, so what we receive
 * is mute-state transitions. In 1:1 calls "the only unmuted remote"
 * is a perfect proxy for "now speaking"; in 3+ calls multiple
 * participants are often simultaneously unmuted and the most-recent
 * `started=true` is a heuristic, not truth. The store treats the
 * most-recent unmute-edge as the active speaker — a UI rendering
 * this should label the badge as "speaking" only when confident
 * (e.g. when a tap-energy aligner agrees), or as a generic
 * "potentially speaking" indicator otherwise.
 *
 * Why a store: an event stream isn't directly renderable, and each
 * mid-meeting React re-render that needs to know "who's flagged
 * right now?" should not re-iterate the SSE tail. Stashing the most-
 * recent `started=true` per meeting + clearing on the matching
 * `started=false` lets a "now speaking" badge be a 1-line `useStore`
 * subscription with no extra glue.
 *
 * Tier 0b #4 deliberately stops at the store / handler boundary —
 * the speaker badge UI is a separate PR. The store is exposed so
 * that PR can ship as a pure rendering change.
 */

import { create } from "zustand";

import type { MeetingId, SpeakerChangedData } from "../lib/types";

interface SpeakerState {
  /**
   * Per-meeting active-speaker name. `null` (or absent key) means no
   * one is currently flagged as active for that meeting — either the
   * meeting just started, or the most recent event was a `started=false`
   * for whichever speaker was active.
   *
   * Keyed by `MeetingId` because the daemon can in principle surface
   * events for multiple concurrent meetings (manual capture + auto-
   * detected ad-hoc). Today only one is active at a time, but keying
   * defensively keeps the store accurate if that invariant relaxes.
   */
  activeByMeeting: Record<string, string | null>;
  /**
   * Apply a `speaker.changed` envelope to the store. Idempotent: a
   * `started=false` for a name that isn't the current active speaker
   * is a no-op (the AX bridge can re-emit "still silent" lines as a
   * heartbeat). A `started=true` always wins — the most recent
   * speaker is the one we render.
   */
  apply: (meetingId: MeetingId, event: SpeakerChangedData) => void;
  /** Drop all per-meeting state. Used on `meeting.completed`. */
  clear: (meetingId: MeetingId) => void;
}

export const useSpeakerStore = create<SpeakerState>((set) => ({
  activeByMeeting: {},
  apply: (meetingId, event) =>
    set((state) => {
      const current = state.activeByMeeting[meetingId] ?? null;
      if (event.started) {
        if (current === event.name) return state;
        return {
          activeByMeeting: {
            ...state.activeByMeeting,
            [meetingId]: event.name,
          },
        };
      }
      // started=false: only clear if this is the currently-active speaker.
      // A `started=false` for someone who isn't current means the AX
      // bridge fired the off-edge for a participant we never saw the
      // on-edge for — silently ignore.
      if (current !== event.name) return state;
      return {
        activeByMeeting: {
          ...state.activeByMeeting,
          [meetingId]: null,
        },
      };
    }),
  clear: (meetingId) =>
    set((state) => {
      if (!(meetingId in state.activeByMeeting)) return state;
      const { [meetingId]: _, ...rest } = state.activeByMeeting;
      return { activeByMeeting: rest };
    }),
}));
