/**
 * Unit tests for the transcript store's eviction and segment-replacement
 * logic.
 *
 * Runs headlessly under `bun test` — no jsdom required. The Zustand store
 * is exercised via `getState()` directly, the same pattern established by
 * `salvage.test.ts` and `onboarding.test.ts`.
 *
 * Coverage:
 *   1. Below cap — existing partial/final replacement invariants hold.
 *   2. Above cap — oldest finals are evicted; list length returns to cap.
 *   3. Pending partial at the tail is preserved when finals are evicted.
 *   4. `reset(meetingId)` wipes everything for that meeting.
 */

import { afterEach, describe, expect, test } from "bun:test";

import type { TranscriptSegment } from "../lib/types";
import { MAX_SEGMENTS_PER_MEETING, useTranscriptStore } from "./transcript";

const MID = "meeting-test-001" as const;

// Internal cap exported from transcript.ts; we use it here to keep
// the test in sync with the implementation.
const CAP = MAX_SEGMENTS_PER_MEETING;

afterEach(() => {
  useTranscriptStore.getState().reset(MID);
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function seg(
  start: number,
  isFinal: boolean,
  text = "x",
): TranscriptSegment {
  return {
    speaker: { id: "spk1", display_name: "Alice" },
    text,
    start_secs: start,
    end_secs: start + 3,
    confidence: "high",
    is_final: isFinal,
  };
}

function segments(): TranscriptSegment[] {
  return useTranscriptStore.getState().segments[MID] ?? [];
}

function appendMany(count: number, startOffset = 0): void {
  const { append } = useTranscriptStore.getState();
  for (let i = 0; i < count; i++) {
    append(MID, seg(startOffset + i * 3, true));
  }
}

// ---------------------------------------------------------------------------
// 1. Below cap — existing replacement logic is intact
// ---------------------------------------------------------------------------

describe("below cap — partial/final replacement invariants", () => {
  test("partial collapses earlier non-finals at the same start", () => {
    const { append } = useTranscriptStore.getState();
    append(MID, seg(0, false, "draft 1"));
    append(MID, seg(0, false, "draft 2"));
    const segs = segments();
    expect(segs).toHaveLength(1);
    expect(segs[0].text).toBe("draft 2");
  });

  test("partial collapses all non-finals at or beyond its start_secs", () => {
    const { append } = useTranscriptStore.getState();
    append(MID, seg(0, false, "partial-0"));
    append(MID, seg(3, false, "partial-3"));
    // Partial at t=0 should drop both prior partials (>= 0)
    append(MID, seg(0, false, "revised-0"));
    const segs = segments();
    // Only the revised partial at 0 survives (the t=3 partial is dropped
    // because 3 >= 0 and it is non-final).
    expect(segs).toHaveLength(1);
    expect(segs[0].start_secs).toBe(0);
    expect(segs[0].text).toBe("revised-0");
  });

  test("final seals a partial for the same utterance", () => {
    const { append } = useTranscriptStore.getState();
    append(MID, seg(0, false, "draft"));
    append(MID, seg(0, true, "sealed"));
    const segs = segments();
    expect(segs).toHaveLength(1);
    expect(segs[0].is_final).toBe(true);
    expect(segs[0].text).toBe("sealed");
  });

  test("interleaved partials and finals accumulate correctly", () => {
    const { append } = useTranscriptStore.getState();
    append(MID, seg(0, false, "p0"));
    append(MID, seg(0, true, "f0"));
    append(MID, seg(3, false, "p3"));
    append(MID, seg(3, true, "f3"));
    const segs = segments();
    expect(segs).toHaveLength(2);
    expect(segs.every((s) => s.is_final)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 2. Above cap — oldest finals are evicted, list length returns to cap
// ---------------------------------------------------------------------------

describe("above cap — oldest-final eviction", () => {
  test("appending one segment past cap drops the oldest final", () => {
    // Fill to exactly CAP finals.
    appendMany(CAP);
    expect(segments()).toHaveLength(CAP);
    // One more final should trigger eviction back to CAP.
    useTranscriptStore.getState().append(MID, seg(CAP * 3, true));
    expect(segments()).toHaveLength(CAP);
  });

  test("eviction removes the oldest (lowest start_secs) finals first", () => {
    appendMany(CAP); // finals at t=0, 3, 6, …
    const extraStart = CAP * 3;
    useTranscriptStore.getState().append(MID, seg(extraStart, true, "new"));
    const segs = segments();
    // The oldest (t=0) should be gone; the newest should be present.
    expect(segs.find((s) => s.start_secs === 0)).toBeUndefined();
    expect(segs.find((s) => s.start_secs === extraStart)).toBeDefined();
  });

  test("length stays at cap after a burst of segments well above cap", () => {
    // Append CAP + 200 finals; the store should stay at CAP after each.
    appendMany(CAP + 200);
    expect(segments()).toHaveLength(CAP);
  });

  test("only finals are dropped — the remaining entries are all final", () => {
    appendMany(CAP + 10);
    expect(segments().every((s) => s.is_final)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 3. Pending partial at the tail is preserved during eviction
// ---------------------------------------------------------------------------

describe("partial preservation during eviction", () => {
  test("partial at tail is kept when finals are evicted to make room", () => {
    // CAP finals, then a partial revision at the tail.
    appendMany(CAP);
    const partialStart = CAP * 3 + 10;
    const { append } = useTranscriptStore.getState();
    append(MID, seg(partialStart, false, "in-flight"));

    // One more final pushes us over CAP+1 — eviction must not drop the partial.
    append(MID, seg(CAP * 3, true, "seals-gap"));

    const segs = segments();
    // Length must be exactly CAP (1 eviction happened).
    expect(segs).toHaveLength(CAP);
    // The in-flight partial must still be present.
    const partial = segs.find((s) => !s.is_final);
    expect(partial).toBeDefined();
    expect(partial?.text).toBe("in-flight");
  });
});

// ---------------------------------------------------------------------------
// 4. reset() wipes everything for the meeting
// ---------------------------------------------------------------------------

describe("reset(meetingId)", () => {
  test("clears all segments for the meeting", () => {
    appendMany(10);
    expect(segments()).toHaveLength(10);
    useTranscriptStore.getState().reset(MID);
    expect(segments()).toHaveLength(0);
  });

  test("reset does not affect segments for a different meeting", () => {
    const OTHER = "meeting-other" as const;
    const { append } = useTranscriptStore.getState();
    appendMany(3);
    append(OTHER, seg(0, true, "other"));

    useTranscriptStore.getState().reset(MID);

    expect(segments()).toHaveLength(0);
    const otherSegs = useTranscriptStore.getState().segments[OTHER] ?? [];
    expect(otherSegs).toHaveLength(1);

    // Clean up the other meeting too.
    useTranscriptStore.getState().reset(OTHER);
  });
});
