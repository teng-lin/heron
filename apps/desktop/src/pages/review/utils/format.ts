/**
 * Pure formatting / extraction helpers for the Review route.
 *
 * Extracted from `pages/Review.tsx` per issue #195 so the file
 * reads as a "page that orchestrates loads + saves" rather than
 * "page + four formatters + an action-item extractor".
 *
 * Re-exported from `pages/Review.tsx` for back-compat with existing
 * test imports.
 */

import type { ActionItem, Meeting } from "../../../lib/types";

const ISO_DATE_RE = /^(\d{4})-(\d{2})-(\d{2})$/;
const ACTION_ITEM_DUE_FORMATTER = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  year: "numeric",
});

const TOKEN_COUNT_FORMATTER_INTL = new Intl.NumberFormat(undefined);

/**
 * Wide-locale-friendly token count formatter — re-exported as a
 * constant so the right-rail can format `tokens_in` / `tokens_out`
 * with a thousands separator without each render rebuilding the
 * `Intl.NumberFormat`.
 */
export const TOKEN_COUNT_FORMATTER = TOKEN_COUNT_FORMATTER_INTL;

/**
 * Format an ISO timestamp for the .md.bak restore pill — date and
 * time, in the user's locale. Falls back to the raw string when the
 * input is unparseable so a malformed `created_at` surfaces as
 * literal text instead of "Invalid Date".
 */
export function formatBackupTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(d);
}

/**
 * `Frontmatter.action_items[].due` is `YYYY-MM-DD` (a calendar date,
 * not a timestamp). Parsing it through `new Date(iso)` would treat
 * the string as midnight UTC, which can drift to the prior calendar
 * day in negative-offset timezones. Pin the parts manually so the
 * formatted output matches the date the LLM emitted.
 *
 * Falls back to the raw string when the input doesn't match the
 * expected `YYYY-MM-DD` shape (defensive — a future LLM template
 * change shouldn't render `Invalid Date`).
 */
export function formatActionItemDue(iso: string): string {
  const match = ISO_DATE_RE.exec(iso);
  if (!match) return iso;
  const [, y, m, d] = match;
  const yi = Number(y);
  const mi = Number(m);
  const di = Number(d);
  const date = new Date(yi, mi - 1, di);
  // The `Date` constructor rolls invalid components silently —
  // `2026-02-31` becomes `Mar 3, 2026`, `2026-13-01` becomes
  // `Jan 1, 2027`. Reject anything where the round-trip doesn't
  // match the input so a buggy LLM template surfaces as raw text
  // instead of a confidently-wrong calendar date.
  if (
    date.getFullYear() !== yi ||
    date.getMonth() !== mi - 1 ||
    date.getDate() !== di
  ) {
    return iso;
  }
  return ACTION_ITEM_DUE_FORMATTER.format(date);
}

/**
 * Format `MeetingProcessing.summary_usd` for the right-rail. The
 * summarizer can emit very small amounts (a $0.00004 prompt-cache hit
 * shouldn't render as `$0.00`), so step the precision based on
 * magnitude rather than pinning two decimals. `Intl.NumberFormat`
 * with `maximumFractionDigits` doesn't hit this on its own — it would
 * collapse `0.00004` to `0` in the default `currency` style.
 */
export function formatProcessingCost(usd: number): string {
  if (!Number.isFinite(usd)) return "—";
  const abs = Math.abs(usd);
  // Bucket on the *post-rounding* magnitude so adjacent inputs across
  // a threshold render at consistent precision: `0.0009999` and
  // `0.001` both display as "$0.0010" instead of one rounding up
  // into the next bucket. Standard currency precision (2 digits)
  // applies once a value rounds to >= $0.01.
  let digits: number;
  if (abs === 0 || abs >= 0.005) {
    digits = 2;
  } else if (abs >= 0.00005) {
    digits = 4;
  } else {
    digits = 6;
  }
  return new Intl.NumberFormat(undefined, {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  }).format(usd);
}

/**
 * Pull the bullet list under `## Action Items` (or `## Actions`)
 * out of the markdown. Pragmatic regex — the v1 LLM template emits
 * the heading verbatim. Returns `[]` when no section exists.
 *
 * Tier 0 #3 of the UX redesign moves the canonical source for action
 * items off the markdown body and onto the `Meeting.action_items`
 * wire field. This regex extractor stays as a fallback for vault
 * notes that pre-date the structured emission (or for daemons that
 * haven't been upgraded yet) — see `selectActionItems`.
 *
 * Exported for unit-test consumption; not part of the public app
 * surface.
 */
export function extractActionItems(markdown: string): string[] {
  const re = /^##\s+(?:Action items|Actions)\s*$/im;
  const match = markdown.match(re);
  if (!match || match.index === undefined) return [];
  const tail = markdown.slice(match.index + match[0].length);
  // Stop at the next `## ` heading (or EOF). Leading whitespace +
  // dash bullets are normalized to plain strings.
  const nextHeading = tail.match(/^##\s+/m);
  const section = nextHeading ? tail.slice(0, nextHeading.index) : tail;
  return section
    .split("\n")
    .map((line) => line.match(/^\s*[-*]\s+(.*)$/))
    .filter((m): m is RegExpMatchArray => m !== null)
    .map((m) => m[1].trim())
    .filter((s) => s.length > 0);
}

/**
 * Uniform shape the Actions tab renders. `id` is stable for typed
 * rows (Tier 0 #3) and synthesized (`fallback:<index>`) for
 * regex-extracted bullets so React keys stay distinct.
 */
export interface ActionItemRow {
  id: string;
  text: string;
  owner: string | null;
  due: string | null;
  /**
   * Day 8–10 (action-item write-back). Mirrors `ActionItem.done` from
   * the wire. Always `false` for `structured: false` rows because the
   * regex-fallback path can't recover the flag — and the editor hides
   * the checkbox on those rows anyway, so the value is just a default
   * that satisfies the type.
   */
  done: boolean;
  /**
   * `true` when this row came from the structured
   * `Meeting.action_items` wire field (Tier 0 #3); `false` when it
   * was reconstructed from the markdown body via the legacy regex
   * extractor. The Actions tab uses this to gate the assignee / due
   * pill rendering — the regex path can't recover those.
   */
  structured: boolean;
}

/**
 * Tier 0 #3: prefer the structured `Meeting.action_items` wire
 * field, fall back to regex-extracted bullets when the field is
 * absent or empty. Empty / absent structured field on a finalized
 * note is the legacy-vault signal: pre-Tier-0-#3 frontmatter wrote
 * action items only into the markdown body, so the wire field stays
 * empty and we have to recover them from prose.
 *
 * Exported for testability — the precedence rule is the load-bearing
 * piece of this PR.
 */
export function selectActionItems(
  meeting: Meeting | null,
  markdown: string,
): ActionItemRow[] {
  const structured = meeting?.action_items ?? [];
  if (structured.length > 0) {
    return structured.map((item: ActionItem, idx: number) => ({
      // `id` is optional on the wire (back-compat with pre-Tier-0
      // daemons), so we synthesize a stable React key from the index
      // when it's missing rather than collapsing all rows onto the
      // same key.
      id: item.id ?? `legacy:${idx}`,
      text: item.text,
      owner: item.owner,
      due: item.due,
      // Day 8–10: `done` is required on the wire post-write-back.
      // Coalesce missing for back-compat with daemons that haven't
      // shipped the field yet — the read path treats it as `false`.
      done: item.done ?? false,
      structured: true,
    }));
  }
  return extractActionItems(markdown).map((text, idx) => ({
    id: `fallback:${idx}`,
    text,
    owner: null,
    due: null,
    done: false,
    structured: false,
  }));
}
