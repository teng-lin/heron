/**
 * Session basename helpers for the Review sidebar.
 *
 * The vault writer in `crates/heron-vault/src/writer.rs` formats
 * filenames as `YYYY-MM-DD-HHMM <slug>.md` (per `docs/plan.md` §3.2).
 * PR-κ (phase 72) parses this prefix in the renderer rather than
 * shelling out to the filesystem so the sidebar stays offline,
 * IPC-free, and survives sessions whose mtime drifted (e.g. a vault
 * synced through Dropbox after the original write).
 *
 * Filenames that don't match the convention fall back to the raw
 * basename — we never throw, since user-authored notes occasionally
 * land in the meetings folder during dogfooding.
 */
export interface ParsedSessionName {
  /** Raw basename (no `.md` suffix) — the IPC handle for the session. */
  id: string;
  /** Parsed start timestamp; `null` if the basename didn't match the convention. */
  timestamp: Date | null;
  /** Display label without the date prefix; falls back to `id` on no match. */
  label: string;
}

/**
 * Bucket label used by the sidebar grouping. Order is meaningful —
 * the sidebar renders in this exact sequence.
 */
export type SessionBucket = "today" | "yesterday" | "thisWeek" | "older";

/** Display order of session buckets; the sidebar iterates this list. */
export const SESSION_BUCKET_ORDER: readonly SessionBucket[] = [
  "today",
  "yesterday",
  "thisWeek",
  "older",
] as const;

/** Human-readable header for each bucket. */
export const SESSION_BUCKET_LABELS: Record<SessionBucket, string> = {
  today: "Today",
  yesterday: "Yesterday",
  thisWeek: "This week",
  older: "Older",
};

/**
 * Buckets that default to expanded on first render. Older buckets
 * collapse so a long-running vault doesn't dominate the sidebar.
 */
export const DEFAULT_EXPANDED_BUCKETS: ReadonlySet<SessionBucket> = new Set([
  "today",
  "yesterday",
]);

/**
 * Match `YYYY-MM-DD-HHMM <slug>` at the start of the basename. Slug is
 * everything after the first space — including more spaces — but never
 * the trailing newline. Slug may be empty (we still surface the
 * timestamp, just with no label).
 */
const SESSION_PREFIX = /^(\d{4})-(\d{2})-(\d{2})-(\d{2})(\d{2})(?:\s+(.*))?$/;

/**
 * Strip the `.md` suffix if present, returning the raw session id used
 * by `heron_read_note` / `heron_list_sessions`.
 */
function stripExtension(basename: string): string {
  return basename.endsWith(".md") ? basename.slice(0, -3) : basename;
}

/**
 * Parse the leading `YYYY-MM-DD-HHMM` prefix into a `Date`. Returns
 * `null` for unparseable basenames so the caller can fall back to
 * raw display.
 *
 * The prefix is interpreted as **local time** — the writer formats it
 * from the user's wall clock when the meeting started, and showing
 * "9am" for a meeting the user remembers as 9am is the right behavior
 * even if the user later travels across time zones.
 */
export function parseSessionTimestamp(basename: string): Date | null {
  return parseSessionName(basename).timestamp;
}

/**
 * Parse a session basename into the parts the sidebar renders.
 *
 * Returns `timestamp = null` and `label = id` for filenames that don't
 * match the `YYYY-MM-DD-HHMM <slug>` convention so the sidebar still
 * surfaces them.
 */
export function parseSessionName(basename: string): ParsedSessionName {
  const id = stripExtension(basename);
  const m = SESSION_PREFIX.exec(id);
  if (!m) return { id, timestamp: null, label: id };
  // Regex constrains all five components to digits, so Number() is
  // guaranteed finite — we only need range + calendar-validity checks.
  const year = Number(m[1]);
  const month = Number(m[2]);
  const day = Number(m[3]);
  const hour = Number(m[4]);
  const minute = Number(m[5]);
  if (
    month < 1 ||
    month > 12 ||
    day < 1 ||
    day > 31 ||
    hour > 23 ||
    minute > 59
  ) {
    return { id, timestamp: null, label: id };
  }
  const date = new Date(year, month - 1, day, hour, minute, 0, 0);
  // `Date` constructor wraps invalid components silently
  // (e.g. month=2, day=31 → March 3) — verify the round-trip so
  // out-of-range values fall through to the raw-basename path.
  if (
    date.getFullYear() !== year ||
    date.getMonth() !== month - 1 ||
    date.getDate() !== day ||
    date.getHours() !== hour ||
    date.getMinutes() !== minute
  ) {
    return { id, timestamp: null, label: id };
  }
  // Group 6 is the slug (everything after the prefix); empty when the
  // session has no slug (rare but legal — we still want to render the
  // clock).
  const slug = m[6]?.trim() ?? "";
  return { id, timestamp: date, label: slug };
}

/**
 * Compare two parsed sessions for descending chronological order.
 * Sessions without a timestamp sort to the bottom, then by descending
 * id (lexicographic) — preserving the PR-γ behavior for unparseable
 * names.
 */
export function compareSessionsDesc(
  a: ParsedSessionName,
  b: ParsedSessionName,
): number {
  if (a.timestamp && b.timestamp) {
    return b.timestamp.getTime() - a.timestamp.getTime();
  }
  if (a.timestamp) return -1;
  if (b.timestamp) return 1;
  return b.id.localeCompare(a.id);
}

/**
 * Local-time start-of-day for the given date.
 *
 * Exposed so tests can pin a fixed "now" without monkey-patching
 * `Date`.
 */
export function startOfLocalDay(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate(), 0, 0, 0, 0);
}

/**
 * Stable string key for a local calendar day, e.g. `"2026-04-25"`.
 *
 * Used by the sidebar to invalidate the bucketing memo when the local
 * day rolls over — comparing strings is cheaper than rebuilding the
 * full bucket map on every render, and keeps the memo dep array
 * primitive-only.
 */
export function localDayKey(d: Date): string {
  const y = d.getFullYear().toString().padStart(4, "0");
  const m = (d.getMonth() + 1).toString().padStart(2, "0");
  const day = d.getDate().toString().padStart(2, "0");
  return `${y}-${m}-${day}`;
}

/**
 * Bucket a session timestamp relative to `now`.
 *
 * - `today`: same local calendar day as `now`.
 * - `yesterday`: the day before `now`.
 * - `thisWeek`: 2–6 days ago (so the bucket is a rolling 7-day window
 *   excluding today + yesterday).
 * - `older`: everything else, including future-dated sessions (those
 *   are vanishingly rare but not worth a fifth bucket).
 *
 * Sessions without a timestamp are bucketed as `older` — the sidebar
 * still surfaces them, just at the bottom of the list.
 */
export function bucketForSession(
  timestamp: Date | null,
  now: Date = new Date(),
): SessionBucket {
  if (!timestamp) return "older";
  const today = startOfLocalDay(now);
  const session = startOfLocalDay(timestamp);
  const dayMs = 24 * 60 * 60 * 1000;
  const diffDays = Math.round((today.getTime() - session.getTime()) / dayMs);
  if (diffDays === 0) return "today";
  if (diffDays === 1) return "yesterday";
  if (diffDays >= 2 && diffDays <= 6) return "thisWeek";
  return "older";
}

/**
 * Group basenames into the four buckets, preserving descending
 * chronological order within each bucket. Buckets with zero sessions
 * are still present in the returned map (callers skip them at render
 * time) so consumers don't need to remember the bucket-list keys.
 */
export function groupSessionsByBucket(
  basenames: readonly string[],
  now: Date = new Date(),
): Record<SessionBucket, ParsedSessionName[]> {
  const buckets: Record<SessionBucket, ParsedSessionName[]> = {
    today: [],
    yesterday: [],
    thisWeek: [],
    older: [],
  };
  for (const raw of basenames) {
    const parsed = parseSessionName(raw);
    buckets[bucketForSession(parsed.timestamp, now)].push(parsed);
  }
  for (const key of SESSION_BUCKET_ORDER) {
    buckets[key].sort(compareSessionsDesc);
  }
  return buckets;
}

/**
 * Format the leading `HH:MM` clock for a parsed session, using the
 * user's locale via `Intl.DateTimeFormat`. Empty string when the
 * session has no parseable timestamp; the caller then falls back to
 * the raw id.
 */
export function formatSessionClock(timestamp: Date | null): string {
  if (!timestamp) return "";
  return new Intl.DateTimeFormat(undefined, {
    hour: "2-digit",
    minute: "2-digit",
  }).format(timestamp);
}

/**
 * Compose the final list-item label.
 *
 * - `HH:MM — <title>` when both clock and slug are present.
 * - `HH:MM` when the slug is empty.
 * - The raw basename when the timestamp didn't parse.
 *
 * The em-dash is intentional — a regular hyphen reads as part of the
 * slug when slugs contain hyphens.
 */
export function formatSessionLabel(parsed: ParsedSessionName): string {
  const clock = formatSessionClock(parsed.timestamp);
  if (!clock) return parsed.id;
  if (!parsed.label) return clock;
  return `${clock} — ${parsed.label}`;
}
