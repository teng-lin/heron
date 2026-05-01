/**
 * Validate the user's `target_bundle_ids` list. Returns flags the
 * Settings → Audio "Recorded apps" card uses to render an inline
 * error banner.
 *
 * - `hasEmpty` — at least one row is blank / whitespace-only.
 * - `hasDupe` — two or more rows have the same trimmed bundle ID.
 *   Empty rows are excluded from the duplicate check (they're flagged
 *   separately by `hasEmpty`).
 */
export function validateBundleIds(targets: string[]): {
  hasEmpty: boolean;
  hasDupe: boolean;
} {
  const trimmed = targets.map((t) => t.trim());
  const nonEmpty = trimmed.filter((t) => t !== "");
  return {
    hasEmpty: trimmed.length !== nonEmpty.length,
    hasDupe: nonEmpty.length !== new Set(nonEmpty).size,
  };
}
