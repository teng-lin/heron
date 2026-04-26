/**
 * Phase 75 (PR-ν) integration check on the salvage prompt store.
 *
 * The codebase does not (yet) carry a React component testing setup —
 * jsdom + @testing-library would pull in a meaningful build dep for a
 * single component test. The PR brief explicitly accepts an
 * integration check on the store flag in lieu of a render assertion.
 *
 * The assertions below cover the visibility predicate the
 * `<SalvageBanner />` component reads:
 *
 *   visible iff promptedThisSession && unfinalizedCount > 0 && !dismissed
 *
 * Each assertion exercises one boundary of that predicate so a future
 * regression that re-introduces the toast-only UX, drops the
 * `dismissed` flag, or stops mirroring the count fails fast in CI.
 *
 * Runs under `bun test` (see `apps/desktop/package.json`); no jsdom
 * dependency is required because Zustand's React store wrapper is
 * usable headlessly via `getState()`/`setState()`.
 */

import { afterEach, describe, expect, test } from "bun:test";

import { useSalvagePromptStore } from "./salvage";

afterEach(() => {
  // Each test exercises a fresh store snapshot. Without the reset,
  // earlier writes would leak into the next test (`bun test` shares
  // the module cache across tests in the same file).
  useSalvagePromptStore.getState().reset();
});

/**
 * Mirrors the **store-driven slice** of the `<SalvageBanner />`
 * visibility predicate. The component additionally suppresses the
 * banner on `/salvage` itself (a `useLocation()` check), but that
 * branch is independent of the store and is exercised by manual
 * smoke-testing rather than via this headless test.
 */
function bannerVisible(state: ReturnType<typeof useSalvagePromptStore.getState>) {
  return (
    state.promptedThisSession &&
    state.unfinalizedCount > 0 &&
    !state.dismissed
  );
}

describe("salvage banner visibility", () => {
  test("salvage_banner_visible_when_unfinalized_present", () => {
    // Initial state: no scan has run, count is zero, not dismissed —
    // banner stays hidden.
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(false);

    // Scan runs and turns up two unfinalized sessions: banner
    // becomes visible.
    useSalvagePromptStore.getState().markPrompted(2);
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(true);
    expect(useSalvagePromptStore.getState().unfinalizedCount).toBe(2);
  });

  test("hidden when scan reports zero unfinalized sessions", () => {
    // The scan running is necessary but not sufficient — a clean
    // launch with no recoverable sessions must NOT render the banner.
    useSalvagePromptStore.getState().markPrompted(0);
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(false);
  });

  test("hidden after the user dismisses it", () => {
    useSalvagePromptStore.getState().markPrompted(3);
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(true);

    useSalvagePromptStore.getState().dismiss();
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(false);
    // The count itself is unchanged — the dismiss is purely a banner
    // affordance, not a write to the recoverable-session inventory.
    expect(useSalvagePromptStore.getState().unfinalizedCount).toBe(3);
  });

  test("hidden after a per-row purge drains the count to zero", () => {
    // Salvage page mirrors per-row purges into the store via
    // `setUnfinalizedCount`. When the last row is purged the banner
    // disappears without the user having to dismiss it.
    useSalvagePromptStore.getState().markPrompted(1);
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(true);

    useSalvagePromptStore.getState().setUnfinalizedCount(0);
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(false);
  });

  test("hidden before the scan has run", () => {
    // Even if the count is non-zero from a stale mutation, the gate
    // refuses to render the banner before `promptedThisSession` flips
    // — protects against a render flash on cold start.
    useSalvagePromptStore.getState().setUnfinalizedCount(5);
    expect(useSalvagePromptStore.getState().promptedThisSession).toBe(false);
    expect(bannerVisible(useSalvagePromptStore.getState())).toBe(false);
  });
});
