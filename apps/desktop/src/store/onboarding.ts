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
 *   the page already has enough complexity (5 step bodies + buttons)
 *   that pulling step state out keeps the page focused on layout.
 *
 * - The "Next" button enabledness depends on whether the step has
 *   been tested OR explicitly skipped. Encoding that predicate in a
 *   selector (`canAdvance(step)`) keeps the page free of branching
 *   per-step logic.
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
 * The five steps in the §13.3 wizard.
 *
 * Numbered from 1 to match the §13.3 spec (and the user-visible
 * progress dots) instead of 0-indexed. The page consumes this enum
 * via `STEPS` for ordering, never spelled out as a literal.
 */
export type StepId =
  | "microphone"
  | "audio_tap"
  | "accessibility"
  | "calendar"
  | "model_download";

export const STEPS: readonly StepId[] = [
  "microphone",
  "audio_tap",
  "accessibility",
  "calendar",
  "model_download",
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
 * Rule per §13.3 / PR-ι spec: enabled when the step's probe has been
 * run at least once OR the step has been explicitly skipped. The
 * outcome's status is **not** consulted — a `fail` / `needs_permission`
 * outcome still satisfies "tested" so the user can advance and try to
 * fix the issue from inside System Settings without losing wizard
 * progress. The user can always click Skip if they want to bypass
 * altogether.
 */
export function canAdvance(step: StepState): boolean {
  return step.outcome !== null || step.skipped;
}
