//! `timed_io` — shared instrumentation helper for LLM and vault calls.
//!
//! Both areas in #225 share the same shape: an external/IO call wrapped
//! with timing + outcome. This module ships one helper used twice — by
//! the LLM crate's [`Summarizer::summarize`] paths and by the vault
//! writer's `atomic_write` / `update_action_item` / `finalize` paths —
//! so a future tweak (say, adding a third dimension to every duration
//! histogram) lands in one place.
//!
//! # Privacy
//!
//! Both arms accept only [`RedactedLabel`] for label values; there is
//! no overload that takes `&str` or `String`. Combined with the
//! `redacted!` macro's literal-only matcher, this means a caller
//! cannot smuggle a transcript snippet, persona text, or a `format!`ed
//! `model_id` into a label dimension. A reviewer who sees
//! `timed_io_sync(..., RedactedLabel::from_static(&id), ...)` has the
//! foothold to reject the PR — the `'static` bound fails to compile
//! against a runtime `String`.
//!
//! # Buffer-reuse caveat (PR #228)
//!
//! `read_transcript_segments` reuses a `Vec::with_capacity(256)` buffer
//! across iterations via `buf.clear()`. Wrapping the whole function in
//! a `timed_io_sync` block is fine — the buffer's `let mut buf = ...`
//! lives inside the closure body, which is the function body. **Do NOT**
//! move the `buf.clear()` call inside an inner metrics-recording block;
//! that would re-introduce the per-iteration allocation #228 fixed.

use std::time::Instant;

use crate::label::RedactedLabel;

/// Outcome of a timed call. Kept enum-shaped so the corresponding
/// `failures_total{reason}` counter dimension stays low-cardinality —
/// every variant maps to a single static label string.
///
/// Failures carry a `reason` produced by the call site as a
/// [`RedactedLabel`]. The label values pinned by callers MUST come
/// from `redacted!("literal")` — see `docs/observability.md`.
#[derive(Debug)]
pub enum Outcome {
    /// Call succeeded.
    Success,
    /// Call failed; `reason` is an enum-shaped low-cardinality label
    /// (e.g. `redacted!("missing_api_key")`, `redacted!("io_error")`).
    Failure { reason: RedactedLabel },
}

impl Outcome {
    /// Convenience: build a `Failure` from a `RedactedLabel` reason.
    pub fn failure(reason: RedactedLabel) -> Self {
        Self::Failure { reason }
    }
}

/// Run `f` while timing it; emit a duration histogram + (on failure)
/// the failures counter. The label dimension `op` is shared by both
/// metrics so dashboards can pivot on it without label-set drift.
///
/// # Naming
///
/// - `duration_metric` — the histogram name (must end in `_seconds`).
/// - `failures_metric` — the counter name (must end in `_total`).
///
/// Pass them as [`metric_name!`]-validated `&'static str`s so a typo
/// flunks the validator at first call.
///
/// # Why two metric names per call
///
/// Histograms and counters live in separate Prometheus type spaces;
/// you can't `count_by_outcome` against a histogram. Pairing them lets
/// the dashboard render p50/p95 latency from one and "failures per
/// second per reason" from the other without label cardinality
/// blowing up.
///
/// # Sync vs async
///
/// This is the sync flavour — used by `heron-vault` (filesystem ops
/// are blocking calls). The LLM crate uses [`timed_io_async`].
pub fn timed_io_sync<F, T, E>(
    duration_metric: &'static str,
    failures_metric: &'static str,
    op_dim: (&'static str, RedactedLabel),
    f: F,
) -> Result<T, E>
where
    F: FnOnce() -> Result<T, E>,
    E: ClassifyFailure,
{
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed().as_secs_f64();
    record_outcome(
        duration_metric,
        failures_metric,
        op_dim,
        elapsed,
        match &result {
            Ok(_) => Outcome::Success,
            Err(e) => Outcome::failure(e.failure_reason()),
        },
    );
    result
}

/// Async variant of [`timed_io_sync`]. Same contract — runs the
/// future, records duration, emits a failure counter on `Err`. Used
/// by `heron-llm` for the API/CLI summarize paths.
pub async fn timed_io_async<Fut, T, E>(
    duration_metric: &'static str,
    failures_metric: &'static str,
    op_dim: (&'static str, RedactedLabel),
    fut: Fut,
) -> Result<T, E>
where
    Fut: std::future::Future<Output = Result<T, E>>,
    E: ClassifyFailure,
{
    let start = Instant::now();
    let result = fut.await;
    let elapsed = start.elapsed().as_secs_f64();
    record_outcome(
        duration_metric,
        failures_metric,
        op_dim,
        elapsed,
        match &result {
            Ok(_) => Outcome::Success,
            Err(e) => Outcome::failure(e.failure_reason()),
        },
    );
    result
}

/// Internal shared write path. Records the duration histogram on every
/// call, and on `Outcome::Failure` also bumps the failure counter with
/// the additional `reason` dimension.
///
/// Cloning the `op_dim` label on emit: the `metrics` crate's macro
/// signature wants an owned `Cow<'static, str>` per label value, and
/// histograms + counters are emitted separately. Cloning a
/// `RedactedLabel` (which is a thin newtype around `String`) once per
/// call site is the right tradeoff vs threading a `&'static` map
/// through.
fn record_outcome(
    duration_metric: &'static str,
    failures_metric: &'static str,
    op_dim: (&'static str, RedactedLabel),
    elapsed_secs: f64,
    outcome: Outcome,
) {
    let (op_key, op_label) = op_dim;
    metrics::histogram!(
        duration_metric,
        op_key => op_label.clone().into_inner(),
    )
    .record(elapsed_secs);
    if let Outcome::Failure { reason } = outcome {
        metrics::counter!(
            failures_metric,
            op_key => op_label.into_inner(),
            "reason" => reason.into_inner(),
        )
        .increment(1);
    }
}

/// Maps an error type to a low-cardinality `reason` label. Implementors
/// MUST return a `RedactedLabel` constructed from a `redacted!("literal")`
/// macro — never a `format!()`-ed string. The trait exists rather than
/// taking a closure parameter because every call site for a given
/// error type wants the same reason mapping; defining it once on the
/// type keeps the per-call-site noise minimal AND makes the mapping
/// auditable (one impl per error enum, all reason variants visible in
/// one place).
pub trait ClassifyFailure {
    /// Return the `reason` label for this error.
    fn failure_reason(&self) -> RedactedLabel;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::recorder::init_prometheus_recorder;
    use crate::redacted;

    #[derive(Debug)]
    struct TestError {
        kind: &'static str,
    }

    impl ClassifyFailure for TestError {
        fn failure_reason(&self) -> RedactedLabel {
            match self.kind {
                "io" => redacted!("io_error"),
                "parse" => redacted!("parse_error"),
                _ => redacted!("other"),
            }
        }
    }

    #[test]
    fn timed_io_sync_emits_duration_on_success() {
        let handle = init_prometheus_recorder().expect("recorder");
        let result: Result<u32, TestError> = timed_io_sync(
            crate::metric_name!("timed_io_test_success_seconds"),
            crate::metric_name!("timed_io_test_success_failures_total"),
            ("op", redacted!("smoke_success")),
            || Ok(42),
        );
        assert_eq!(result.expect("ok"), 42);
        let body = handle.render();
        assert!(
            body.contains("timed_io_test_success_seconds"),
            "histogram missing: {body}"
        );
        // Successful call must NOT bump the failure counter.
        assert!(
            !body.contains("timed_io_test_success_failures_total"),
            "failure counter incremented on success: {body}"
        );
    }

    #[test]
    fn timed_io_sync_emits_failure_counter_on_err() {
        let handle = init_prometheus_recorder().expect("recorder");
        let result: Result<u32, TestError> = timed_io_sync(
            crate::metric_name!("timed_io_test_failure_seconds"),
            crate::metric_name!("timed_io_test_failure_failures_total"),
            ("op", redacted!("smoke_failure")),
            || Err(TestError { kind: "io" }),
        );
        assert!(result.is_err());
        let body = handle.render();
        assert!(
            body.contains("timed_io_test_failure_seconds"),
            "histogram missing: {body}"
        );
        assert!(
            body.contains("timed_io_test_failure_failures_total"),
            "failure counter missing: {body}"
        );
        assert!(
            body.contains("reason=\"io_error\""),
            "reason label not propagated: {body}"
        );
    }

    #[tokio::test]
    async fn timed_io_async_emits_duration_on_success() {
        let handle = init_prometheus_recorder().expect("recorder");
        let result: Result<&'static str, TestError> = timed_io_async(
            crate::metric_name!("timed_io_async_success_seconds"),
            crate::metric_name!("timed_io_async_success_failures_total"),
            ("op", redacted!("async_smoke")),
            async { Ok("ok") },
        )
        .await;
        assert_eq!(result.expect("ok"), "ok");
        let body = handle.render();
        assert!(
            body.contains("timed_io_async_success_seconds"),
            "histogram missing: {body}"
        );
    }

    #[tokio::test]
    async fn timed_io_async_records_failure_counter() {
        let handle = init_prometheus_recorder().expect("recorder");
        let result: Result<u32, TestError> = timed_io_async(
            crate::metric_name!("timed_io_async_failure_seconds"),
            crate::metric_name!("timed_io_async_failure_failures_total"),
            ("op", redacted!("async_failure")),
            async { Err(TestError { kind: "parse" }) },
        )
        .await;
        assert!(result.is_err());
        let body = handle.render();
        assert!(
            body.contains("timed_io_async_failure_failures_total"),
            "failure counter missing: {body}"
        );
        assert!(
            body.contains("reason=\"parse_error\""),
            "reason label missing: {body}"
        );
    }
}
