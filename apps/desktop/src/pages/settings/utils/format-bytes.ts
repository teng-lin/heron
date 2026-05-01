/**
 * Humanize a byte count for the Settings → Audio disk-usage gauge.
 *
 * SI units (1000-step), not IEC, so the displayed number matches what
 * macOS Finder reports. Returns e.g. `"1.4 GB"` / `"38 MB"` / `"512 B"`.
 *
 * `toFixed(1)` keeps the trailing `.0` so "1.0 GB" doesn't visually
 * jitter into "1 GB" between polls.
 */
export function formatBytes(bytes: number): string {
  if (bytes < 1000) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"] as const;
  let value = bytes;
  let unit = -1;
  do {
    value /= 1000;
    unit += 1;
  } while (value >= 1000 && unit < units.length - 1);
  return `${value.toFixed(1)} ${units[unit] ?? "TB"}`;
}
