/**
 * Pure-helper tests for `extractActionItems` + `selectActionItems`.
 *
 * Tier 0 #3 of the UX redesign: the Actions tab now prefers the
 * structured `Meeting.action_items` wire field over a regex extracted
 * from the markdown body. The precedence rule and the back-compat
 * fallback are the load-bearing pieces — pin them here so a future
 * refactor can't silently re-introduce the regex-only path.
 *
 * Cases covered:
 *
 *   1. `extractActionItems` still works on legacy notes (regex
 *      under `## Action items`).
 *   2. `selectActionItems` prefers structured rows when present and
 *      surfaces `id`, `owner`, `due` straight through.
 *   3. Empty / undefined `meeting.action_items` falls back to the
 *      regex extractor.
 *   4. `null` meeting (e.g. daemon unavailable) falls back to the
 *      regex extractor against the live markdown.
 */

import { describe, expect, test } from "bun:test";

import type { Meeting } from "../lib/types";
import {
  extractActionItems,
  formatActionItemDue,
  formatProcessingCost,
  selectActionItems,
} from "./Review";

function meetingWith(actionItems: Meeting["action_items"]): Meeting {
  return {
    id: "mtg_test",
    status: "done",
    platform: "zoom",
    title: null,
    calendar_event_id: null,
    started_at: "2026-04-29T10:00:00Z",
    ended_at: "2026-04-29T10:30:00Z",
    duration_secs: 1_800,
    participants: [],
    transcript_status: "complete",
    summary_status: "ready",
    action_items: actionItems,
  };
}

describe("extractActionItems", () => {
  test("pulls bullets under `## Action items`", () => {
    const md = [
      "# Header",
      "",
      "## Action items",
      "",
      "- Write the doc",
      "- Pick a reviewer",
      "",
      "## Decisions",
      "- (ignored)",
    ].join("\n");
    expect(extractActionItems(md)).toEqual([
      "Write the doc",
      "Pick a reviewer",
    ]);
  });

  test("returns empty list when the section is absent", () => {
    expect(extractActionItems("# Just a title\n\nbody.\n")).toEqual([]);
  });

  test("accepts the `## Actions` heading variant", () => {
    expect(extractActionItems("## Actions\n- item one\n")).toEqual([
      "item one",
    ]);
  });
});

describe("selectActionItems", () => {
  test("prefers structured rows from Meeting.action_items", () => {
    // Load-bearing: the typed path beats the regex path even when
    // the markdown body still has a `## Action items` section. This
    // is the post-Tier-0-#3 happy path.
    const meeting = meetingWith([
      {
        id: "11111111-2222-3333-4444-555555555555",
        text: "Write the doc",
        owner: "Teng",
        due: "2026-05-01",
      },
      {
        id: "22222222-3333-4444-5555-666666666666",
        text: "Pick a reviewer",
        owner: null,
        due: null,
      },
    ]);
    const md = "## Action items\n- (regex would say this, ignore)\n";
    const rows = selectActionItems(meeting, md);
    expect(rows).toHaveLength(2);
    expect(rows[0]).toEqual({
      id: "11111111-2222-3333-4444-555555555555",
      text: "Write the doc",
      owner: "Teng",
      due: "2026-05-01",
      structured: true,
    });
    expect(rows[1].owner).toBeNull();
    expect(rows[1].due).toBeNull();
    expect(rows[1].structured).toBe(true);
  });

  test("falls back to regex when structured field is absent", () => {
    // Legacy vault note (or pre-Tier-0-#3 daemon): the wire field
    // is absent, so the actions tab keeps working by regex-parsing
    // the markdown body. `structured: false` lets the renderer hide
    // owner/due pills the regex can't recover.
    const meeting = meetingWith(undefined);
    const md = "## Action items\n- legacy bullet\n";
    const rows = selectActionItems(meeting, md);
    expect(rows).toHaveLength(1);
    expect(rows[0].text).toBe("legacy bullet");
    expect(rows[0].structured).toBe(false);
    expect(rows[0].owner).toBeNull();
    expect(rows[0].id).toBe("fallback:0");
  });

  test("falls back to regex when structured field is empty", () => {
    // Important wrinkle: an empty array on the wire is the same
    // signal as "field absent" — the daemon emits `[]` for live
    // meetings, but at that point the markdown body is also empty
    // so the regex extractor returns `[]`. A finalized note with a
    // body that has bullets but an empty wire field is the legacy
    // case the fallback unblocks.
    const meeting = meetingWith([]);
    const md = "## Action items\n- still works\n";
    expect(selectActionItems(meeting, md)).toEqual([
      {
        id: "fallback:0",
        text: "still works",
        owner: null,
        due: null,
        structured: false,
      },
    ]);
  });

  test("falls back to regex when the meeting is null", () => {
    // Daemon unavailable / 404: `meetingLoad.kind === "unavailable"`
    // surfaces a null meeting. Actions tab should still work off
    // the locally-read markdown.
    const md = "## Action items\n- offline-friendly\n";
    expect(selectActionItems(null, md)).toEqual([
      {
        id: "fallback:0",
        text: "offline-friendly",
        owner: null,
        due: null,
        structured: false,
      },
    ]);
  });

  test("synthesizes a stable react key when structured rows omit id", () => {
    // Pre-Tier-0-#3 daemons used `#[serde(default)]` to fill `id`
    // with the nil UUID; the Rust-side back-compat note documents
    // that behaviour. JS-side, a missing `id` would otherwise
    // collapse every row onto `undefined` — synthesize a unique
    // key so React doesn't merge them.
    const meeting = meetingWith([
      { text: "first", owner: null, due: null },
      { text: "second", owner: null, due: null },
    ]);
    const rows = selectActionItems(meeting, "");
    expect(rows.map((r) => r.id)).toEqual(["legacy:0", "legacy:1"]);
    expect(rows.every((r) => r.structured)).toBe(true);
  });
});

describe("formatActionItemDue", () => {
  test("renders YYYY-MM-DD as a human date in the local calendar", () => {
    // Parsing the literal `YYYY-MM-DD` string through `new Date(iso)`
    // would treat it as midnight UTC and shift to the previous day in
    // negative-offset zones. The formatter splits the parts out so
    // `2026-05-01` always renders as May 1, 2026 regardless of TZ.
    const out = formatActionItemDue("2026-05-01");
    expect(out).toContain("2026");
    expect(out).toContain("1");
    expect(out.toLowerCase()).toContain("may");
  });

  test("falls back to the raw string when the input doesn't match", () => {
    expect(formatActionItemDue("not a date")).toBe("not a date");
    expect(formatActionItemDue("2026/05/01")).toBe("2026/05/01");
  });
});

describe("formatProcessingCost", () => {
  test("renders typical dollar amounts at two decimals", () => {
    expect(formatProcessingCost(1.23)).toBe("$1.23");
    expect(formatProcessingCost(12)).toBe("$12.00");
  });

  test("renders sub-cent amounts without collapsing to $0.00", () => {
    // Anti-regression: a $0.00004 prompt-cache hit must not show as
    // "$0" — that's the failure mode the prompt called out explicitly.
    const tiny = formatProcessingCost(0.00004);
    expect(tiny).not.toBe("$0");
    expect(tiny).not.toBe("$0.00");
    expect(tiny).toContain("0.000040");
  });

  test("renders sub-dollar amounts with four decimals", () => {
    expect(formatProcessingCost(0.0042)).toBe("$0.0042");
  });

  test("renders zero as $0.00", () => {
    expect(formatProcessingCost(0)).toBe("$0.00");
  });

  test("falls back to em-dash on non-finite input", () => {
    expect(formatProcessingCost(Number.NaN)).toBe("—");
    expect(formatProcessingCost(Number.POSITIVE_INFINITY)).toBe("—");
  });
});
