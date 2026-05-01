/**
 * Pure-helper tests for the Settings → Hotkey chord-capture key
 * normalizer. The Rust side registers the chord verbatim with
 * `tauri-plugin-global-shortcut`, so a regression in the JS-side
 * spelling is a registration failure at runtime.
 */

import { describe, expect, test } from "bun:test";

import { normalizeKey } from "./keys";

describe("normalizeKey", () => {
  test("uppercases single-character keys", () => {
    expect(normalizeKey("r")).toBe("R");
    expect(normalizeKey("R")).toBe("R");
    expect(normalizeKey("1")).toBe("1");
  });

  test("maps Space literal to the Tauri spelling", () => {
    // Anti-regression: `" ".length === 1` would otherwise route through
    // the uppercase fast path and emit a literal space — which Tauri's
    // parser does not accept.
    expect(normalizeKey(" ")).toBe("Space");
  });

  test("maps arrow keys to their Tauri short names", () => {
    expect(normalizeKey("ArrowLeft")).toBe("Left");
    expect(normalizeKey("ArrowRight")).toBe("Right");
    expect(normalizeKey("ArrowUp")).toBe("Up");
    expect(normalizeKey("ArrowDown")).toBe("Down");
  });

  test("passes through multi-character keys verbatim", () => {
    // F-row, Enter, Tab, etc. ride through unchanged — the browser's
    // `key` value already matches the Tauri spelling.
    expect(normalizeKey("F1")).toBe("F1");
    expect(normalizeKey("Enter")).toBe("Enter");
    expect(normalizeKey("Tab")).toBe("Tab");
    expect(normalizeKey("Escape")).toBe("Escape");
  });
});
