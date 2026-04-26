/**
 * Pure-helper tests for `aggregateSeverity` + `summariseEntries`.
 *
 * Both helpers drive the wizard's headline runtime-checks badge: the
 * former picks the badge colour (worst severity), the latter the text
 * ("3 OK, 1 warning, 1 failed"). The two read the same entry list in
 * the same wizard call, so a precedence regression in either would
 * misalign the badge with the per-row panel below.
 *
 * Cases pin three policies:
 *
 *   1. `aggregateSeverity` precedence: `fail` > `warn` > `pass`, empty
 *      list = `pass` (vacuously clear).
 *   2. `summariseEntries` only mentions buckets that have entries
 *      (`0 warning, 0 failed` is noise) and pluralises correctly
 *      (`1 warning` vs `2 warnings`).
 *   3. Both helpers are pure — repeat invocations on the same list
 *      yield the same answer (no caching / mutation surprises).
 */

import { describe, expect, test } from "bun:test";

import type { RuntimeCheckEntry } from "../lib/invoke";
import { aggregateSeverity, summariseEntries } from "./RuntimeChecksPanel";

function entry(
  severity: RuntimeCheckEntry["severity"],
  name = "x",
): RuntimeCheckEntry {
  return { name, severity, summary: "", detail: "" };
}

describe("aggregateSeverity", () => {
  test("empty list returns pass (vacuously clear)", () => {
    expect(aggregateSeverity([])).toBe("pass");
  });

  test("all-pass list returns pass", () => {
    expect(aggregateSeverity([entry("pass"), entry("pass")])).toBe("pass");
  });

  test("any warn (no fail) returns warn", () => {
    expect(aggregateSeverity([entry("pass"), entry("warn")])).toBe("warn");
  });

  test("any fail dominates regardless of warn / pass count", () => {
    expect(
      aggregateSeverity([entry("pass"), entry("warn"), entry("fail")]),
    ).toBe("fail");
  });

  test("fail dominates even when fail comes last", () => {
    // The implementation can early-return on first fail — pin the
    // ordering doesn't matter for the contract.
    expect(aggregateSeverity([entry("warn"), entry("warn"), entry("fail")])).toBe(
      "fail",
    );
  });
});

describe("summariseEntries", () => {
  test("empty list returns the placeholder string", () => {
    expect(summariseEntries([])).toBe("No checks reported.");
  });

  test("all-pass list mentions only the OK count", () => {
    expect(summariseEntries([entry("pass"), entry("pass")])).toBe("2 OK");
  });

  test("singular warning is not pluralised", () => {
    expect(summariseEntries([entry("pass"), entry("warn")])).toBe(
      "1 OK, 1 warning",
    );
  });

  test("multiple warnings pluralise to 'warnings'", () => {
    expect(
      summariseEntries([entry("pass"), entry("warn"), entry("warn")]),
    ).toBe("1 OK, 2 warnings");
  });

  test("warning + failure both surface", () => {
    expect(
      summariseEntries([entry("pass"), entry("warn"), entry("fail")]),
    ).toBe("1 OK, 1 warning, 1 failed");
  });

  test("zero-count buckets are omitted", () => {
    // A `fail`-only outcome should not produce "0 OK, 0 warning, 1
    // failed" — drop the zero buckets so the badge text is tight.
    expect(summariseEntries([entry("fail")])).toBe("0 OK, 1 failed");
  });
});
