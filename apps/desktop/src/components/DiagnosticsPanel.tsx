/**
 * Diagnostics tab for the Review route — PR-ε (phase 67).
 *
 * Reads `<cache_root>/sessions/<sessionId>/heron_session.json` via
 * `heron_diagnostics` and renders the returned `DiagnosticsView` as a
 * grid of metric cards. Each card pairs a value with a status icon
 * (`CheckCircle2` / `AlertTriangle` / `XCircle`) computed from the
 * thresholds documented inline below.
 *
 * The thresholds are deliberately conservative: a 60% AX hit rate is
 * a warning (not an error) because some apps are inherently low-yield
 * for accessibility — Zoom-Cloud meetings versus Google Meet, for
 * instance. The UI's job is to surface *something useful* when a
 * recording goes sideways, not to gate the user behind a green-light
 * checklist.
 *
 * Empty state: when `heron_diagnostics` returns NotFound (the file
 * hasn't been written yet — recording in progress, or pre-§19.2
 * sessions), we render a friendly "No diagnostics yet" rather than
 * an opaque error.
 */

import { useEffect, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Coins,
  Mic2,
  Timer,
  XCircle,
  type LucideIcon,
} from "lucide-react";

import { invoke, type DiagnosticsView } from "../lib/invoke";
import { cn } from "../lib/cn";

type LoadState =
  | { kind: "loading" }
  | { kind: "ready"; view: DiagnosticsView }
  | { kind: "empty" }
  | { kind: "error"; message: string };

interface DiagnosticsPanelProps {
  /** Cache root from `heron_default_cache_root`. */
  cacheRoot: string;
  /** Current session basename. */
  sessionId: string;
  /**
   * Refresh nonce — the parent bumps this when it knows new
   * diagnostics may have landed (e.g. after a successful
   * re-summarize). The panel re-fetches on bump.
   */
  refreshKey?: number;
}

/** Distinguish "file not on disk yet" from "real read error". */
function isNotFoundError(message: string): boolean {
  // The Rust side emits the literal string `session.json not found`
  // via `DiagnosticsError::NotFound`. Matching on substring keeps us
  // resilient to the surrounding format ever shifting (path is
  // appended).
  return message.includes("not found");
}

/**
 * Build the platform path the diagnostics command expects.
 * Mirrors the Rust side's `<cache>/sessions/<id>/heron_session.json`
 * layout per `docs/observability.md`.
 */
function diagnosticsPath(cacheRoot: string, sessionId: string): string {
  return `${cacheRoot}/sessions/${sessionId}/heron_session.json`;
}

type Status = "ok" | "warn" | "error" | "unknown";

const STATUS_ICON: Record<Status, LucideIcon> = {
  ok: CheckCircle2,
  warn: AlertTriangle,
  error: XCircle,
  unknown: AlertTriangle,
};

const STATUS_CLASS: Record<Status, string> = {
  ok: "text-green-600",
  warn: "text-amber-600",
  error: "text-red-600",
  unknown: "text-muted-foreground",
};

/**
 * AX hit rate thresholds — see §19.2:
 * - ≥0.9 OK (the typical Zoom-on-Mac result)
 * - ≥0.6 warn (Meet, lower-yield apps)
 * - <0.6 error (probably an accessibility-permission issue)
 * - missing => unknown (older sessions / no AX activity)
 */
function axStatus(rate: number | null): Status {
  if (rate === null) return "unknown";
  if (rate >= 0.9) return "ok";
  if (rate >= 0.6) return "warn";
  return "error";
}

/**
 * Dropped-frames thresholds — §7.4:
 * - 0 OK
 * - 1..=10 warn (rare, suggests CPU spike)
 * - >10 error (back-pressure persisted; investigate APM tuning)
 */
function droppedStatus(n: number | null): Status {
  if (n === null) return "unknown";
  if (n === 0) return "ok";
  if (n <= 10) return "warn";
  return "error";
}

/**
 * STT wall-time thresholds — §8.6 perf budget assumes ≤1.0× the
 * meeting length on Apple silicon. We don't know the meeting length
 * here, so we bucket by absolute seconds: under a minute is fine,
 * 1–5 minutes is the warn band (long meetings or slow models),
 * >5 min suggests something stalled.
 */
function sttStatus(secs: number | null): Status {
  if (secs === null) return "unknown";
  if (secs < 60) return "ok";
  if (secs < 300) return "warn";
  return "error";
}

/**
 * Cost is informational — we never fail a session on cost, but
 * surface a warn when a single summarize crossed $0.50 so the user
 * notices runaway prompt size.
 */
function costStatus(usd: number | null): Status {
  if (usd === null) return "unknown";
  if (usd <= 0.5) return "ok";
  return "warn";
}

function errorStatus(count: number): Status {
  if (count === 0) return "ok";
  if (count <= 2) return "warn";
  return "error";
}

interface MetricCardProps {
  label: string;
  value: string;
  status: Status;
  icon: LucideIcon;
}

function MetricCard({ label, value, status, icon: Icon }: MetricCardProps) {
  const StatusIcon = STATUS_ICON[status];
  return (
    <div className="border border-border rounded-md p-3 bg-background space-y-1">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2 text-xs text-muted-foreground uppercase tracking-wide">
          <Icon className="h-3.5 w-3.5" aria-hidden="true" />
          {label}
        </div>
        <StatusIcon
          className={cn("h-4 w-4 shrink-0", STATUS_CLASS[status])}
          aria-label={`status: ${status}`}
        />
      </div>
      <div className="text-lg font-semibold tabular-nums">{value}</div>
    </div>
  );
}

function formatPercent(rate: number | null): string {
  if (rate === null) return "—";
  return `${Math.round(rate * 100)}%`;
}

function formatCount(n: number | null): string {
  if (n === null) return "—";
  return n.toString();
}

function formatSeconds(secs: number | null): string {
  if (secs === null) return "—";
  if (secs < 60) return `${secs.toFixed(1)}s`;
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  return `${m}m ${s}s`;
}

function formatCostUsd(usd: number | null): string {
  if (usd === null) return "—";
  // 4 decimal places: a single summarize is typically $0.0001-$0.05.
  return `$${usd.toFixed(4)}`;
}

export function DiagnosticsPanel({
  cacheRoot,
  sessionId,
  refreshKey,
}: DiagnosticsPanelProps) {
  const [load, setLoad] = useState<LoadState>({ kind: "loading" });

  useEffect(() => {
    let cancelled = false;
    setLoad({ kind: "loading" });
    invoke("heron_diagnostics", {
      sessionLogPath: diagnosticsPath(cacheRoot, sessionId),
    })
      .then((view) => {
        if (cancelled) return;
        setLoad({ kind: "ready", view });
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        if (isNotFoundError(message)) {
          setLoad({ kind: "empty" });
        } else {
          setLoad({ kind: "error", message });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [cacheRoot, sessionId, refreshKey]);

  if (load.kind === "loading") {
    return (
      <div className="text-sm text-muted-foreground">Loading diagnostics…</div>
    );
  }
  if (load.kind === "empty") {
    return (
      <div className="text-sm text-muted-foreground italic">
        No diagnostics yet — recording is in progress.
      </div>
    );
  }
  if (load.kind === "error") {
    return (
      <div className="text-sm text-destructive">
        Failed to load diagnostics: {load.message}
      </div>
    );
  }

  const { view } = load;
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
        <MetricCard
          label="AX hit rate"
          value={formatPercent(view.ax_hit_rate)}
          status={axStatus(view.ax_hit_rate)}
          icon={Mic2}
        />
        <MetricCard
          label="Dropped frames"
          value={formatCount(view.dropped_frames)}
          status={droppedStatus(view.dropped_frames)}
          icon={AlertTriangle}
        />
        <MetricCard
          label="STT wall time"
          value={formatSeconds(view.stt_wall_time_secs)}
          status={sttStatus(view.stt_wall_time_secs)}
          icon={Timer}
        />
        <MetricCard
          label="LLM cost"
          value={formatCostUsd(view.llm_cost_usd)}
          status={costStatus(view.llm_cost_usd)}
          icon={Coins}
        />
        <MetricCard
          label="Errors"
          value={view.error_count.toString()}
          status={errorStatus(view.error_count)}
          icon={XCircle}
        />
      </div>

      {view.errors.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-sm font-semibold text-muted-foreground uppercase tracking-wide">
            Error log
          </h3>
          <ul className="space-y-1">
            {view.errors.map((err, i) => (
              // The session log doesn't enforce a unique id per error
              // — it's append-only and the same `kind` may legitimately
              // recur with different `at` timestamps. Use the index as
              // the tiebreaker so React's reconciler stays stable
              // across re-fetches without us inventing a synthetic id.
              <li
                key={`${err.kind}-${err.at ?? "no-at"}-${i}`}
                className="text-xs border border-border rounded-md p-2 space-y-0.5"
              >
                <div className="flex items-center gap-2 font-mono text-muted-foreground">
                  <span className="font-semibold text-foreground">
                    {err.kind}
                  </span>
                  {err.at && <span>{err.at}</span>}
                </div>
                <p className="text-foreground">{err.message}</p>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
