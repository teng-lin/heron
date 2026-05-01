//! `heron-metrics` — observability primitives.
//!
//! Foundation crate for the workspace's metrics surface. Sub-issues
//! #224 (capture + STT), #225 (LLM + vault), #226 (frontend errors)
//! consume the primitives, naming convention, and redaction posture
//! defined here. Concrete instrumentation belongs there, not here.
//!
//! ## Library choice
//!
//! Uses the [`metrics`](https://crates.io/crates/metrics) facade with
//! a [`metrics-exporter-prometheus`] recorder. Rationale captured in
//! [`docs/observability.md`](../../../docs/observability.md): a thin
//! facade keeps call sites (`metrics::counter!`) decoupled from the
//! exporter, so swapping in StatsD or OTLP later is one wiring change
//! rather than a workspace-wide rewrite. Prometheus exposition is the
//! lingua franca of self-hosted observability and the daemon's
//! existing localhost-HTTP surface gives us a free transport.
//!
//! ## Naming convention
//!
//! Prometheus-style snake_case with units in the suffix:
//!
//! - Counters end in `_total` (`capture_started_total`,
//!   `llm_calls_total`).
//! - Histograms end in their unit (`llm_call_duration_seconds`,
//!   `vault_note_size_bytes`).
//! - Gauges end in their unit or are bare nouns
//!   (`salvage_candidates_pending`, `replay_cache_depth`).
//!
//! [`metric_name!`] runs the convention validator at first call;
//! drifted names panic, and every metric must have at least one
//! unit test that exercises the call site so the panic fires in
//! CI rather than in production. See `docs/observability.md`
//! §"Naming" for the full rule list.
//!
//! ## Privacy posture (CRITICAL)
//!
//! Metric **labels** must never carry user-content-derived strings.
//! The bypass risk is real: a `meeting_id`, transcript snippet, or
//! attendee name embedded in a label cardinality-explodes the time
//! series database AND leaks user content to anything that can read
//! the exposition endpoint (a future Prometheus scrape, a debug
//! curl, `--diagnostics-bundle`).
//!
//! This crate enforces redaction at the type level: every label value
//! flows through [`RedactedLabel`], which is **only constructable**
//! through the [`redacted!`] macro (compile-time validation against
//! a static allowlist of low-cardinality enum-like values), the
//! [`RedactedLabel::from_static`] constructor (any `&'static str` —
//! the `'static` bound is what makes it safe: a freshly-`format!`ed
//! string cannot satisfy it), or the [`RedactedLabel::hashed`]
//! constructor (BLAKE-style 8-byte hex digest of an arbitrary input
//! when grouping by an opaque correlation id is genuinely needed).
//!
//! There is **no** `From<String>`, `From<&str>` for non-static
//! references, or `Display`-via-format constructor. A reviewer
//! catching `redacted!("meeting-{id}")` should reject the PR; the
//! macro accepts only string literals.
//!
//! ## Smoke metric
//!
//! [`SMOKE_CAPTURE_STARTED_TOTAL`] is the canonical example sub-issues
//! copy. It's instrumented in `heron-orchestrator::start_capture` and
//! reachable via `GET /v1/__metrics` on the daemon (see
//! `crates/herond/src/routes/metrics.rs`).

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod label;
mod naming;
mod recorder;

pub use label::{InvalidLabel, RedactedLabel};
pub use naming::{InvalidMetricName, validate_metric_name};
pub use recorder::{MetricsHandle, init_prometheus_recorder};

/// Canonical name of the smoke metric instrumented in
/// `heron-orchestrator::start_capture`. Sub-issues #224 / #225 / #226
/// copy the call-site shape (label-via-`redacted!`, name-as-`const`)
/// when adding their own metrics.
///
/// The naming convention is asserted by
/// `naming::tests::smoke_metric_name_passes_validation`, which
/// fails the workspace test suite if a future rename drifts.
/// Convention enforcement for downstream metrics is via
/// [`metric_name!`] (recommended for new sites) or via a
/// `validate_metric_name`-asserting unit test on a `pub const`.
pub const SMOKE_CAPTURE_STARTED_TOTAL: &str = "capture_started_total";
