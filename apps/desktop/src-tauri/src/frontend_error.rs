//! Issue #226: frontend error reporting + ErrorBoundary instrumentation.
//!
//! `heron_report_frontend_error` is the IPC command the renderer's
//! `ErrorBoundary` calls (fire-and-forget) when a render exception
//! escapes a React component subtree. The handler:
//!
//!   1. Increments the `frontend_errors_total{component, error_class}`
//!      counter on the process-global Prometheus recorder installed by
//!      [`heron_metrics::init_prometheus_recorder`] at daemon startup.
//!   2. Logs the structured payload via `tracing::warn!` so the same
//!      report lands in the daemon's normal log stream / diagnostics
//!      bundle without the operator having to scrape the metrics
//!      endpoint.
//!
//! ## Privacy posture
//!
//! The renderer is responsible for **constructing** a redacted
//! [`FrontendErrorReport`]; the Rust side is the second line of defense:
//!
//! - `error_class` is a closed enum ([`ErrorClass`]) with snake_case
//!   discriminants. Free-form strings would risk Prometheus label
//!   cardinality explosion and aren't accepted on the wire.
//! - `component` flows through [`heron_metrics::RedactedLabel::hashed`]
//!   when emitted as a metric label — the hashed digest stays inside
//!   `MAX_LABEL_LEN` and the charset rules. The original value (a
//!   build-time component path like `"App.Recording"`) still lands in
//!   the structured `tracing` log so a developer can correlate.
//! - The message / stack / route are NEVER attached as metric labels
//!   (cardinality + privacy). They reach `tracing::warn!` only.
//!
//! See `docs/observability.md` §"Privacy posture" for the rule and
//! `crates/heron-metrics/src/label.rs` for the redaction primitives.

use serde::{Deserialize, Serialize};

/// Closed enum of frontend error classes the renderer may report.
///
/// Keeping this an enum (rather than a free-form `String`) is the
/// Prometheus-cardinality safeguard for the `error_class` metric label
/// dimension. Adding a new variant is intentionally a Rust + TS edit
/// so the wire surface stays auditable.
///
/// Wire format: snake_case via `#[serde(rename_all = "snake_case")]`,
/// matching the convention every other Rust-side enum uses (see
/// `heron_types::RecordingState`, `Platform`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// React render-phase exception caught by `getDerivedStateFromError`.
    RenderError,
    /// Lifecycle / effect exception caught by `componentDidCatch`.
    LifecycleError,
    /// Unhandled `Promise` rejection bubbled to `window.onunhandledrejection`.
    PromiseRejection,
    /// Anything else the renderer didn't classify. Folded into a
    /// single bucket rather than letting the wire grow new variants.
    Unknown,
}

impl ErrorClass {
    /// Stable label string for the metrics dimension. Pinned here (not
    /// derived from `Debug`) so a future `Debug` derive change can't
    /// silently rename a Prometheus label — that would break dashboards
    /// + alert rules built on the prior name.
    pub fn as_label(&self) -> heron_metrics::RedactedLabel {
        match self {
            Self::RenderError => heron_metrics::redacted!("render_error"),
            Self::LifecycleError => heron_metrics::redacted!("lifecycle_error"),
            Self::PromiseRejection => heron_metrics::redacted!("promise_rejection"),
            Self::Unknown => heron_metrics::redacted!("unknown"),
        }
    }
}

/// Wire-format payload the renderer sends to
/// [`super::heron_report_frontend_error`].
///
/// Constructed by the renderer's `ErrorBoundary` from explicit safe
/// fields — never from `JSON.stringify(props)` or `serialize(state)`.
/// See `apps/desktop/src/lib/errorReport.ts` for the JS-side builder
/// and the redaction unit test.
///
/// Fields are intentionally all owned `String` (no `Cow`, no borrows)
/// because the value crosses the Tauri IPC bridge as JSON: serde owns
/// the bytes and there's no lifetime to plumb. Round-tripped under the
/// `ipc_shape.rs` insta snapshot to lock the wire format down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendErrorReport {
    /// Closed enum — see [`ErrorClass`].
    pub error_class: ErrorClass,
    /// Error message body. Renderer-side redactor truncates and
    /// replaces home-directory prefixes; the Rust side trusts the
    /// renderer's redaction and treats this as opaque text.
    pub message: String,
    /// Build-time component path identifier. NOT a filesystem path —
    /// e.g. `"App.Recording"` or `"ResummarizeDialog"`. Build-time
    /// strings are safe (no user content); this field doubles as the
    /// `component` metric label dimension after `RedactedLabel::hashed`.
    pub component: String,
    /// React-router route at the time of the error, e.g. `"/recording"`.
    /// Build-time strings (no user content); kept off the metric label
    /// to avoid widening cardinality past `component`.
    pub route: String,
    /// App version + build stamp the renderer was running. Threads
    /// `__APP_VERSION__` / `__APP_BUILD__` from `vite.config.ts`. Both
    /// build-time strings.
    pub app_version: String,
    pub app_build: String,
    /// Stack trace with home-directory paths normalized to `~/...`
    /// renderer-side. The Rust side does NOT re-redact — the renderer
    /// is the source of truth for "this is safe to log."
    pub stack: Option<String>,
    /// React component stack from `ErrorInfo.componentStack`. Same
    /// normalization contract as `stack`.
    pub component_stack: Option<String>,
}

/// Implementation of the `heron_report_frontend_error` Tauri command
/// (the `#[tauri::command]` shim lives in `lib.rs` so the macro's
/// path-resolution finds it; this is the unit-testable function).
///
/// **Fire-and-forget contract:** errors are swallowed (logged) rather
/// than returned to the renderer. The ErrorBoundary's UI must keep
/// rendering even when the daemon is down — see issue #226's
/// "Notes" §.
///
/// Returns `Result<(), String>` so the wire signature matches the rest
/// of the Tauri surface; in practice every code path returns `Ok(())`.
pub fn report_frontend_error(report: FrontendErrorReport) {
    // The `component` field is a build-time string from the renderer
    // (e.g. "App.Recording"). It still goes through `RedactedLabel::
    // hashed` for the metric label — the hash collapses arbitrary
    // length / charset into a fixed 16-hex-char digest, which keeps
    // Prometheus happy AND means a future bug that lets user-content
    // sneak into the `component` field can't widen cardinality past
    // the digest space. The pre-hash value still lands in the
    // structured `tracing::warn!` below for developer correlation.
    let component_label = heron_metrics::RedactedLabel::hashed(&report.component);
    let class_label = report.error_class.as_label();

    metrics::counter!(
        FRONTEND_ERRORS_TOTAL,
        "component" => component_label.into_inner(),
        "error_class" => class_label.into_inner(),
    )
    .increment(1);

    // Structured log so the daemon's existing `tracing` JSON sink
    // captures the full report. The metric only carries the redacted
    // dimensions; the log carries the message + stack so a developer
    // looking at a single error can reproduce.
    //
    // We log at WARN (not ERROR): a single render error is a
    // user-visible degraded state but not a service-down event. ERROR
    // is reserved for daemon-side failures. See the corresponding
    // convention in `crates/herond/src/lib.rs`.
    tracing::warn!(
        error_class = ?report.error_class,
        component = %report.component,
        route = %report.route,
        app_version = %report.app_version,
        app_build = %report.app_build,
        message = %report.message,
        stack = report.stack.as_deref().unwrap_or("(none)"),
        component_stack = report.component_stack.as_deref().unwrap_or("(none)"),
        "frontend error reported via heron_report_frontend_error",
    );
}

/// Canonical name of the frontend-error counter.
///
/// Pinned here (not inlined at the call site) so a future rename
/// flows through one place AND so the integration test can assert
/// on the exposition output by literal string match.
pub const FRONTEND_ERRORS_TOTAL: &str = "frontend_errors_total";

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn error_class_serializes_snake_case() {
        // Wire format must be snake_case so the renderer's
        // `"render_error"` literal decodes to `ErrorClass::RenderError`
        // without an explicit map.
        assert_eq!(
            serde_json::to_value(ErrorClass::RenderError).unwrap(),
            serde_json::json!("render_error"),
        );
        assert_eq!(
            serde_json::to_value(ErrorClass::LifecycleError).unwrap(),
            serde_json::json!("lifecycle_error"),
        );
        assert_eq!(
            serde_json::to_value(ErrorClass::PromiseRejection).unwrap(),
            serde_json::json!("promise_rejection"),
        );
        assert_eq!(
            serde_json::to_value(ErrorClass::Unknown).unwrap(),
            serde_json::json!("unknown"),
        );
    }

    #[test]
    fn error_class_round_trips_through_json() {
        // Belt-and-suspenders: a future serde-shape change on either
        // side fails this round-trip before the wire format drifts.
        for class in [
            ErrorClass::RenderError,
            ErrorClass::LifecycleError,
            ErrorClass::PromiseRejection,
            ErrorClass::Unknown,
        ] {
            let v = serde_json::to_value(class).unwrap();
            let decoded: ErrorClass = serde_json::from_value(v).unwrap();
            assert_eq!(decoded, class);
        }
    }

    #[test]
    fn error_class_label_strings_are_stable() {
        // Pin the metric label literals — a future rename here would
        // break dashboards + alerts. The test reads `as_label`'s
        // inner so a contributor renaming a variant has to update
        // this assertion AND knows the rename is observable.
        assert_eq!(ErrorClass::RenderError.as_label().as_str(), "render_error");
        assert_eq!(
            ErrorClass::LifecycleError.as_label().as_str(),
            "lifecycle_error"
        );
        assert_eq!(
            ErrorClass::PromiseRejection.as_label().as_str(),
            "promise_rejection"
        );
        assert_eq!(ErrorClass::Unknown.as_label().as_str(), "unknown");
    }

    #[test]
    fn frontend_error_report_round_trips_through_json() {
        // Snapshot tests in `ipc_shape.rs` lock the wire shape; this
        // unit test catches a `#[serde(skip)]` regression that would
        // drop a real field on decode.
        let report = FrontendErrorReport {
            error_class: ErrorClass::RenderError,
            message: "Cannot read property 'x' of undefined".to_owned(),
            component: "App.Recording".to_owned(),
            route: "/recording".to_owned(),
            app_version: "0.1.0".to_owned(),
            app_build: "2026-05-01".to_owned(),
            stack: Some("at f (~/app.tsx:1:1)".to_owned()),
            component_stack: Some("\n  at App\n  at ErrorBoundary".to_owned()),
        };
        let v = serde_json::to_value(&report).unwrap();
        let decoded: FrontendErrorReport = serde_json::from_value(v).unwrap();
        assert_eq!(decoded, report);
    }

    #[test]
    fn frontend_error_report_decodes_with_null_optional_fields() {
        // The renderer may legitimately have no stack (e.g. a thrown
        // string instead of an Error). The wire form is `null`, not
        // omission — pin that the decoder accepts both.
        let json_with_null = serde_json::json!({
            "error_class": "lifecycle_error",
            "message": "boom",
            "component": "Settings",
            "route": "/settings",
            "app_version": "0.1.0",
            "app_build": "2026-05-01",
            "stack": null,
            "component_stack": null,
        });
        let decoded: FrontendErrorReport =
            serde_json::from_value(json_with_null).expect("decode with null stacks");
        assert_eq!(decoded.stack, None);
        assert_eq!(decoded.component_stack, None);
    }

    #[test]
    fn report_frontend_error_increments_counter() {
        // Drive the handler against a freshly-installed Prometheus
        // recorder and assert the rendered exposition contains the
        // metric name + the expected label dimensions. This is the
        // canonical integration assertion the issue calls for.
        //
        // The recorder is process-global and idempotent, so this test
        // can run alongside the smoke metric test in `heron-metrics`
        // without stomping.
        let handle = heron_metrics::init_prometheus_recorder().expect("install recorder");
        let report = FrontendErrorReport {
            error_class: ErrorClass::RenderError,
            message: "Cannot read property 'x' of undefined".to_owned(),
            component: "App.Recording".to_owned(),
            route: "/recording".to_owned(),
            app_version: "0.1.0".to_owned(),
            app_build: "2026-05-01".to_owned(),
            stack: Some("at f (~/app.tsx:1:1)".to_owned()),
            component_stack: Some("\n  at App".to_owned()),
        };
        report_frontend_error(report);

        let body = handle.render();
        assert!(
            body.contains(FRONTEND_ERRORS_TOTAL),
            "rendered exposition must contain frontend_errors_total. Got:\n{body}"
        );
        assert!(
            body.contains("error_class=\"render_error\""),
            "rendered exposition must carry error_class label. Got:\n{body}"
        );
        // The component label is the BLAKE-style hashed digest of
        // "App.Recording" (16 hex chars). We don't pin the exact
        // digest — `heron-metrics` is free to migrate the hash function
        // — but we DO assert the label key is present so a regression
        // that drops the dimension would fail here.
        assert!(
            body.contains("component=\""),
            "rendered exposition must carry component label. Got:\n{body}"
        );
    }

    #[test]
    fn report_frontend_error_does_not_panic_on_long_component_string() {
        // Privacy regression guard: even if a developer accidentally
        // stuffs user content into `component` (long path, transcript-
        // shaped text), the `RedactedLabel::hashed` fold collapses it
        // to a stable 16-char digest — no panic, no cardinality blow-up.
        let _handle = heron_metrics::init_prometheus_recorder().expect("install recorder");
        let report = FrontendErrorReport {
            error_class: ErrorClass::Unknown,
            // Deliberately path-shaped (not a real path — the renderer
            // would normalize) AND longer than `MAX_LABEL_LEN`. The
            // hash digestor must accept arbitrary input without
            // panicking.
            component: "/Users/alice/very-long-component-stack-that-should-not-blow-up/in/labels"
                .repeat(8),
            message: String::new(),
            route: "/".to_owned(),
            app_version: "0.1.0".to_owned(),
            app_build: "2026-05-01".to_owned(),
            stack: None,
            component_stack: None,
        };
        report_frontend_error(report);
        // The fact we got here without panicking is the assertion.
    }
}
