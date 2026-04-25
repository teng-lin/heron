/**
 * Inline result badge for the onboarding/settings probe Test buttons.
 *
 * Mirrors the vexa pattern: each Test button records the latest
 * [`TestOutcome`] it received, and a paired `<TestStatus>` renders the
 * outcome with a status icon + the probe's `details` string.
 *
 * The four [`TestOutcome`] variants map to three icons:
 * - `pass`             → `CheckCircle2` (green)
 * - `fail`             → `XCircle` (destructive red)
 * - `needs_permission` → `AlertTriangle` (amber)
 * - `skipped`          → `AlertTriangle` (muted) — the probe ran but
 *   the platform precondition wasn't met (e.g. macOS-only paths off
 *   Apple), so it's neither a hard failure nor a clean pass.
 *
 * `outcome={null}` renders nothing — callers can pass the latest
 * outcome state without an extra `outcome &&` guard.
 *
 * PR-δ wires this into the Calendar tab only. The remaining four
 * onboarding probes (mic, audio-tap, accessibility, model-download)
 * adopt this same component when their respective tabs ship.
 */

import { AlertTriangle, CheckCircle2, XCircle } from "lucide-react";

import { cn } from "../lib/cn";
import type { TestOutcome } from "../lib/invoke";

export interface TestStatusProps {
  /** Latest outcome from the probe, or `null` if it hasn't run yet. */
  outcome: TestOutcome | null;
  className?: string;
}

export function TestStatus({ outcome, className }: TestStatusProps) {
  if (outcome === null) {
    return null;
  }

  const { Icon, tone, label } = pickPresentation(outcome.status);

  return (
    <div
      className={cn(
        "flex items-start gap-2 text-sm",
        tone,
        className,
      )}
      role="status"
      aria-live="polite"
    >
      <Icon className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
      <span>
        <span className="font-medium">{label}</span>
        {outcome.details && (
          <>
            <span aria-hidden="true">: </span>
            <span className="font-normal">{outcome.details}</span>
          </>
        )}
      </span>
    </div>
  );
}

function pickPresentation(status: TestOutcome["status"]): {
  Icon: typeof CheckCircle2;
  tone: string;
  label: string;
} {
  switch (status) {
    case "pass":
      return {
        Icon: CheckCircle2,
        tone: "text-emerald-600 dark:text-emerald-400",
        label: "OK",
      };
    case "fail":
      return {
        Icon: XCircle,
        tone: "text-destructive",
        label: "Failed",
      };
    case "needs_permission":
      return {
        Icon: AlertTriangle,
        tone: "text-amber-600 dark:text-amber-400",
        label: "Permission needed",
      };
    case "skipped":
      return {
        Icon: AlertTriangle,
        tone: "text-muted-foreground",
        label: "Skipped",
      };
  }
}
