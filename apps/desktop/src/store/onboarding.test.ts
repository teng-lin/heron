/**
 * Gap #5: store + selector contract for the onboarding wizard.
 *
 * Mirrors the headless Zustand pattern `salvage.test.ts` established —
 * `bun test` with no jsdom dependency, exercising the store via
 * `getState()` / typed selectors.
 *
 * The assertions pin the gap-#5 invariants the wizard relies on:
 *
 *   - `daemon` is part of the canonical step list, so a future
 *     refactor cannot silently drop it back to the original five.
 *   - `canAdvance` for the daemon step requires a `pass` outcome and
 *     ignores the skip flag, so a `fail` / `needs_permission` cannot
 *     gate the user past Finish setup.
 *   - The other five steps keep the original permissive predicate
 *     (any outcome OR skipped advances the wizard).
 *   - `freshSteps()` includes a per-step entry for `daemon`, so a
 *     `reset()` mid-wizard does not crash on a missing key.
 */

import { afterEach, describe, expect, test } from "bun:test";

import { STEPS, canAdvance, useOnboardingStore } from "./onboarding";

afterEach(() => {
  useOnboardingStore.getState().reset();
});

describe("onboarding step list", () => {
  test("includes the daemon liveness step at the end", () => {
    expect(STEPS).toEqual([
      "microphone",
      "audio_tap",
      "accessibility",
      "calendar",
      "model_download",
      "daemon",
    ]);
  });

  test("freshSteps allocates state for every step including daemon", () => {
    const state = useOnboardingStore.getState();
    expect(state.steps.daemon).toEqual({
      outcome: null,
      skipped: false,
      loading: false,
    });
  });
});

describe("canAdvance — daemon step gating (gap #5)", () => {
  test("blocks until the probe runs", () => {
    const state = useOnboardingStore.getState();
    expect(canAdvance("daemon", state.steps.daemon)).toBe(false);
  });

  test("blocks on a fail outcome — no permissive escape hatch", () => {
    useOnboardingStore.getState().setOutcome("daemon", {
      status: "fail",
      details: "herond not reachable at 127.0.0.1:7384 (connection refused)",
    });
    const step = useOnboardingStore.getState().steps.daemon;
    expect(canAdvance("daemon", step)).toBe(false);
  });

  test("blocks on a needs_permission outcome", () => {
    useOnboardingStore.getState().setOutcome("daemon", {
      status: "needs_permission",
      details: "auth token missing",
    });
    const step = useOnboardingStore.getState().steps.daemon;
    expect(canAdvance("daemon", step)).toBe(false);
  });

  test("ignores the skip flag — daemon cannot be bypassed", () => {
    useOnboardingStore.getState().setSkipped("daemon", true);
    const step = useOnboardingStore.getState().steps.daemon;
    expect(canAdvance("daemon", step)).toBe(false);
  });

  test("unblocks only on a pass outcome", () => {
    useOnboardingStore.getState().setOutcome("daemon", {
      status: "pass",
      details: "herond v0.1.0 responding at /v1/health",
    });
    const step = useOnboardingStore.getState().steps.daemon;
    expect(canAdvance("daemon", step)).toBe(true);
  });
});

describe("canAdvance — original five steps stay permissive", () => {
  test("any outcome (including fail) advances the mic step", () => {
    useOnboardingStore.getState().setOutcome("microphone", {
      status: "fail",
      details: "TCC denied",
    });
    const step = useOnboardingStore.getState().steps.microphone;
    expect(canAdvance("microphone", step)).toBe(true);
  });

  test("explicit skip advances the calendar step", () => {
    useOnboardingStore.getState().setSkipped("calendar", true);
    const step = useOnboardingStore.getState().steps.calendar;
    expect(canAdvance("calendar", step)).toBe(true);
  });

  test("untouched step stays gated", () => {
    const step = useOnboardingStore.getState().steps.audio_tap;
    expect(canAdvance("audio_tap", step)).toBe(false);
  });
});
