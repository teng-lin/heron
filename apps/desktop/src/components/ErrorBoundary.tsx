/**
 * Top-level render error catcher.
 *
 * Without this, a thrown render in any descendant unmounts the whole
 * tree and leaves the user staring at the system body color — looks
 * exactly like an empty page. We surface the error message + stack so
 * "broken page" turns into "broken page with debuggable copy" without
 * having to open devtools.
 *
 * Issue #226: in addition to the local console fallback the boundary
 * also dispatches a redacted [`FrontendErrorReport`] to the daemon via
 * the `heron_report_frontend_error` Tauri command so the
 * `frontend_errors_total{component, error_class}` counter increments
 * and the structured payload lands in the daemon's normal log stream.
 *
 * - Fire-and-forget — the boundary's UI must keep rendering even when
 *   the daemon is down, so the dispatch never awaits.
 * - Redaction at construction — the report is built from explicit
 *   safe fields by `buildFrontendErrorReport` (`lib/errorReport.ts`).
 *   Props/state are NEVER serialized into the payload; transcript
 *   text, participant names, API keys cannot leak through props.
 */

import { Component, type ErrorInfo, type ReactNode } from "react";

import {
  buildFrontendErrorReport,
  reportFrontendError,
} from "../lib/errorReport";
import type { FrontendErrorClass } from "../lib/invoke";

interface State {
  error: Error | null;
  info: ErrorInfo | null;
}

interface Props {
  children: ReactNode;
  /**
   * Build-time component path identifier for the metric label
   * dimension. Optional so call sites that just wrap the whole tree
   * don't have to thread one in (the default identifies the boundary
   * as the top-level guard).
   */
  componentPath?: string;
  /**
   * Injectable dispatcher. Defaults to the production
   * `reportFrontendError` (which fire-and-forgets the IPC). The
   * Bun-only test suite passes a stub so it can capture the payload
   * without going through Tauri's bridge.
   */
  reportError?: (
    report: ReturnType<typeof buildFrontendErrorReport>,
  ) => void;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, info: null };

  static getDerivedStateFromError(error: Error): State {
    return { error, info: null };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ error, info });
    // eslint-disable-next-line no-console
    console.error("[heron] render error:", error, info);

    // Issue #226: dispatch the redacted IPC report. We construct the
    // payload from explicit safe fields only — never from props or
    // state. The `componentDidCatch` lifecycle hook runs in the
    // commit phase; React's invariants are still upheld here, so a
    // throw from the redactor / dispatcher would NOT trigger a
    // recursive boundary catch (componentDidCatch errors propagate up
    // the tree). Belt-and-suspenders: the dispatcher is non-throwing
    // by contract (`reportFrontendError` swallows all rejections).
    try {
      const report = buildFrontendErrorReport({
        error,
        component: this.props.componentPath ?? "ErrorBoundary",
        route: readRoute(),
        appVersion: readAppVersion(),
        appBuild: readAppBuild(),
        // Lifecycle errors AND render errors both reach
        // `componentDidCatch`; React doesn't expose a per-error
        // discriminator, so we classify based on whether
        // `info.componentStack` is set (lifecycle errors carry one;
        // render exceptions also do — both flow here). Default to
        // `render_error` since that's the dominant case and bucket
        // others into `lifecycle_error`. Wider classification (e.g.
        // event-handler errors that DON'T reach this boundary) lands
        // in a follow-up.
        errorClass: classifyError(info) satisfies FrontendErrorClass,
        componentStack: info.componentStack ?? null,
      });
      const dispatch = this.props.reportError ?? reportFrontendError;
      dispatch(report);
    } catch (dispatchErr) {
      // The dispatcher swallows network failures, but the redactor
      // could in principle throw on a pathological input we didn't
      // foresee. Don't let that take down the boundary — the user is
      // already seeing a broken-page screen; a second exception here
      // would compound the problem. Log to the same console fallback.
      // eslint-disable-next-line no-console
      console.error("[heron] error-report dispatch failed:", dispatchErr);
    }
  }

  reset = () => this.setState({ error: null, info: null });

  render() {
    const { error, info } = this.state;
    if (error === null) return this.props.children;
    return (
      <div
        style={{
          padding: 24,
          fontFamily: "ui-monospace, SFMono-Regular, monospace",
          fontSize: 13,
          color: "var(--color-ink, #111)",
          background: "var(--color-paper, #fff)",
          height: "100vh",
          overflow: "auto",
        }}
      >
        <h1
          style={{
            fontSize: 18,
            fontWeight: 600,
            marginBottom: 12,
            color: "var(--color-rec, #c0392b)",
          }}
        >
          Something broke during render
        </h1>
        <p style={{ marginBottom: 8 }}>{String(error.message ?? error)}</p>
        <pre
          style={{
            whiteSpace: "pre-wrap",
            background: "var(--color-paper-2, #f5f5f5)",
            padding: 12,
            border: "1px solid var(--color-rule, #ddd)",
            borderRadius: 4,
            maxHeight: "60vh",
            overflow: "auto",
          }}
        >
          {error.stack ?? "(no stack)"}
          {info?.componentStack ?? ""}
        </pre>
        <div style={{ marginTop: 12, display: "flex", gap: 8 }}>
          <button
            type="button"
            onClick={this.reset}
            style={{
              padding: "6px 12px",
              border: "1px solid var(--color-rule, #ddd)",
              borderRadius: 4,
              background: "var(--color-paper, #fff)",
              cursor: "pointer",
            }}
          >
            Try again
          </button>
          <button
            type="button"
            onClick={() => window.location.reload()}
            style={{
              padding: "6px 12px",
              border: "1px solid var(--color-rule, #ddd)",
              borderRadius: 4,
              background: "var(--color-paper, #fff)",
              cursor: "pointer",
            }}
          >
            Reload
          </button>
        </div>
      </div>
    );
  }
}

/**
 * React-router pathname or `/` if window/location aren't available
 * (e.g. tests without jsdom). Reading from `window.location` rather
 * than `useLocation` keeps the boundary as a class component without
 * threading a navigate context.
 */
function readRoute(): string {
  if (typeof window !== "undefined" && window.location) {
    return window.location.pathname || "/";
  }
  return "/";
}

/** App version threaded in via vite's `define` block; falls back to "0.0.0". */
function readAppVersion(): string {
  if (typeof __APP_VERSION__ === "string") {
    return __APP_VERSION__;
  }
  return "0.0.0";
}

/** App build stamp; falls back to "unknown" when the define isn't replaced (tests). */
function readAppBuild(): string {
  if (typeof __APP_BUILD__ === "string") {
    return __APP_BUILD__;
  }
  return "unknown";
}

/**
 * React doesn't expose a per-error discriminator in `componentDidCatch`
 * — every exception thrown during render, lifecycle, or constructor
 * reaches this hook with the same `(error, info)` shape. We default to
 * `render_error` since it's the dominant case the boundary catches in
 * production. A thrown promise rejection that escapes the React tree
 * goes through `window.onunhandledrejection` instead and is reported
 * separately (follow-up) — it does not reach this boundary.
 *
 * The classifier exists as a function (rather than a constant) so a
 * future React feature that tags errors (e.g. `info.errorBoundary`
 * carrying a kind hint) drops in here without the call site changing.
 */
function classifyError(_info: ErrorInfo): FrontendErrorClass {
  return "render_error";
}
