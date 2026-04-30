/**
 * Pure-helper tests for `filterMeetings`.
 *
 * Day 4 of the UX-redesign sprint adds the `tagFilter` axis next to
 * the existing status / free-text filters in `MeetingsTable`. The
 * filter predicate was extracted into a pure function so the
 * status × tag × query matrix can be pinned without React renderer
 * scaffolding (same pure-helper convention as `Review.test.ts`).
 *
 * Cases covered:
 *
 *   1. `tagFilter === null` — no tag constraint; all meetings pass
 *      the tag axis.
 *   2. `tagFilter === "untagged"` — keep only meetings with empty /
 *      missing `tags` (the back-compat `meeting.tags ?? []` codepath).
 *   3. `tagFilter === "react"` (a specific tag) — keep only meetings
 *      whose `tags` contains that exact tag, case-insensitive.
 *   4. Status × tag compose AND — selecting "active" + "untagged"
 *      drops a `done` meeting even if it's untagged.
 *   5. Free-text query searches tag strings as well as title /
 *      platform / participant.
 */

import { describe, expect, test } from "bun:test";

import type { Meeting } from "../../lib/types";
import { filterMeetings } from "./meetings-table";

function meetingFixture(overrides: Partial<Meeting>): Meeting {
  return {
    id: overrides.id ?? "mtg_test",
    status: overrides.status ?? "done",
    platform: overrides.platform ?? "zoom",
    title: overrides.title ?? null,
    calendar_event_id: null,
    started_at: "2026-04-29T10:00:00Z",
    ended_at: "2026-04-29T10:30:00Z",
    duration_secs: 1_800,
    participants: overrides.participants ?? [],
    transcript_status: "complete",
    summary_status: "ready",
    tags: overrides.tags,
  };
}

describe("filterMeetings — tag axis", () => {
  test("`null` tag filter passes every meeting through the tag axis", () => {
    const tagged = meetingFixture({ id: "a", tags: ["react"] });
    const untagged = meetingFixture({ id: "b", tags: [] });
    const legacy = meetingFixture({ id: "c", tags: undefined });
    const out = filterMeetings([tagged, untagged, legacy], "", "all", null);
    expect(out.map((m) => m.id)).toEqual(["a", "b", "c"]);
  });

  test("`untagged` keeps only meetings with empty / missing `tags`", () => {
    // Coverage for the `meeting.tags ?? []` back-compat path. The
    // pre-Tier-0-#1 daemon emits no `tags` field at all; a freshly-
    // armed meeting emits an empty array. Both should pass the
    // `Untagged` filter — the user's mental model of "no tags" is
    // identical for the two.
    const tagged = meetingFixture({ id: "a", tags: ["react"] });
    const untagged = meetingFixture({ id: "b", tags: [] });
    const legacy = meetingFixture({ id: "c", tags: undefined });
    const out = filterMeetings(
      [tagged, untagged, legacy],
      "",
      "all",
      "untagged",
    );
    expect(out.map((m) => m.id)).toEqual(["b", "c"]);
  });

  test("specific-tag filter is exact-match, case-insensitive", () => {
    // The summarizer emits tags however the LLM writes them — usually
    // lowercase, but not always. The Home filter is pinned to
    // case-insensitive exact match so a user clicking `#React` from a
    // row chip still surfaces a meeting tagged `react` from another
    // row.
    const a = meetingFixture({ id: "a", tags: ["React", "frontend"] });
    const b = meetingFixture({ id: "b", tags: ["react"] });
    const c = meetingFixture({ id: "c", tags: ["backend"] });
    const out = filterMeetings([a, b, c], "", "all", "react");
    expect(out.map((m) => m.id)).toEqual(["a", "b"]);
  });

  test("status × tag compose AND", () => {
    // A `done` meeting with no tags should NOT show up when the
    // status filter is `active` even though it satisfies the
    // `untagged` axis on its own.
    const activeUntagged = meetingFixture({
      id: "a",
      status: "recording",
      tags: [],
    });
    const doneUntagged = meetingFixture({
      id: "b",
      status: "done",
      tags: [],
    });
    const out = filterMeetings(
      [activeUntagged, doneUntagged],
      "",
      "active",
      "untagged",
    );
    expect(out.map((m) => m.id)).toEqual(["a"]);
  });
});

describe("filterMeetings — free-text query searches tags", () => {
  test("substring of a tag string matches the row", () => {
    // The free-text search bar is the catch-all — typing `react` into
    // the search box should hit a meeting tagged `react` even when
    // the title doesn't mention it. Substring (vs exact) so a partial
    // type still narrows the list.
    const a = meetingFixture({
      id: "a",
      title: "Weekly sync",
      tags: ["react", "frontend"],
    });
    const b = meetingFixture({
      id: "b",
      title: "Weekly sync",
      tags: ["backend"],
    });
    const out = filterMeetings([a, b], "react", "all", null);
    expect(out.map((m) => m.id)).toEqual(["a"]);
  });
});
