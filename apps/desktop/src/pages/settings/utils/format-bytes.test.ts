/**
 * Pure-helper tests for the Settings → Audio disk-usage gauge formatter.
 *
 * The gauge polls every 5s; the format must match macOS Finder's SI
 * (1000-step) reporting so the displayed number doesn't drift between
 * the gauge and the user's external sense of disk usage.
 */

import { describe, expect, test } from "bun:test";

import { formatBytes } from "./format-bytes";

describe("formatBytes", () => {
  test("renders sub-1000 byte counts as raw bytes", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1)).toBe("1 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(999)).toBe("999 B");
  });

  test("steps into KB at 1000 bytes", () => {
    expect(formatBytes(1000)).toBe("1.0 KB");
    expect(formatBytes(1500)).toBe("1.5 KB");
    expect(formatBytes(999_999)).toBe("1000.0 KB");
  });

  test("steps into MB at 1_000_000 bytes", () => {
    expect(formatBytes(1_000_000)).toBe("1.0 MB");
    expect(formatBytes(38_000_000)).toBe("38.0 MB");
  });

  test("steps into GB / TB", () => {
    expect(formatBytes(1_400_000_000)).toBe("1.4 GB");
    expect(formatBytes(2_500_000_000_000)).toBe("2.5 TB");
  });

  test("keeps trailing .0 so the gauge doesn't visually jitter between polls", () => {
    // Anti-regression: dropping `toFixed(1)` would make "1.0 GB" flip
    // to "1 GB" between polls when the value rounds to integer.
    expect(formatBytes(1_000_000_000)).toBe("1.0 GB");
  });

  test("clamps overflow to TB", () => {
    // Values larger than the unit table cap at TB rather than going
    // off-the-end with `undefined`.
    expect(formatBytes(10_000_000_000_000_000)).toBe("10000.0 TB");
  });
});
