/**
 * Issue #226 — frontend error report redactor.
 *
 * Builds a [`FrontendErrorReport`] payload from an `Error` instance +
 * a small set of explicit safe inputs. The hard rule the issue spells
 * out:
 *
 *   > Privacy redaction at construction, not best-effort filtering.
 *   > Build the payload from explicit safe fields. Don't
 *   > `JSON.stringify(props)` and hope you didn't leak.
 *
 * So this builder NEVER touches `props`, `state`, transcript text,
 * participant lists, or anything user-derived. Component path + route
 * are build-time strings the caller threads in. The error message and
 * stack trace are kept (they're the diagnostic value) but normalized
 * to redact home-directory paths and clamp length so a transcript-
 * shaped `error.message` can't dump 100 KB of user content into the
 * daemon log.
 *
 * The fire-and-forget dispatch wrapper (`reportFrontendError`) is
 * separate so the redactor stays a pure function for the unit tests.
 */

import type {
  FrontendErrorClass,
  FrontendErrorReport,
} from "./invoke";
import { invoke as defaultInvoke } from "./invoke";

/**
 * Maximum length of a single stringly-typed payload field. Keeps the
 * IPC + log volume bounded if a buggy component throws a multi-megabyte
 * error. The cap is deliberately generous (so a real stack stays
 * readable) but not unbounded.
 */
const MAX_FIELD_LEN = 8 * 1024;

/**
 * Inputs the renderer must thread in explicitly. Anything not listed
 * here will not appear in the report — there is no escape hatch via
 * `extra`, `props`, or `metadata`. Adding a new safe field is a
 * deliberate edit + matching Rust struct change.
 */
export interface BuildErrorReportInput {
  /** The thrown value the ErrorBoundary received. */
  error: unknown;
  /** Build-time component path (e.g. `"App.Recording"`). */
  component: string;
  /** React-router pathname at the time of the error. */
  route: string;
  /** App version (`__APP_VERSION__` from vite). */
  appVersion: string;
  /** App build stamp (`__APP_BUILD__` from vite). */
  appBuild: string;
  /**
   * Closed-union classifier the boundary chose. The TS compiler keeps
   * this honest — anything not in the [`FrontendErrorClass`] union
   * fails to type-check at the call site.
   */
  errorClass: FrontendErrorClass;
  /** Optional component stack from `ErrorInfo`. */
  componentStack?: string | null;
}

/**
 * Pure, deterministic redactor. Tests pin the contract: feed in a
 * thrown value with secrets all over (`error.props`, `error.cause`,
 * deep object keys) and assert none of them surface in the returned
 * payload.
 *
 * The function is also home-directory-aware — `/Users/<u>/...` and
 * `/home/<u>/...` paths in the stack become `~/...` so the rendered
 * report can be safely shared in a diagnostics bundle.
 */
export function buildFrontendErrorReport(
  input: BuildErrorReportInput,
): FrontendErrorReport {
  // `extractMessage` always returns a string (no `null` fallback),
  // so `redactString` here returns the non-null branch — assert via
  // `??` to a fixed sentinel and let TS narrow the type. The sentinel
  // is unreachable in practice but keeps the wire-type's `string`
  // contract honest at the type level.
  const message = redactString(extractMessage(input.error)) ?? "<unknown error>";
  const stack = redactString(extractStack(input.error));
  const componentStack = redactString(input.componentStack ?? null);

  // The Rust label-redaction (`RedactedLabel::hashed`) accepts
  // arbitrary input — no need to charset-clean here. We DO trim to a
  // sane length so the structured `tracing` log line stays readable.
  const component = clampLen(input.component, 256);
  const route = clampLen(input.route, 256);
  const appVersion = clampLen(input.appVersion, 64);
  const appBuild = clampLen(input.appBuild, 64);

  return {
    error_class: input.errorClass,
    message,
    component,
    route,
    app_version: appVersion,
    app_build: appBuild,
    stack,
    component_stack: componentStack,
  };
}

/**
 * Extract a human-readable message from a thrown value WITHOUT calling
 * `JSON.stringify(error)` (which would walk the whole prototype chain
 * and any attached props). Order of preference:
 *
 *   1. `Error.message` — the canonical field.
 *   2. `String(value)` for primitives — captures `throw "boom"`.
 *   3. `"<unknown error>"` for everything else (notably plain objects,
 *      where `JSON.stringify` would otherwise leak the keys).
 *
 * The default `String(obj)` returns `"[object Object]"` which is fine —
 * a developer who sees that in the report knows to look at the
 * component code; meanwhile the user content the object held never
 * reaches the wire.
 */
function extractMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  if (
    typeof error === "string" ||
    typeof error === "number" ||
    typeof error === "boolean" ||
    typeof error === "bigint"
  ) {
    return String(error);
  }
  return "<unknown error>";
}

/**
 * Extract `error.stack` if present — a string field we trust the V8
 * runtime to populate. Plain-object throws have no stack and return
 * `null`, which the wire schema accepts.
 */
function extractStack(error: unknown): string | null {
  if (error instanceof Error && typeof error.stack === "string") {
    return error.stack;
  }
  return null;
}

/**
 * Normalize a stringly field: clamp length, redact home directories.
 * `null` passes through unchanged (the wire form distinguishes
 * "no stack" from "empty string").
 */
function redactString(input: string | null): string | null {
  if (input === null) return null;
  return clampLen(redactHomeDirs(input), MAX_FIELD_LEN);
}

/**
 * Replace `/Users/<name>/`, `/home/<name>/`, and Windows
 * `C:\Users\<name>\` (rare but cheap to handle) prefixes with `~/`.
 * Catches the most common home-directory leak vector — V8 stack
 * frames bake the absolute on-disk path of the source file in.
 *
 * Heuristic, not perfect: a path embedded inside a quote ("at
 * /Users/...") is the V8 norm and matches; a manually-constructed
 * `error.message` of `/Users/alice/notes/transcript.txt` would also
 * be redacted (the regex matches anywhere in the string), which is the
 * conservative behaviour we want.
 */
function redactHomeDirs(s: string): string {
  return s
    .replace(/\/Users\/[^/\s)]+\//g, "~/")
    .replace(/\/home\/[^/\s)]+\//g, "~/")
    .replace(/[A-Z]:\\Users\\[^\\\s)]+\\/g, "~\\");
}

/**
 * Clamp a string to `max` characters. Appends an ellipsis marker when
 * truncation actually happened so the report consumer knows the field
 * was cut.
 */
function clampLen(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max - 16)}…[truncated]`;
}

/**
 * Fire-and-forget dispatch of a built report. The hard rule the issue
 * spells out:
 *
 *   > Fire-and-forget on the frontend. Don't block the error UI on
 *   > the IPC.
 *
 * So this never throws and never returns a meaningful promise — every
 * failure mode (daemon down, command not registered in dev, IPC
 * timeout) is swallowed and surfaced via the local `console.error`
 * fallback the boundary still emits.
 *
 * The `invoke` parameter is injectable so the unit tests can capture
 * the IPC arguments without going through the real Tauri bridge.
 */
export function reportFrontendError(
  report: FrontendErrorReport,
  invoke: typeof defaultInvoke = defaultInvoke,
): void {
  // The promise rejection is intentionally unhandled at the
  // attachment site — `void`-ing it lets the linter / type-checker
  // see we're aware. We swallow inside the `.catch` so a daemon-down
  // failure doesn't surface as an UnhandledPromiseRejection (which
  // would itself round-trip back through the boundary and create a
  // dispatch loop).
  void invoke("heron_report_frontend_error", { report }).catch(() => {
    // Intentionally empty: see fire-and-forget contract above.
  });
}
