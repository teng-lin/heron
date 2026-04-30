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

// Strip all non-digit characters so currency assertions don't break
// on hosts that swap `,`/`.` (e.g. de-DE) or move the `$`. We assert
// digit-sequence + sign + presence of `$`, which captures the
// implementation contract without locking into en-US punctuation.
function digitsOf(s: string): string {
  return s.replace(/\D/g, "");
}

describe("formatActionItemDue", () => {
  test("renders a non-raw human date for a valid YYYY-MM-DD", () => {
    // Parsing the literal `YYYY-MM-DD` string through `new Date(iso)`
    // would treat it as midnight UTC and shift to the previous day in
    // negative-offset zones. The formatter splits the parts out so
    // `2026-05-01` always renders the date the LLM emitted regardless
    // of TZ. Locale-agnostic check: the year survives, a day digit
    // survives, and the output isn't the raw input or `Invalid Date`.
    const out = formatActionItemDue("2026-05-01");
    expect(out).not.toBe("2026-05-01");
    expect(out).not.toContain("Invalid");
    expect(out).toContain("2026");
    expect(out).toMatch(/\b0?1\b/);
  });

  test("falls back to the raw string when the input doesn't match", () => {
    expect(formatActionItemDue("not a date")).toBe("not a date");
    expect(formatActionItemDue("2026/05/01")).toBe("2026/05/01");
  });

  test("rejects out-of-range month and day instead of rolling them over", () => {
    // The `Date` constructor accepts and silently rolls these:
    // `2026-13-01` would become Jan 1, 2027; `2026-02-31` becomes
    // Mar 3, 2026. Returning the raw string surfaces a buggy LLM
    // emission instead of rendering a confidently-wrong real date.
    expect(formatActionItemDue("2026-13-01")).toBe("2026-13-01");
    expect(formatActionItemDue("2026-02-31")).toBe("2026-02-31");
    expect(formatActionItemDue("2026-04-31")).toBe("2026-04-31");
    expect(formatActionItemDue("2026-00-15")).toBe("2026-00-15");
    expect(formatActionItemDue("2026-05-32")).toBe("2026-05-32");
    expect(formatActionItemDue("0000-00-00")).toBe("0000-00-00");
  });

  test("rejects ISO timestamps and surrounding whitespace", () => {
    // Anchored regex enforces date-only shape; pinning here so a
    // future maintainer doesn't loosen `^...$` and accidentally
    // start passing timestamps through the part-parser.
    expect(formatActionItemDue("2026-05-01T00:00:00Z")).toBe(
      "2026-05-01T00:00:00Z",
    );
    expect(formatActionItemDue(" 2026-05-01")).toBe(" 2026-05-01");
    expect(formatActionItemDue("2026-05-01\n")).toBe("2026-05-01\n");
  });
});

describe("formatProcessingCost", () => {
  test("renders typical dollar amounts at two decimals", () => {
    expect(formatProcessingCost(1.23)).toContain("$");
    expect(digitsOf(formatProcessingCost(1.23))).toBe("123");
    expect(digitsOf(formatProcessingCost(12))).toBe("1200");
  });

  test("renders sub-cent amounts without collapsing to $0.00", () => {
    // Anti-regression: a $0.00004 prompt-cache hit must not show as
    // "$0" — that's the failure mode the prompt called out explicitly.
    const tiny = formatProcessingCost(0.00004);
    expect(tiny).toContain("$");
    expect(digitsOf(tiny)).toBe("0000040");
  });

  test("renders sub-dollar amounts with four decimals", () => {
    expect(digitsOf(formatProcessingCost(0.0042))).toBe("00042");
  });

  test("renders zero with two-decimal precision", () => {
    expect(formatProcessingCost(0)).toContain("$");
    expect(digitsOf(formatProcessingCost(0))).toBe("000");
  });

  test("falls back to em-dash on non-finite input", () => {
    expect(formatProcessingCost(Number.NaN)).toBe("—");
    expect(formatProcessingCost(Number.POSITIVE_INFINITY)).toBe("—");
    expect(formatProcessingCost(Number.NEGATIVE_INFINITY)).toBe("—");
  });

  test("renders sub-dollar values >= 1 cent at two decimals", () => {
    // Anti-regression: the previous threshold pinned 4 digits below
    // $1, so $0.50 rendered as "$0.5000". Standard currency precision
    // takes over once the value rounds to at least one cent.
    expect(digitsOf(formatProcessingCost(0.5))).toBe("050");
    expect(digitsOf(formatProcessingCost(0.01))).toBe("001");
    expect(digitsOf(formatProcessingCost(0.99))).toBe("099");
  });

  test("buckets on the post-rounding magnitude to avoid threshold inversions", () => {
    // `0.0009999` and `0.001` both round to the same displayed value
    // and must use the same precision; the previous logic gave
    // "$0.001000" vs "$0.0010" for adjacent inputs.
    expect(formatProcessingCost(0.0009999)).toBe(
      formatProcessingCost(0.001),
    );
  });

  test("renders negative amounts with sign", () => {
    const out = formatProcessingCost(-1.23);
    expect(out).toContain("$");
    expect(out).toContain("-");
    expect(digitsOf(out)).toBe("123");
    const tiny = formatProcessingCost(-0.0042);
    expect(tiny).toContain("-");
    expect(digitsOf(tiny)).toBe("00042");
  });

  test("renders very large amounts without scientific notation", () => {
    const out = formatProcessingCost(1_000_000);
    expect(out).toContain("$");
    expect(out).not.toMatch(/e/i);
    expect(digitsOf(out)).toBe("100000000");
  });
});
