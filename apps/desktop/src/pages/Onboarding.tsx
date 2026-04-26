/**
 * Onboarding wizard (§13.3 / PR-ι, phase 71; gap #5 added the daemon
 * step; gap #5b wired the real WhisperKit download; gap #6 added the
 * runtime-checks step).
 *
 * Test-button walkthrough that exercises the §13.3 probes (mic,
 * audio-tap, accessibility, calendar, WhisperKit model fetch), then
 * the doctor's environment sweep (ONNX / Zoom / keychain ACL /
 * network), then a final daemon-liveness check before the user starts
 * recording. The wizard is one-shot per install — `Finish setup` calls
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
 * - "Re-run onboarding" entry point in Settings — defer.
 */

import { useEffect, useState } from "react";
import {
  RuntimeChecksPanel,
  aggregateSeverity,
  summariseEntries,
  type RuntimeChecksLoad,
} from "../components/RuntimeChecksPanel";
import { useNavigate } from "react-router-dom";
import {
  AlertTriangle,
  CheckCircle2,
  Circle,
  Loader2,
  PlayCircle,
} from "lucide-react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
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
import {
  invoke,
  type ModelDownloadProgress,
  type TestOutcome,
} from "../lib/invoke";
import { resolvePostOnboardingDestination } from "../lib/postOnboardingDestination";
import {
  STEPS,
  canAdvance,
  useOnboardingStore,
  type StepId,
} from "../store/onboarding";
import { useSettingsStore } from "../store/settings";

/** Tauri event name `heron_download_model` emits 0..1 progress on. */
const MODEL_DOWNLOAD_PROGRESS_EVENT = "model_download:progress";

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
    body: "heron downloads ~1 GB of WhisperKit models for on-device transcription. Connect to wifi if you're on a metered link — clicking Download fetches the model into the system cache and reports progress as it goes. On non-Apple builds, this step can be skipped.",
  },
  runtime_checks: {
    title: "Runtime checks",
    body: "One last environment sweep: ONNX models on disk, Zoom availability, the macOS keychain ACL, and network reachability. Permissions are covered by the earlier steps; this is the consolidated \"is the rest of the machine ready?\" answer from heron-doctor.",
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
  // Latest 0..1 fraction reported on `model_download:progress`. We keep
  // it as a separate piece of state (instead of stuffing it into the
  // store) because progress is volatile UI feedback, not wizard
  // outcome — it resets every time the user clicks Download again and
  // doesn't survive Back/Next navigation.
  const [modelProgress, setModelProgress] = useState<number | null>(null);
  // Runtime-check step (gap #6) renders a list of doctor entries
  // rather than a single `TestOutcome`. We track that list in
  // page-local state for the same reason `modelProgress` lives here:
  // wire-shape state that's volatile across the wizard's one-shot
  // lifetime doesn't belong in the Zustand store.
  const [runtimeChecksLoad, setRuntimeChecksLoad] =
    useState<RuntimeChecksLoad>({ kind: "idle" });

  // Lifecycle-bound listener for `model_download:progress` ticks.
  // Registering inside the component (rather than inside the
  // `runModelDownload` async helper) makes the cleanup deterministic:
  // a user who navigates away mid-download triggers `unlisten()`
  // synchronously, and a user who returns gets a fresh `setModelProgress`
  // setter wired to the live component instance instead of a stale one
  // captured in a long-lived background promise.
  useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    void (async () => {
      const u = await listen<ModelDownloadProgress>(
        MODEL_DOWNLOAD_PROGRESS_EVENT,
        (event) => {
          const { fraction } = event.payload;
          if (typeof fraction === "number" && Number.isFinite(fraction)) {
            setModelProgress(fraction);
          }
        },
      );
      if (cancelled) {
        u();
        return;
      }
      unlisten = u;
    })();
    return () => {
      cancelled = true;
      if (unlisten) {
        unlisten();
      }
    };
  }, []);

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
    if (stepId === "runtime_checks") {
      await runRuntimeChecks();
      return;
    }
    setLoading(stepId, true);
    try {
      // Step 5 (model_download) has its own code path: instead of a
      // probe that returns `TestOutcome`, we invoke the real
      // `heron_download_model` command, listen on the progress event,
      // and synthesize a TestOutcome from the resolved/rejected
      // promise. Splitting here keeps `invokeProbe` honest (every
      // other entry truly is a `TestOutcome`-returning probe) without
      // introducing a discriminated-return type that all five other
      // call sites would have to switch over.
      if (stepId === "model_download") {
        // Reset to 0 so a re-run starts the bar at empty rather than
        // stuck at the previous run's terminal value.
        setModelProgress(0);
        const outcome = await runModelDownload();
        setOutcome(stepId, outcome);
        return;
      }
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

  /**
   * Gap #6: invoke `heron_run_runtime_checks` and surface the
   * consolidated entry list. We also synthesize a `TestOutcome` so
   * the standard `canAdvance(step)` selector lights up the Next /
   * Finish button without forking the store predicate for one step.
   *
   * The synthesized outcome's `status` reflects the *blocking*
   * severity only: any `fail` entry → `fail`; otherwise `pass`. A
   * lone `warn` (e.g. Zoom not currently running) keeps the
   * headline badge green so it does not contradict the per-row
   * panel below — the warning is still visible there and is named
   * in the `details` line ("3 OK, 1 warning"). Without this rule
   * the headline shows red "Failed" while the panel shows amber,
   * which reads as a wizard bug.
   */
  const runRuntimeChecks = async () => {
    if (runtimeChecksLoad.kind === "loading") {
      // Defense-in-depth against a double-click slipping past the
      // button's `disabled={step.loading}` gate (e.g. via keyboard).
      return;
    }
    setLoading("runtime_checks", true);
    setRuntimeChecksLoad({ kind: "loading" });
    try {
      const entries = await invoke("heron_run_runtime_checks");
      setRuntimeChecksLoad({ kind: "ready", entries });
      const worst = aggregateSeverity(entries);
      const detailsLine = summariseEntries(entries);
      setOutcome("runtime_checks", {
        status: worst === "fail" ? "fail" : "pass",
        details: detailsLine,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setRuntimeChecksLoad({ kind: "error", message });
      setOutcome("runtime_checks", {
        status: "fail",
        details: `doctor call failed: ${message}`,
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
          A few quick checks before your first recording. Each step has a
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
          {stepId === "model_download" &&
            (step.loading || modelProgress !== null) && (
              <ModelDownloadProgressBar fraction={modelProgress} />
            )}

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
                  {stepId === "model_download" ? "Downloading…" : "Testing…"}
                </>
              ) : step.outcome ? (
                <>
                  <PlayCircle className="h-4 w-4" aria-hidden="true" />
                  {stepId === "model_download" ? "Re-download" : "Re-run test"}
                </>
              ) : (
                <>
                  <PlayCircle className="h-4 w-4" aria-hidden="true" />
                  {stepId === "model_download" ? "Download" : "Test"}
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

          {stepId === "runtime_checks" && (
            <RuntimeChecksPanel load={runtimeChecksLoad} />
          )}
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
 * readers announce "list of N items". The active step is marked with
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
 * Live progress bar rendered while `heron_download_model` is in
 * flight. `fraction` is the latest `[0, 1]` ratio reported on the
 * `model_download:progress` Tauri event, or `null` before the first
 * tick lands.
 *
 * The bar is `<progress>` — a real, native, accessible element — so
 * screen readers announce "halfway done" without us having to thread
 * `aria-valuenow` ourselves. WhisperKit's bridge fires the first
 * 0.0 tick immediately, so the indeterminate fallback (when
 * `fraction === null`) typically only renders for a few ms during
 * the IPC handshake; we still emit it so the user sees motion the
 * instant they click Download.
 */
function ModelDownloadProgressBar({ fraction }: { fraction: number | null }) {
  const percent =
    fraction === null ? null : Math.round(Math.max(0, Math.min(1, fraction)) * 100);
  return (
    <div className="space-y-1.5" aria-live="polite">
      {percent === null ? (
        <progress className="w-full" aria-label="Model download progress" />
      ) : (
        <progress
          className="w-full"
          value={percent}
          max={100}
          aria-label="Model download progress"
        />
      )}
      <p className="text-xs text-muted-foreground">
        {percent === null
          ? "Starting download…"
          : `Downloading WhisperKit model… ${percent}%`}
      </p>
    </div>
  );
}

/**
 * Invoke `heron_download_model` and synthesize a `TestOutcome` from
 * the resolved/rejected promise. Progress ticks land on the
 * `model_download:progress` Tauri event, which the parent
 * `<Onboarding>` component subscribes to in a `useEffect` (so the
 * subscription is bound to the React lifecycle rather than to this
 * promise's lifetime).
 *
 * On success we render the backend's "ready" message; on failure we
 * surface the stringified `SttError` directly, matching the per-error
 * copy in `model_download::classify_ensure_model_result`.
 */
async function runModelDownload(): Promise<TestOutcome> {
  try {
    const message = await invoke("heron_download_model");
    return { status: "pass", details: message };
  } catch (err) {
    const details = err instanceof Error ? err.message : String(err);
    return { status: "fail", details };
  }
}

/**
 * Map a `StepId` to its `invoke(...)` call. Lives outside the
 * component because the per-step args (specifically the audio-tap's
 * `targetBundleId`) need to read fresh state at call time, not the
 * value captured at render time.
 *
 * Two steps are intentionally absent:
 *
 * - `model_download` (gap #3) is wired to the real
 *   `heron_download_model` command, which is a long-running download
 *   with progress events rather than a sub-second probe; the
 *   `runModelDownload` helper above owns its call site.
 * - `runtime_checks` (gap #6) returns a list of `RuntimeCheckEntry`
 *   rather than a single `TestOutcome` — the page handles that shape
 *   directly via `runRuntimeChecks`.
 */
async function invokeProbe(
  step: Exclude<StepId, "model_download" | "runtime_checks">,
) {
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
    case "daemon":
      return invoke("heron_test_daemon");
  }
}

