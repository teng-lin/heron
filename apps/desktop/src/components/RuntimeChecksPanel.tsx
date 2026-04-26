/**
 * Onboarding/Status display for `heron-doctor`'s consolidated runtime
 * preflight checks (gap #6).
 *
 * Renders one row per [`RuntimeCheckEntry`] returned by the
 * `heron_run_runtime_checks` Tauri command, with a status icon paired
 * to the entry's `severity` (`pass` / `warn` / `fail`) and the
 * doctor-supplied `summary` + optional `detail`.
 *
 * Why a separate component (instead of reusing `<TestStatus>`)?
 *
 * - `<TestStatus>` renders a single `TestOutcome` for one TCC probe.
 *   The doctor returns a **list** of entries, and the wizard wants
 *   them stacked with per-row severity badges — that's a different
 *   shape.
 * - The doctor's `summary` + `detail` split is richer than
 *   `TestOutcome`'s flat `details` string. The detail block is
 *   collapsed under the summary (rather than concatenated) so a
 *   verbose error doesn't blow out the wizard layout.
 *
 * The component is intentionally presentational: state (loading /
 * loaded / failed) lives in the parent so the same panel can be
 * dropped into a future Settings → "Run preflight" surface without
 * forking the data flow.
 */

import {
  AlertTriangle,
  CheckCircle2,
  Loader2,
  XCircle,
  type LucideIcon,
} from "lucide-react";

import { cn } from "../lib/cn";
import type {
  RuntimeCheckEntry,
  RuntimeCheckSeverity,
} from "../lib/invoke";

export type RuntimeChecksLoad =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; entries: RuntimeCheckEntry[] }
  | { kind: "error"; message: string };

interface RuntimeChecksPanelProps {
  load: RuntimeChecksLoad;
  className?: string;
}

/**
 * Per-severity presentation. The doctor's `warn` is amber rather than
 * destructive because — per `heron_doctor::runtime` module docs —
 * "warn" is the band that lets the wizard advance ("Zoom isn't
 * running but you can launch it later"), and we want the user to be
 * able to visually distinguish it from a hard `fail` at a glance.
 */
function pickPresentation(severity: RuntimeCheckSeverity): {
  Icon: LucideIcon;
  tone: string;
  label: string;
} {
  switch (severity) {
    case "pass":
      return {
        Icon: CheckCircle2,
        tone: "text-emerald-600 dark:text-emerald-400",
        label: "OK",
      };
    case "warn":
      return {
        Icon: AlertTriangle,
        tone: "text-amber-600 dark:text-amber-400",
        label: "Warning",
      };
    case "fail":
      return {
        Icon: XCircle,
        tone: "text-destructive",
        label: "Failed",
      };
  }
}

/**
 * Friendly labels for the doctor's stable check names. Falls back to
 * the raw `name` string when the doctor adds a new check the renderer
 * doesn't know about yet — see `RuntimeCheckEntry` JSDoc for why we
 * accept unknown names rather than filtering them.
 */
const CHECK_LABELS: Record<string, string> = {
  onnx_runtime: "ONNX runtime / models",
  zoom_process: "Zoom availability",
  keychain_acl: "Keychain ACL",
  network_reachability: "Network reachability",
};

function checkLabel(name: string): string {
  return CHECK_LABELS[name] ?? name.replace(/_/g, " ");
}

export function RuntimeChecksPanel({
  load,
  className,
}: RuntimeChecksPanelProps) {
  if (load.kind === "idle") {
    return null;
  }

  if (load.kind === "loading") {
    return (
      <div
        className={cn(
          "flex items-center gap-2 text-sm text-muted-foreground",
          className,
        )}
        role="status"
        aria-live="polite"
      >
        <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
        Running runtime checks…
      </div>
    );
  }

  if (load.kind === "error") {
    return (
      <div
        className={cn("text-sm text-destructive", className)}
        role="status"
        aria-live="polite"
      >
        Could not run runtime checks: {load.message}
      </div>
    );
  }

  if (load.entries.length === 0) {
    return (
      <div
        className={cn("text-sm italic text-muted-foreground", className)}
      >
        No runtime checks reported.
      </div>
    );
  }

  // `aria-live` lives on the wrapping `<div>` rather than the `<ul>`
  // so screen readers announce the live region once and the list
  // semantics once, not interleaved.
  return (
    <div className={className} role="status" aria-live="polite">
      <ul className="space-y-2">
      {load.entries.map((entry, idx) => {
        const { Icon, tone, label } = pickPresentation(entry.severity);
        return (
          <li
            // The doctor today emits unique `name` per entry, but
            // `RuntimeCheckEntry` is documented as forward-compatible
            // with unknown names — a future probe could in principle
            // emit two entries under the same `name`. Prefixing with
            // the index keeps React keys unique either way.
            key={`${idx}-${entry.name}`}
            className="rounded-md border border-border bg-background p-3 text-sm"
          >
            <div className="flex items-start gap-2">
              <Icon
                className={cn("mt-0.5 h-4 w-4 shrink-0", tone)}
                aria-label={`status: ${entry.severity}`}
              />
              <div className="flex-1 space-y-1">
                <div className="flex flex-wrap items-baseline gap-x-2">
                  <span className="font-medium">{checkLabel(entry.name)}</span>
                  <span className={cn("text-xs uppercase tracking-wide", tone)}>
                    {label}
                  </span>
                </div>
                <p className="text-muted-foreground">{entry.summary}</p>
                {entry.detail && (
                  <p className="text-xs text-muted-foreground/80 whitespace-pre-wrap">
                    {entry.detail}
                  </p>
                )}
              </div>
            </div>
          </li>
        );
      })}
      </ul>
    </div>
  );
}

/**
 * Aggregate severity across an entry list. Used by the onboarding
 * wizard to decide whether to surface a summary line ("All clear" vs
 * "1 issue detected").
 *
 * Returns the worst severity seen — `fail` dominates `warn` dominates
 * `pass`. An empty list returns `pass` (vacuously clear).
 */
export function aggregateSeverity(
  entries: RuntimeCheckEntry[],
): RuntimeCheckSeverity {
  let worst: RuntimeCheckSeverity = "pass";
  for (const e of entries) {
    if (e.severity === "fail") {
      return "fail";
    }
    if (e.severity === "warn") {
      worst = "warn";
    }
  }
  return worst;
}

/**
 * One-line synthesis of an entry list — bucketed by severity so the
 * wizard's `<TestStatus>` row above the per-entry panel reads as
 * "2 OK, 1 warning" at a glance. Empty list returns a placeholder.
 *
 * Keeps `aggregateSeverity` and `summariseEntries` together since the
 * onboarding wizard always uses both — the former drives the badge
 * colour, the latter the badge text.
 */
export function summariseEntries(entries: RuntimeCheckEntry[]): string {
  if (entries.length === 0) {
    return "No checks reported.";
  }
  let pass = 0;
  let warn = 0;
  let fail = 0;
  for (const e of entries) {
    if (e.severity === "pass") pass += 1;
    else if (e.severity === "warn") warn += 1;
    else fail += 1;
  }
  const parts: string[] = [`${pass} OK`];
  if (warn > 0) parts.push(`${warn} warning${warn === 1 ? "" : "s"}`);
  if (fail > 0) parts.push(`${fail} failed`);
  return parts.join(", ");
}
