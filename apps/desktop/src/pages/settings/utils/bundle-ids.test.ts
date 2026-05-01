/**
 * Pure-helper tests for the Settings → Audio "Recorded apps" card
 * validator. The card promotes edits to the autosave path only when
 * `validateBundleIds` returns clean — pinning the predicate here so
 * a future maintainer can't accidentally relax it (which would persist
 * an empty / duplicated target list to disk).
 */

import { describe, expect, test } from "bun:test";

import { validateBundleIds } from "./bundle-ids";

describe("validateBundleIds", () => {
  test("clean list reports neither flag", () => {
    expect(
      validateBundleIds(["us.zoom.xos", "com.microsoft.teams2"]),
    ).toEqual({ hasEmpty: false, hasDupe: false });
  });

  test("flags blank rows as empty", () => {
    expect(validateBundleIds(["us.zoom.xos", ""])).toEqual({
      hasEmpty: true,
      hasDupe: false,
    });
  });

  test("flags whitespace-only rows as empty", () => {
    expect(validateBundleIds(["us.zoom.xos", "   "])).toEqual({
      hasEmpty: true,
      hasDupe: false,
    });
  });

  test("flags duplicate trimmed rows as duplicates", () => {
    expect(
      validateBundleIds(["us.zoom.xos", " us.zoom.xos "]),
    ).toEqual({ hasEmpty: false, hasDupe: true });
  });

  test("excludes empty rows from the duplicate check", () => {
    // Two empty rows are flagged via `hasEmpty` only — duplicating
    // empty strings shouldn't double-fire `hasDupe`.
    expect(validateBundleIds(["", ""])).toEqual({
      hasEmpty: true,
      hasDupe: false,
    });
  });

  test("empty list is clean", () => {
    expect(validateBundleIds([])).toEqual({
      hasEmpty: false,
      hasDupe: false,
    });
  });
});
