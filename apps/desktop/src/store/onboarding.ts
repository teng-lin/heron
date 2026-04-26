/**
 * Wizard-local state for the §13.3 onboarding flow (PR-ι / phase 71).
 *
 * Why a Zustand store instead of `useState` inside the page?
 *
 * - The wizard's Back button navigates between steps **without
 *   re-running** any prior step's Test probe. The simplest way to
 *   make that work is to keep the per-step outcome + skip flag
 *   outside the route component so each step picks up where it left
 *   off. Lifting state into a parent component would also work, but
 *   the page already has enough complexity (six step bodies + buttons)
 *   that pulling step state out keeps the page focused on layout.
 *
 * - The "Next" button enabledness depends on whether the step has
 *   been tested OR explicitly skipped (with the daemon step demanding
 *   a real pass — see `canAdvance` below). Encoding that predicate in
 *   a selector (`canAdvance(stepId, step)`) keeps the page free of
 *   branching per-step logic.
 *
 * The store is **wizard-local**: it holds in-memory state only, does
 * not persist across app restarts, and is reset on `Finish setup`.
 * Persistence of test outcomes to disk (so a future re-run of the
 * wizard remembered last time's result) is explicitly out of scope —
 * the wizard is one-shot per install.
 */

import { create } from "zustand";

import type { TestOutcome } from "../lib/invoke";

/**
 * The six steps in the §13.3 wizard.
 *
 * Numbered from 1 to match the §13.3 spec (and the user-visible
 * progress dots) instead of 0-indexed. The page consumes this enum
 * via `STEPS` for ordering, never spelled out as a literal.
 *
 * `daemon` (gap #5) is the final gate: every preceding step exercises
 * a TCC permission or a bundled-asset probe that the user can grant
 * out-of-band, but the in-process `herond` is what actually drives
 * capture/transcription, so it goes last so the wizard's "Finish
 * setup" button is only reachable when the daemon is verifiably up.
 */
export type StepId =
  | "microphone"
  | "audio_tap"
  | "accessibility"
  | "calendar"
  | "model_download"
  | "daemon";

export const STEPS: readonly StepId[] = [
  "microphone",
  "audio_tap",
  "accessibility",
  "calendar",
  "model_download",
  "daemon",
] as const;

interface StepState {
  /** Latest outcome from the step's probe, or `null` if it hasn't run. */
  outcome: TestOutcome | null;
  /**
   * `true` if the user explicitly chose to skip this step. The skip
   * flag and a non-null outcome are independent — a step can be
   * skipped after a Test was run (the user reviewed the result and
   * decided to skip anyway), and the Next button enables on either.
   */
  skipped: boolean;
  /** True while a Test invocation is in flight for this step. */
  loading: boolean;
}

interface OnboardingState {
  /** Index into `STEPS` (0..STEPS.length). */
  current: number;
  /** Per-step outcome / skip / loading flags. Keyed by `StepId`. */
  steps: Record<StepId, StepState>;
  /**
   * The bundle ID currently selected in the audio-tap step's
   * dropdown. PR-λ (phase 73) replaces this with a persistent list;
   * the wizard keeps it as wizard-local state for now.
   */
  selectedBundle: string;
  setOutcome: (id: StepId, outcome: TestOutcome) => void;
  setLoading: (id: StepId, loading: boolean) => void;
  setSkipped: (id: StepId, skipped: boolean) => void;
  setSelectedBundle: (bundle: string) => void;
  goPrev: () => void;
  goNext: () => void;
  reset: () => void;
}

/**
 * Default-construct a fresh per-step state. Helper so `reset()` and
 * the initial store value share one source of truth.
 */
function freshStepState(): StepState {
  return { outcome: null, skipped: false, loading: false };
}

function freshSteps(): Record<StepId, StepState> {
  return {
    microphone: freshStepState(),
    audio_tap: freshStepState(),
    accessibility: freshStepState(),
    calendar: freshStepState(),
    model_download: freshStepState(),
    daemon: freshStepState(),
  };
}

/** Default audio-tap target on first launch — Zoom is the §13.3 anchor app. */
const DEFAULT_BUNDLE_ID = "us.zoom.xos";

export const useOnboardingStore = create<OnboardingState>((set) => ({
  current: 0,
  steps: freshSteps(),
  selectedBundle: DEFAULT_BUNDLE_ID,
  setOutcome: (id, outcome) =>
    set((state) => ({
      steps: {
        ...state.steps,
        [id]: { ...state.steps[id], outcome, loading: false },
      },
    })),
  setLoading: (id, loading) =>
    set((state) => ({
      steps: { ...state.steps, [id]: { ...state.steps[id], loading } },
    })),
  setSkipped: (id, skipped) =>
    set((state) => ({
      steps: { ...state.steps, [id]: { ...state.steps[id], skipped } },
    })),
  setSelectedBundle: (bundle) => set({ selectedBundle: bundle }),
  goPrev: () =>
    set((state) => ({ current: Math.max(0, state.current - 1) })),
  goNext: () =>
    set((state) => ({
      current: Math.min(STEPS.length - 1, state.current + 1),
    })),
  reset: () => {
    // Used by the wizard's "Finish setup" path so reopening the
    // wizard (e.g. via a future Settings → Re-run) starts clean.
    set({
      current: 0,
      steps: freshSteps(),
      selectedBundle: DEFAULT_BUNDLE_ID,
    });
  },
}));

/**
 * Selector: `true` iff the wizard's Next/Finish button should be
 * enabled for the current step.
 *
 * For the original five §13.3 / PR-ι steps the rule is permissive:
 * enabled when the probe has run at least once OR the step has been
 * explicitly skipped — a `fail` / `needs_permission` outcome still
 * satisfies "tested" so the user can advance and fix the underlying
 * permission from System Settings without losing wizard progress.
 *
 * The `daemon` step (gap #5) is the exception: the wizard cannot
 * meaningfully complete unless the in-process `herond` is reachable,
 * so this selector requires a `pass` outcome and ignores the skip
 * flag. The page mirrors that policy by hiding Skip on this step.
 */
export function canAdvance(stepId: StepId, step: StepState): boolean {
  if (stepId === "daemon") {
    return step.outcome?.status === "pass";
  }
  return step.outcome !== null || step.skipped;
}
