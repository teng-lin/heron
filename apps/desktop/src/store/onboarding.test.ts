/**
 * Store + selector contract for the onboarding wizard.
 *
 * Mirrors the headless Zustand pattern `salvage.test.ts` established —
 * `bun test` with no jsdom dependency, exercising the store via
 * `getState()` / typed selectors.
 *
 * The assertions pin two gap-specific invariants the wizard relies on:
 *
 *   - Gap #5 — `daemon` is the final step. `canAdvance` for it
 *     requires a `pass` outcome and ignores the skip flag, so a
 *     `fail` / `needs_permission` cannot gate the user past Finish
 *     setup.
 *   - Gap #6 — `runtime_checks` sits between the original five and
 *     `daemon`. It uses the same permissive predicate as the original
 *     five (any outcome OR skipped advances), so a doctor `fail`
 *     doesn't trap the user mid-wizard.
 *
 * Plus the original five §13.3 steps keep the permissive predicate
 * (any outcome OR skipped advances the wizard), and `freshSteps()`
 * seeds an entry for every `StepId` so a `reset()` mid-wizard does
 * not crash on a missing key.
 */

import { afterEach, describe, expect, test } from "bun:test";

import {
  STEPS,
  canAdvance,
  useOnboardingStore,
  type StepId,
} from "./onboarding";

afterEach(() => {
  useOnboardingStore.getState().reset();
});

describe("onboarding step list", () => {
  test("runs the original five, then runtime_checks, then daemon", () => {
    expect(STEPS).toEqual([
      "microphone",
      "audio_tap",
      "accessibility",
      "calendar",
      "model_download",
      "runtime_checks",
      "daemon",
    ]);
  });

  test("daemon is the final step (gap #5 — Finish gate)", () => {
    expect(STEPS[STEPS.length - 1]).toBe("daemon");
  });

  test("runtime_checks sits between the per-capability probes and daemon", () => {
    const idx = STEPS.indexOf("runtime_checks");
    expect(idx).toBe(STEPS.indexOf("model_download") + 1);
    expect(idx).toBe(STEPS.indexOf("daemon") - 1);
  });

  test("freshSteps allocates state for every StepId", () => {
    const { steps } = useOnboardingStore.getState();
    for (const id of STEPS) {
      const s = steps[id satisfies StepId];
      expect(s).toBeDefined();
      expect(s.outcome).toBeNull();
      expect(s.skipped).toBe(false);
      expect(s.loading).toBe(false);
    }
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

describe("canAdvance — runtime_checks step is permissive (gap #6)", () => {
  test("blocks until the doctor has been invoked", () => {
    const step = useOnboardingStore.getState().steps.runtime_checks;
    expect(canAdvance("runtime_checks", step)).toBe(false);
  });

  test("a fail outcome still advances — the user can revisit later", () => {
    // The page sets `status: "fail"` whenever the doctor reports any
    // warn-or-fail entry. That should still satisfy "tested at least
    // once" so the user isn't trapped mid-wizard on a missing model
    // / Zoom binary they can install in parallel.
    useOnboardingStore.getState().setOutcome("runtime_checks", {
      status: "fail",
      details: "1 failed",
    });
    const step = useOnboardingStore.getState().steps.runtime_checks;
    expect(canAdvance("runtime_checks", step)).toBe(true);
  });

  test("explicit skip advances the runtime_checks step", () => {
    useOnboardingStore.getState().setSkipped("runtime_checks", true);
    const step = useOnboardingStore.getState().steps.runtime_checks;
    expect(canAdvance("runtime_checks", step)).toBe(true);
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
