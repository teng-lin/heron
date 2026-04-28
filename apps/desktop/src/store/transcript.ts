/**
 * Live transcript segments fanned out from the SSE bridge.
 *
 * Keyed by meeting ID so multiple concurrent meetings (a future
 * Pollux feature, even though we ship Clio first) don't fight over
 * one ordered list. The `useSseEvents` hook is the only writer:
 * `transcript.partial` and `transcript.final` events land here in
 * order; consumers (Recording.tsx, Review.tsx) just read.
 *
 * v1 keeps everything in memory — segments outlive a single
 * recording session because Review.tsx renders the historical
 * transcript and the SSE bridge replays from `since_event_id` after
 * a reconnect. Memory is bounded by the daemon's own retention
 * policy on disk; nothing here grows unbounded for a single user.
 */

import { create } from "zustand";

import type { MeetingId, TranscriptSegment } from "../lib/types";

// 5 000 finals covers ~4–5 hours at 3–5 s/segment with comfortable
// headroom. A real Clio meeting caps at ~2 hours; this is defence-in-depth
// so a daemon replay bug or runaway reconnect can't pin the renderer.
export const MAX_SEGMENTS_PER_MEETING = 5_000;

interface TranscriptState {
  /** segments keyed by meetingId, append-only per meeting. */
  segments: Record<MeetingId, TranscriptSegment[]>;
  /**
   * Append a partial or final segment. `is_final = false` segments
   * supersede earlier non-final segments with the same start time —
   * we use a simple "replace any prior non-final at >= start_secs"
   * rule (matches the spec: partials are revisions, finals are
   * sealed).
   */
  append: (meetingId: MeetingId, seg: TranscriptSegment) => void;
  /** Drop everything for one meeting. Called when the user navigates away. */
  reset: (meetingId: MeetingId) => void;
}

export const useTranscriptStore = create<TranscriptState>((set) => ({
  segments: {},
  append: (meetingId, seg) =>
    set((state) => {
      const prior = state.segments[meetingId] ?? [];
      let next: TranscriptSegment[];
      if (!seg.is_final) {
        // Partial: drop any prior non-finals at or beyond this
        // segment's start, then append. The daemon emits monotonic
        // partials, so this collapses successive revisions of the
        // same in-progress utterance into a single tail entry.
        const truncated = prior.filter(
          (s) => s.is_final || s.start_secs < seg.start_secs,
        );
        next = [...truncated, seg];
      } else {
        // Final: drop matching non-finals at the same start, append.
        // The next utterance's `start_secs` typically equals this
        // utterance's `end_secs` (contiguous transcripts), so the
        // upper bound is `>=`, not `>` — otherwise a partial that
        // begins exactly when this utterance ends would be deleted.
        const truncated = prior.filter(
          (s) =>
            s.is_final ||
            s.start_secs < seg.start_secs ||
            s.start_secs >= seg.end_secs,
        );
        next = [...truncated, seg];
      }

      // Evict oldest finals when the cap is exceeded. Partials are
      // never dropped — an in-flight partial has no sealed successor
      // yet, so removing it would leave the UI with a stale utterance.
      if (next.length > MAX_SEGMENTS_PER_MEETING) {
        const excess = next.length - MAX_SEGMENTS_PER_MEETING;
        let dropped = 0;
        const evicted = next.filter((s) => {
          if (s.is_final && dropped < excess) {
            dropped++;
            return false;
          }
          return true;
        });

        if (dropped === 0) {
          // Pathological: every entry is a partial — skip eviction rather
          // than corrupting in-flight utterances.
          console.warn(
            `[transcript] meetingId=${meetingId}: segment count ${next.length} exceeds cap but no finals to evict`,
          );
        } else {
          next = evicted;
        }
      }

      return { segments: { ...state.segments, [meetingId]: next } };
    }),
  reset: (meetingId) =>
    set((state) => {
      const { [meetingId]: _, ...rest } = state.segments;
      return { segments: rest };
    }),
}));
