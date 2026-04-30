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
 *   1. `{ kind: "all" }` — no tag constraint; all meetings pass
 *      the tag axis.
 *   2. `{ kind: "untagged" }` — keep only meetings with empty /
 *      missing `tags` (the back-compat `meeting.tags ?? []` codepath).
 *   3. `{ kind: "tag", value: "react" }` — keep only meetings whose
 *      `tags` contains that exact tag, case-insensitive.
 *   4. Discriminated-union pin: a tag literally named `"untagged"`
 *      cannot collide with the no-tags sentinel. Regression for the
 *      original `null | "untagged" | string` shape.
 *   5. Status × tag compose AND — selecting "active" + "untagged"
 *      drops a `done` meeting even if it's untagged.
 *   6. Untagged × free-text composes AND — `react` query plus
 *      untagged sentinel keeps title-only matches but drops rows
 *      that only matched via tag strings.
 *   7. Free-text query searches tag strings as well as title /
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
  test("`{ kind: 'all' }` tag filter passes every meeting through the tag axis", () => {
    const tagged = meetingFixture({ id: "a", tags: ["react"] });
    const untagged = meetingFixture({ id: "b", tags: [] });
    const legacy = meetingFixture({ id: "c", tags: undefined });
    const out = filterMeetings([tagged, untagged, legacy], "", "all", {
      kind: "all",
    });
    expect(out.map((m) => m.id)).toEqual(["a", "b", "c"]);
  });

  test("`{ kind: 'untagged' }` keeps only meetings with empty / missing `tags`", () => {
    // Coverage for the `meeting.tags ?? []` back-compat path. The
    // pre-Tier-0-#1 daemon emits no `tags` field at all; a freshly-
    // armed meeting emits an empty array. Both should pass the
    // `Untagged` filter — the user's mental model of "no tags" is
    // identical for the two.
    const tagged = meetingFixture({ id: "a", tags: ["react"] });
    const untagged = meetingFixture({ id: "b", tags: [] });
    const legacy = meetingFixture({ id: "c", tags: undefined });
    const out = filterMeetings([tagged, untagged, legacy], "", "all", {
      kind: "untagged",
    });
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
    const out = filterMeetings([a, b, c], "", "all", {
      kind: "tag",
      value: "react",
    });
    expect(out.map((m) => m.id)).toEqual(["a", "b"]);
  });

  test("a tag literally named 'untagged' matches `{ kind: 'tag', value: 'untagged' }`", () => {
    // Regression pin for the magic-string-collision bug the original
    // `null | "untagged" | string` shape had: an LLM-emitted tag
    // literally named `"untagged"` would be impossible to filter
    // for, because the filter would interpret it as the no-tags
    // sentinel. The discriminated union eliminates the collision —
    // the literal tag is `{ kind: "tag", value: "untagged" }` and is
    // distinct from `{ kind: "untagged" }`.
    const literalUntagged = meetingFixture({
      id: "a",
      tags: ["untagged"],
    });
    const tagged = meetingFixture({ id: "b", tags: ["react"] });
    const empty = meetingFixture({ id: "c", tags: [] });
    // `{ kind: "tag", value: "untagged" }` keeps ONLY the meeting
    // tagged `untagged` — it is NOT the no-tags sentinel.
    const tagLiteral = filterMeetings([literalUntagged, tagged, empty], "", "all", {
      kind: "tag",
      value: "untagged",
    });
    expect(tagLiteral.map((m) => m.id)).toEqual(["a"]);
    // `{ kind: "untagged" }` is the sentinel — it keeps `c` and
    // (importantly) DOES NOT match `a`'s `untagged` tag string.
    const sentinel = filterMeetings([literalUntagged, tagged, empty], "", "all", {
      kind: "untagged",
    });
    expect(sentinel.map((m) => m.id)).toEqual(["c"]);
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
    const out = filterMeetings([activeUntagged, doneUntagged], "", "active", {
      kind: "untagged",
    });
    expect(out.map((m) => m.id)).toEqual(["a"]);
  });

  test("untagged × free-text composes AND — query that only matches tags drops untagged rows", () => {
    // Reviewer-flagged coverage gap. With `{ kind: "untagged" }`
    // active, a free-text query of `"react"` must NOT surface a
    // meeting whose only `react` mention is in its tag strings (the
    // tag rows are excluded by the untagged axis), but MUST still
    // surface a meeting that has `react` in its title and is itself
    // untagged.
    const taggedReact = meetingFixture({
      id: "a",
      title: "Weekly sync",
      tags: ["react"],
    });
    const untaggedReactInTitle = meetingFixture({
      id: "b",
      title: "react review",
      tags: [],
    });
    const untaggedUnrelated = meetingFixture({
      id: "c",
      title: "1:1",
      tags: [],
    });
    const out = filterMeetings(
      [taggedReact, untaggedReactInTitle, untaggedUnrelated],
      "react",
      "all",
      { kind: "untagged" },
    );
    expect(out.map((m) => m.id)).toEqual(["b"]);
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
    const out = filterMeetings([a, b], "react", "all", { kind: "all" });
    expect(out.map((m) => m.id)).toEqual(["a"]);
  });
});
