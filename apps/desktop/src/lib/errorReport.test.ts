/**
 * Issue #226 — redaction unit tests for the frontend error report
 * builder + dispatcher.
 *
 * The hard rule the issue spells out:
 *
 *   > Privacy redaction at construction, not best-effort filtering.
 *   > Build the payload from explicit safe fields. Don't
 *   > `JSON.stringify(props)` and hope you didn't leak.
 *
 * These tests pin that contract:
 *
 *   1. The redactor's INPUT type is `BuildErrorReportInput` — there is
 *      no `props` / `state` / `extra` field on it. Even if a developer
 *      attaches `error.props = { secret: "..." }` to the thrown value,
 *      the secret cannot reach the report because the builder never
 *      walks the error's own properties past the canonical `message` /
 *      `stack` fields.
 *   2. Stack traces with home-directory prefixes are redacted to `~/`
 *      so a diagnostics bundle is shareable.
 *   3. Long fields are clamped so a multi-megabyte error can't dump
 *      transcript-shaped content into the daemon log.
 *   4. The dispatcher (`reportFrontendError`) is fire-and-forget — a
 *      throwing IPC mock never propagates back to the caller, which is
 *      the contract that keeps the ErrorBoundary UI rendering when the
 *      daemon is down.
 *   5. The dispatcher passes the report verbatim through the IPC
 *      argument shape `{ report }` so a future renamed parameter is
 *      caught at test time.
 */

import { describe, expect, test } from "bun:test";

import {
  buildFrontendErrorReport,
  reportFrontendError,
} from "./errorReport";
import type {
  FrontendErrorReport,
  HeronCommand,
  HeronCommands,
} from "./invoke";

describe("buildFrontendErrorReport", () => {
  test("ignores attached props/state on the thrown value", () => {
    // Simulate a buggy component that attaches user-content to its
    // error before re-throwing. The redactor MUST NOT walk these.
    const error = new Error("Cannot read property 'x' of undefined");
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (error as any).props = {
      transcript: "Hi everyone, thanks for joining today's standup.",
      participants: ["alice@example.com", "Bob"],
      apiKey: "sk-secret-deadbeef",
    };
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (error as any).state = { meetingTitle: "Q3 Strategy Review" };

    const report = buildFrontendErrorReport({
      error,
      component: "App.Recording",
      route: "/recording",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });

    const serialized = JSON.stringify(report);
    // Spot-check every leaked term. If any of these appear, the
    // builder leaked user content — the test fails LOUDLY.
    expect(serialized).not.toContain("Hi everyone");
    expect(serialized).not.toContain("thanks for joining");
    expect(serialized).not.toContain("alice@example.com");
    expect(serialized).not.toContain("Bob");
    expect(serialized).not.toContain("sk-secret-deadbeef");
    expect(serialized).not.toContain("Q3 Strategy Review");
    expect(serialized).not.toContain("transcript");
    expect(serialized).not.toContain("participants");
    expect(serialized).not.toContain("apiKey");
    expect(serialized).not.toContain("meetingTitle");

    // The canonical fields DO appear.
    expect(report.message).toContain("Cannot read property");
    expect(report.component).toBe("App.Recording");
    expect(report.route).toBe("/recording");
    expect(report.error_class).toBe("render_error");
  });

  test("does not walk error.cause chain (would leak nested user content)", () => {
    // V8's `new Error("msg", { cause: x })` exposes `error.cause`. A
    // naive serializer would walk this and could leak whatever the
    // calling code stuffed in. The builder MUST only consume
    // `error.message` and `error.stack`.
    const causeWithSecret = {
      transcript: "Hi everyone, today's standup",
      apiKey: "sk-leak-attempt",
    };
    const error = new Error("outer failure", { cause: causeWithSecret });

    const report = buildFrontendErrorReport({
      error,
      component: "App",
      route: "/",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });

    const serialized = JSON.stringify(report);
    expect(serialized).not.toContain("Hi everyone");
    expect(serialized).not.toContain("sk-leak-attempt");
    expect(serialized).not.toContain("transcript");
    expect(serialized).not.toContain("apiKey");
    // Outer message survives (the diagnostic value).
    expect(report.message).toContain("outer failure");
  });

  test("does not call error.toString() (would invoke a custom toString that leaks)", () => {
    // Adversarial case: a component class overrides `toString` to dump
    // its state. The builder MUST NOT invoke `String(error)` on Error
    // instances — only on primitive throws.
    class LeakingError extends Error {
      override toString() {
        return "LEAKED: secret=sk-deadbeef transcript=hi-everyone";
      }
    }
    const error = new LeakingError("safe message");
    const report = buildFrontendErrorReport({
      error,
      component: "App",
      route: "/",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });
    expect(report.message).toBe("safe message");
    const serialized = JSON.stringify(report);
    expect(serialized).not.toContain("LEAKED");
    expect(serialized).not.toContain("sk-deadbeef");
    expect(serialized).not.toContain("hi-everyone");
  });

  test("non-Error thrown values produce a safe message and no stack", () => {
    // `throw "boom"` is legal JS. The redactor must not call
    // `JSON.stringify(error)` — `String(value)` is the safe path.
    const report = buildFrontendErrorReport({
      error: "boom",
      component: "Settings",
      route: "/settings",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "lifecycle_error",
      componentStack: null,
    });
    expect(report.message).toBe("boom");
    expect(report.stack).toBeNull();
  });

  test("plain-object throws collapse to <unknown error> and do not leak keys", () => {
    // The dangerous case: a buggy component does
    // `throw { transcript: "...", apiKey: "..." }`. A naive redactor
    // would stringify the object and leak both keys. We collapse to a
    // safe placeholder.
    const report = buildFrontendErrorReport({
      error: {
        transcript: "Hi everyone",
        apiKey: "sk-secret-deadbeef",
      },
      component: "Recording",
      route: "/recording",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });
    expect(report.message).toBe("<unknown error>");
    const serialized = JSON.stringify(report);
    expect(serialized).not.toContain("Hi everyone");
    expect(serialized).not.toContain("sk-secret-deadbeef");
    expect(serialized).not.toContain("transcript");
    expect(serialized).not.toContain("apiKey");
  });

  test("home-directory paths in stack traces are redacted to ~/", () => {
    const error = new Error("crash");
    error.stack = [
      "Error: crash",
      "    at f (/Users/alice/heron/app.tsx:1:1)",
      "    at g (/home/bob/heron/lib.ts:2:2)",
    ].join("\n");

    const report = buildFrontendErrorReport({
      error,
      component: "App",
      route: "/",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });

    expect(report.stack).not.toContain("/Users/alice");
    expect(report.stack).not.toContain("/home/bob");
    // The relative file portion survives so the report stays useful.
    expect(report.stack).toContain("~/heron/app.tsx");
    expect(report.stack).toContain("~/heron/lib.ts");
  });

  test("Windows-style C:\\Users paths are redacted", () => {
    // Heron is macOS-only in v1, but the redactor runs in CI / on a
    // contributor's Windows checkout. Catching the prefix here keeps
    // a hypothetical Windows build from leaking usernames into reports
    // without us needing platform-specific code paths in production.
    const error = new Error("crash");
    error.stack = "Error: crash\n    at f (C:\\Users\\carol\\heron\\app.tsx:1:1)";

    const report = buildFrontendErrorReport({
      error,
      component: "App",
      route: "/",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });

    expect(report.stack).not.toContain("C:\\Users\\carol");
    // The relative file portion survives. We don't pin the exact
    // separator (~\\heron) since the rest of the path keeps native
    // separators; just assert the username segment is gone.
    expect(report.stack).toContain("heron\\app.tsx");
  });

  test("oversized message is clamped with a truncation marker", () => {
    // A buggy component might `throw new Error(transcript_text)`. The
    // length cap makes sure a single bad call can't dump megabytes
    // through the IPC.
    const huge = "X".repeat(50_000);
    const error = new Error(huge);
    const report = buildFrontendErrorReport({
      error,
      component: "App",
      route: "/",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });
    expect(report.message.length).toBeLessThan(huge.length);
    expect(report.message).toContain("[truncated]");
  });

  test("componentStack passes through the same redaction pipeline", () => {
    const error = new Error("crash");
    const report = buildFrontendErrorReport({
      error,
      component: "App",
      route: "/",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: "\n    at f (/Users/alice/heron/x.tsx:1:1)",
    });
    expect(report.component_stack).not.toContain("/Users/alice");
    expect(report.component_stack).toContain("~/heron/x.tsx");
  });

  test("missing componentStack lands on the wire as null, not undefined", () => {
    // The Rust struct is `Option<String>`; `null` and "missing" both
    // decode to `None`, but the wire emit is `null`, and the
    // `ipc_shape.rs` snapshot pins that. Match here on the JS side.
    const error = new Error("crash");
    const report = buildFrontendErrorReport({
      error,
      component: "App",
      route: "/",
      appVersion: "0.1.0",
      appBuild: "2026-05-01",
      errorClass: "render_error",
      componentStack: null,
    });
    expect(report.component_stack).toBeNull();
  });

  test("preserves the four error_class discriminants verbatim", () => {
    // The closed-union TS type already enforces this at compile time;
    // belt-and-suspenders that the builder doesn't normalize / lower-
    // case / otherwise rewrite the discriminant.
    const classes: ReadonlyArray<FrontendErrorReport["error_class"]> = [
      "render_error",
      "lifecycle_error",
      "promise_rejection",
      "unknown",
    ];
    for (const cls of classes) {
      const report = buildFrontendErrorReport({
        error: new Error("x"),
        component: "App",
        route: "/",
        appVersion: "0.1.0",
        appBuild: "2026-05-01",
        errorClass: cls,
        componentStack: null,
      });
      expect(report.error_class).toBe(cls);
    }
  });
});

describe("reportFrontendError", () => {
  test("dispatches under the heron_report_frontend_error command name with { report }", () => {
    const calls: Array<{ cmd: HeronCommand; args: unknown }> = [];
    const fakeInvoke = (async (cmd: HeronCommand, args: unknown) => {
      calls.push({ cmd, args });
      return undefined;
    }) as unknown as Parameters<typeof reportFrontendError>[1];

    const report: FrontendErrorReport = {
      error_class: "render_error",
      message: "x",
      component: "App",
      route: "/",
      app_version: "0.1.0",
      app_build: "2026-05-01",
      stack: null,
      component_stack: null,
    };
    reportFrontendError(report, fakeInvoke);

    expect(calls.length).toBe(1);
    expect(calls[0]?.cmd).toBe("heron_report_frontend_error");
    expect(calls[0]?.args).toEqual({ report });
  });

  test("swallows IPC rejections (fire-and-forget contract)", async () => {
    // Daemon-down failure mode: the IPC call rejects. The dispatcher
    // must NOT propagate this — the ErrorBoundary's UI is already
    // showing a broken-page screen, and an unhandled promise rejection
    // here would round-trip back through the boundary and create a
    // dispatch loop.
    const fakeInvoke = (async () => {
      throw new Error("daemon unreachable");
    }) as unknown as Parameters<typeof reportFrontendError>[1];

    const report: FrontendErrorReport = {
      error_class: "render_error",
      message: "x",
      component: "App",
      route: "/",
      app_version: "0.1.0",
      app_build: "2026-05-01",
      stack: null,
      component_stack: null,
    };

    // The function returns void; the test passes if it doesn't throw
    // and the rejection doesn't escape as an unhandled promise.
    expect(() => reportFrontendError(report, fakeInvoke)).not.toThrow();

    // Yield to the microtask queue so the catch attached inside
    // `reportFrontendError` runs. Without this the test would exit
    // before the rejection-handling path executes.
    await Promise.resolve();
    await Promise.resolve();
  });

  test("typed under HeronCommands.heron_report_frontend_error", () => {
    // Compile-time anchor: if a future rename / re-shape drops
    // `heron_report_frontend_error` from the `HeronCommands` map, this
    // line fails to type-check. Surfacing as a runtime test means the
    // CI gate ALSO catches the regression rather than relying solely
    // on the editor's type-check.
    const k: keyof HeronCommands = "heron_report_frontend_error";
    expect(k).toBe("heron_report_frontend_error");
  });
});
