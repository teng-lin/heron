//! Capture-lifecycle + salvage metric names instrumented from this
//! crate. Composes with the smoke metric
//! [`heron_metrics::SMOKE_CAPTURE_STARTED_TOTAL`] (instrumented in
//! [`crate::LocalSessionOrchestrator::start_capture`]); see
//! [`docs/observability.md`](../../../docs/observability.md) for the
//! foundation conventions and the redaction rules every label below
//! flows through.
//!
//! Why a module-level `pub(crate) const` rather than `metric_name!`
//! at each call site: the constants are referenced from tests in
//! `herond` to assert the rendered exposition contains them, and a
//! `const` is the cheapest way to share the canonical literal. The
//! convention is enforced by [`tests::all_names_pass_validation`]
//! (a `validate_metric_name` round-trip) so a drifted literal fails
//! the workspace test suite, mirroring the smoke-metric pattern from
//! the foundation PR.

/// Counter incremented in `end_meeting` (and the pipeline-finalisation
/// path) when a capture stops. Label `reason` is one of the pinned
/// `redacted!` enum literals — never a free-form `String`.
///
/// - `user_stop` — user (or HTTP caller) explicitly ended the meeting
///   via [`heron_session::SessionOrchestrator::end_meeting`]. Always
///   the reason emitted on the `end_meeting` path.
/// - `success` — pipeline reached `Done` with a finalised note. Emitted
///   from `complete_pipeline_meeting` once the v1 pipeline finalised
///   cleanly, distinct from `user_stop` so dashboards can separate
///   "user pressed stop" from "pipeline produced a note" — the same
///   capture lifecycle naturally fires both: `user_stop` when the
///   request handler returns, then `success` (or `error`) when the
///   background finalizer joins.
/// - `error` — pipeline failed to finalise (vault write failed, LLM
///   summarise errored, the FSM walk rejected the completion edge).
pub(crate) const CAPTURE_ENDED_TOTAL: &str = "capture_ended_total";

/// Gauge: number of currently-active captures. Incremented on every
/// `start_capture` that successfully transitioned to `Recording`,
/// decremented on every `end_meeting` that successfully claimed an
/// active entry. Sits above `1` only briefly when concurrent platforms
/// run in parallel (today every per-platform start_capture is
/// singleton-checked, so the steady state is `0` or `1`).
pub(crate) const CAPTURE_ACTIVE: &str = "capture_active_count";

/// Gauge: number of unfinished sessions discovered under the
/// orchestrator's cache root at startup. Mirrors what
/// `heron salvage` would print on the same machine.
pub(crate) const SALVAGE_CANDIDATES_PENDING: &str = "salvage_candidates_pending";

/// Counter incremented at the cache-retain decision in the v1 capture
/// pipeline finalisation. Outcome label is one of the pinned
/// `redacted!` enum literals:
///
/// - `recovered` — capture finished cleanly, m4a verified, cache was
///   purged (no salvage candidate left behind for the next launch).
/// - `abandoned` — capture finished but m4a verify failed (or hit a
///   transient encode error), so the WAV cache is retained for the
///   user to recover from. Each occurrence bumps the candidate set
///   that `salvage_candidates_pending` will count on the next boot.
/// - `failed` — capture aborted before STT (audio capture errored,
///   FSM rejected the finalisation edge); the partially-written
///   `state.json` + WAVs are still on disk for `heron salvage`.
pub(crate) const SALVAGE_RECOVERY_TOTAL: &str = "salvage_recovery_total";

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_metrics::validate_metric_name;

    /// Mirror of `heron_metrics::naming::tests::smoke_metric_name_passes_validation`
    /// — every metric name declared in this module flunks the
    /// workspace test suite if a future rename drifts the convention.
    #[test]
    fn all_names_pass_validation() {
        for name in [
            CAPTURE_ENDED_TOTAL,
            CAPTURE_ACTIVE,
            SALVAGE_CANDIDATES_PENDING,
            SALVAGE_RECOVERY_TOTAL,
        ] {
            validate_metric_name(name)
                .unwrap_or_else(|e| panic!("metric name '{name}' violates convention: {e}"));
        }
    }
}
