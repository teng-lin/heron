/**
 * Validation helpers for the action-item write-back surface.
 *
 * The two predicates here are load-bearing for the optimistic-edit
 * controller: each one gates IPC calls so a malformed input never
 * round-trips through the optimistic UI just to be rejected at the
 * Rust writer boundary.
 */

const ISO_DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * `YYYY-MM-DD` calendar-date check. The Rust side validates via
 * `chrono::NaiveDate::parse_from_str` which rejects nonsense like
 * `2026-13-01` or `2026-02-30`; we mirror that semantic here so a
 * laxer client doesn't round-trip a bad date through optimistic UI
 * just to be rejected at the writer boundary. The shape regex catches
 * `9999-99-99`-style inputs at the keystroke; the `Date` round-trip
 * catches the calendar-impossible cases the regex admits.
 *
 * Exported for the controller test — the validation is the load-bearing
 * piece of the due-edit flow.
 */
export function isValidIsoDate(value: string): boolean {
  if (!ISO_DATE_RE.test(value)) return false;
  const parsed = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(parsed.getTime()) && parsed.toISOString().startsWith(value);
}

/**
 * `true` when `id` is a real UUID — the only id shape the Rust
 * `update_action_item` boundary accepts. Synthesized prefixes from
 * `selectActionItems` (`legacy:N` for structured rows whose wire id
 * was dropped by a pre-Tier-0-#3 daemon, `fallback:N` for regex-
 * extracted bullets) deliberately fail this check so the optimistic
 * UI doesn't fire an IPC the backend will reject with a confusing
 * "validation: item_id is not a UUID" toast.
 *
 * Exported so the React surface can branch its affordances on the
 * same predicate the controller uses to gate IPC calls — a row that
 * fails this check renders without checkbox / edit chips so the
 * user can't even click. (Belt-and-suspenders: the controller still
 * gates internally so a direct caller can't bypass.)
 */
export function isStableActionItemId(id: string): boolean {
  return UUID_RE.test(id);
}
