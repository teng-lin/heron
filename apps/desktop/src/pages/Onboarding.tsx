/**
 * Onboarding wizard (§13.3 / PR-ι, phase 71; gap #5 added the daemon step).
 *
 * Six-step Test-button walkthrough that exercises the §13.3 probes
 * (mic, audio-tap, accessibility, calendar, WhisperKit-model presence)
 * plus a final daemon-liveness check before the user starts recording.
 * The wizard is one-shot per install — `Finish setup` calls
 * `heron_mark_onboarded`, which the `App.tsx` first-run detector reads
 * to skip the route on subsequent launches.
 *
 * State is held in `store/onboarding.ts` (Zustand) so Back navigation
 * preserves each step's outcome without re-running its probe. The
 * `<TestStatus>` badge is the same component the Settings → Calendar
 * tab uses, so success / failure / permission-prompt copy stays
 * consistent across surfaces.
 *
 * Accessibility:
 * - Top progress dots are presented as an ordered list; each step is
 *   announced with `aria-current="step"` when active.
 * - Test buttons use a `<Loader2>` spinner with `aria-hidden` and the
 *   button's `disabled` state to communicate loading; the latest
 *   outcome below renders via `<TestStatus>` which carries
 *   `role="status" aria-live="polite"`.
 * - Skip-with-confirmation for the mic step uses a Radix Dialog so
 *   focus is trapped + restored on close.
 *
 * Out of scope (per PR-ι spec):
 * - Persisting per-step outcomes across app restarts.
 * - Multi-target-bundle picker beyond the §13.3 hardcoded list of 4.
 * - Real WhisperKit model download — the probe answers "is a model
 *   already on disk?" and surfaces the result; the actual download is
 *   a future phase.
 * - "Re-run onboarding" entry point in Settings — defer.
 */

import { useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  AlertTriangle,
  CheckCircle2,
  Circle,
  Loader2,
  PlayCircle,
} from "lucide-react";
import { toast } from "sonner";

import { Button } from "../components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../components/ui/select";
import { TestStatus } from "../components/TestStatus";
import { cn } from "../lib/cn";
import { invoke } from "../lib/invoke";
import { resolvePostOnboardingDestination } from "../lib/postOnboardingDestination";
import {
  STEPS,
  canAdvance,
  useOnboardingStore,
  type StepId,
} from "../store/onboarding";
import { useSettingsStore } from "../store/settings";

/**
 * Hardcoded list of audio-tap targets the §13.3 wizard surfaces.
 *
 * Per the PR-ι spec, the wizard does **not** own a persistent list —
 * PR-λ (phase 73) wires the Settings → Recorded apps card. The
 * wizard's choice is wizard-local state in
 * `useOnboardingStore.selectedBundle`; flipping it doesn't write
 * anywhere on disk.
 *
 * The four entries follow the §13.3 anchor list: Zoom desktop, Zoom
 * web, Microsoft Teams desktop, Google Chrome (Meet runs in Chrome).
 */
const TAP_TARGETS = [
  { value: "us.zoom.xos", label: "Zoom (desktop)" },
  { value: "us.zoom.us", label: "Zoom (web)" },
  { value: "com.microsoft.teams2", label: "Microsoft Teams" },
  { value: "com.google.Chrome", label: "Google Chrome (Meet)" },
] as const;

/**
 * Per-step copy. Lives in one map (rather than scattered across the
 * step-body components) so a future copy edit lands in one place and
 * Spanish localization is one nested map away from working.
 */
const STEP_COPY: Record<StepId, { title: string; body: string }> = {
  microphone: {
    title: "Microphone",
    body: "heron records your microphone so we hear you. macOS will prompt for Microphone access the first time you click Test.",
  },
  audio_tap: {
    title: "System audio",
    body: "heron taps the meeting app's audio so we hear everyone else, not just you. Pick the app you use most — you can add more in Settings later.",
  },
  accessibility: {
    title: "Accessibility",
    body: "Accessibility lets heron read window titles to label speakers. Optional but recommended — without it, the transcript names everyone \"Speaker 1 / 2 / 3\".",
  },
  calendar: {
    title: "Calendar",
    body: "Optional. heron can pre-fill meeting titles from your Calendar. Skip if you'd rather keep it offline — calendar access is off by default per heron's privacy posture.",
  },
  model_download: {
    title: "Speech-to-text model",
    body: "heron downloads ~1 GB of WhisperKit models for on-device transcription. Connect to wifi if you're on a metered link — the actual download lands in a future release; this Test only checks whether a model is already in place.",
  },
  daemon: {
    title: "Background service",
    body: "heron's local daemon (herond) routes audio, transcripts, and meeting events between the desktop app and the recording pipeline. This step verifies it is reachable on the loopback port the daemon listens on. You cannot skip this one — without the daemon, recording cannot start.",
  },
};

export default function Onboarding() {
  const navigate = useNavigate();
  const { current, steps, setOutcome, setLoading, setSkipped, goPrev, goNext } =
    useOnboardingStore();
  const markOnboardedLocally = useSettingsStore(
    (state) => state.markOnboarded,
  );
  // All hooks must run on every render — keep the dialog/finishing
  // state above the bounds-check guard below.
  const [skipMicConfirmOpen, setSkipMicConfirmOpen] = useState(false);
  const [finishing, setFinishing] = useState(false);

  const stepId = STEPS[current];
  // STEPS is a fixed-size tuple, but TS narrows to `StepId | undefined`
  // when indexed with a `number`. The guard is runtime + type-narrowing
  // belt-and-suspenders; it should never fire in practice (the store
  // clamps `current` to `[0, STEPS.length - 1]`).
  if (stepId === undefined) {
    return null;
  }
  const step = steps[stepId];
  const isFirst = current === 0;
  const isLast = current === STEPS.length - 1;

  const runTest = async () => {
    setLoading(stepId, true);
    try {
      const outcome = await invokeProbe(stepId);
      setOutcome(stepId, outcome);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      // The Rust probes return `TestOutcome` instead of throwing for
      // expected failures (TCC denial, target-app-not-running) — a
      // thrown error here is an IPC / serialization surprise. Surface
      // it as a Fail outcome so the user sees something concrete.
      setOutcome(stepId, {
        status: "fail",
        details: `probe call failed: ${message}`,
      });
    }
  };

  const requestSkip = () => {
    if (stepId === "microphone") {
      // Mic skip is the one with material UX cost (the user's voice
      // won't be transcribed). Confirm before flipping. Other steps
      // skip without a prompt — accessibility / calendar / model are
      // designated "skip-default" by §13.3, and audio-tap skip is a
      // soft cost (transcripts will only carry the user's mic).
      setSkipMicConfirmOpen(true);
      return;
    }
    setSkipped(stepId, true);
  };

  // The dialog is only opened for the mic step (see `requestSkip`),
  // so flipping `microphone` directly is correct — but parameterizing
  // off `stepId` keeps a future regression that opens the dialog for
  // a different step from silently skipping the wrong one.
  const confirmSkip = () => {
    setSkipped(stepId, true);
    setSkipMicConfirmOpen(false);
  };

  const advance = async () => {
    if (!isLast) {
      goNext();
      return;
    }
    // Last step → finish. Persist the flag, then route the user to
    // the home state (or last-note) via the same logic `App.tsx`'s
    // first-run detector uses.
    setFinishing(true);
    try {
      await invoke("heron_mark_onboarded");
      markOnboardedLocally();
      toast.success("Setup complete", {
        description: "You're ready to record. Hit ⌘⇧R to start.",
      });
      // Defer to the shared helper so the post-wizard landing stays
      // in lockstep with `App.tsx`'s `<FirstRunGate>`.
      const dest = await resolvePostOnboardingDestination();
      navigate(dest, { replace: true });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error("Could not save onboarding state", {
        description: message,
      });
      setFinishing(false);
    }
  };

  return (
    <main className="mx-auto flex min-h-screen max-w-2xl flex-col gap-6 p-6">
      <header className="space-y-2">
        <h1 className="text-2xl font-semibold">Set up heron</h1>
        <p className="text-sm text-muted-foreground">
          Six quick checks before your first recording. Each step has a
          Test button — heron only records when you ask it to.
        </p>
      </header>

      <ProgressDots current={current} steps={steps} />

      <section
        className="rounded-lg border border-border bg-background p-6 shadow-sm"
        aria-labelledby="step-title"
      >
        <h2 id="step-title" className="text-lg font-semibold">
          {STEP_COPY[stepId].title}
        </h2>
        <p className="mt-2 text-sm text-muted-foreground">
          {STEP_COPY[stepId].body}
        </p>

        <div className="mt-4 space-y-3">
          {stepId === "audio_tap" && <AudioTapPicker />}
          {stepId === "model_download" && <ModelDownloadPreviewBadge />}

          <div className="flex items-center gap-3">
            <Button
              type="button"
              onClick={() => void runTest()}
              disabled={step.loading}
              variant={step.outcome ? "outline" : "default"}
            >
              {step.loading ? (
                <>
                  <Loader2
                    className="h-4 w-4 animate-spin"
                    aria-hidden="true"
                  />
                  Testing…
                </>
              ) : step.outcome ? (
                <>
                  <PlayCircle className="h-4 w-4" aria-hidden="true" />
                  Re-run test
                </>
              ) : (
                <>
                  <PlayCircle className="h-4 w-4" aria-hidden="true" />
                  Test
                </>
              )}
            </Button>
            {step.skipped && !step.outcome && (
              <span className="flex items-center gap-1.5 text-sm text-muted-foreground">
                <AlertTriangle className="h-4 w-4" aria-hidden="true" />
                Skipped
              </span>
            )}
          </div>

          <TestStatus outcome={step.outcome} />
        </div>
      </section>

      <nav className="flex items-center justify-between gap-2">
        <Button
          type="button"
          variant="outline"
          onClick={goPrev}
          disabled={isFirst}
        >
          Back
        </Button>
        <div className="flex items-center gap-2">
          {stepId !== "daemon" && (
            <Button type="button" variant="ghost" onClick={requestSkip}>
              Skip step
            </Button>
          )}
          <Button
            type="button"
            onClick={() => void advance()}
            disabled={!canAdvance(stepId, step) || finishing}
          >
            {finishing
              ? "Finishing…"
              : isLast
                ? "Finish setup"
                : "Next →"}
          </Button>
        </div>
      </nav>

      <Dialog open={skipMicConfirmOpen} onOpenChange={setSkipMicConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Skip microphone?</DialogTitle>
            <DialogDescription>
              Without microphone access, heron can record the meeting
              app's audio but not your voice. Your speaker turns won't
              appear in the transcript. You can grant access later in
              System Settings → Privacy & Security → Microphone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setSkipMicConfirmOpen(false)}
            >
              Go back
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={confirmSkip}
            >
              Skip anyway
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </main>
  );
}

/**
 * Top-of-page progress indicator. One filled / outlined circle per
 * step, with the active step's circle slightly larger.
 *
 * Accessibility note: the wrapping element is a real `<ol>` so screen
 * readers announce "list of 5 items". The active step is marked with
 * `aria-current="step"`. Visited steps that have an outcome /
 * skipped flag get a `CheckCircle2` rather than the plain dot.
 */
function ProgressDots({
  current,
  steps,
}: {
  current: number;
  steps: ReturnType<typeof useOnboardingStore.getState>["steps"];
}) {
  return (
    <ol className="flex items-center justify-center gap-3" aria-label="Steps">
      {STEPS.map((id, idx) => {
        const isActive = idx === current;
        const isVisited = canAdvance(id, steps[id]);
        const Icon = isVisited ? CheckCircle2 : Circle;
        // Human-readable label rendered as `sr-only` text inside the
        // <li> rather than only as `aria-label`. VoiceOver on macOS
        // reads `aria-label` reliably on most elements, but having
        // real text content keeps the announcement consistent across
        // assistive tools (NVDA, JAWS, Orca) and survives a future
        // CSS reset that strips ARIA-only labels.
        const stepName = id.replace("_", " ");
        const label = `Step ${idx + 1}: ${stepName}${isVisited ? " (done)" : isActive ? " (current)" : ""}`;
        return (
          <li
            key={id}
            aria-current={isActive ? "step" : undefined}
            className="flex items-center"
          >
            <Icon
              className={cn(
                "h-5 w-5 transition-colors",
                isVisited
                  ? "text-emerald-600 dark:text-emerald-400"
                  : isActive
                    ? "text-primary"
                    : "text-muted-foreground/50",
                isActive && "h-6 w-6",
              )}
              aria-hidden="true"
            />
            <span className="sr-only">{label}</span>
          </li>
        );
      })}
    </ol>
  );
}

/**
 * Step-2 dropdown for picking the audio-tap target bundle ID.
 *
 * Wired directly to `useOnboardingStore.selectedBundle` so the
 * `runTest()` closure in `<Onboarding>` reads the latest value at
 * call time without prop-threading through a controlled component.
 */
function AudioTapPicker() {
  const selected = useOnboardingStore((state) => state.selectedBundle);
  const setSelected = useOnboardingStore((state) => state.setSelectedBundle);
  return (
    <div className="space-y-1.5">
      <label
        htmlFor="audio-tap-target"
        className="text-sm font-medium text-foreground"
      >
        Target meeting app
      </label>
      <Select value={selected} onValueChange={setSelected}>
        <SelectTrigger id="audio-tap-target" className="w-full max-w-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {TAP_TARGETS.map((opt) => (
            <SelectItem key={opt.value} value={opt.value}>
              {opt.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}

/**
 * Inline "preview" pill on step 5 that reminds the reader the model
 * download is not yet wired through the orchestrator. Without this,
 * a Pass result on the probe would mislead the user into thinking
 * the wizard installed the model.
 */
function ModelDownloadPreviewBadge() {
  // TODO(phase 72+): wire `heron_download_model` and replace this
  // static notice with a real progress bar driven by streamed events.
  return (
    <p className="rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-900 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-100">
      <strong>Preview.</strong> The Test button checks whether a model
      is already on disk; it does <em>not</em> trigger a download.
      Real model fetching ships in a follow-up phase.
    </p>
  );
}

/**
 * Map a `StepId` to its `invoke(...)` call. Lives outside the
 * component because the per-step args (specifically the audio-tap's
 * `targetBundleId`) need to read fresh state at call time, not the
 * value captured at render time.
 */
async function invokeProbe(step: StepId) {
  switch (step) {
    case "microphone":
      return invoke("heron_test_microphone");
    case "audio_tap": {
      const bundle = useOnboardingStore.getState().selectedBundle;
      return invoke("heron_test_audio_tap", { targetBundleId: bundle });
    }
    case "accessibility":
      return invoke("heron_test_accessibility");
    case "calendar":
      return invoke("heron_test_calendar");
    case "model_download":
      return invoke("heron_test_model_download");
    case "daemon":
      return invoke("heron_test_daemon");
  }
}
