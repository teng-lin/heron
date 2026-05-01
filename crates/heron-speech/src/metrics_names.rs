//! STT-pipeline metric names. Foundation conventions live in
//! `crates/heron-metrics`; rules in `docs/observability.md`.
//!
//! These metrics are emitted from a wrapper around
//! [`crate::SttBackend::transcribe`] (see [`crate::transcribe_with_metrics`])
//! so every backend (WhisperKit, Sherpa, the test stub) is covered
//! at one call site rather than each backend re-instrumenting its
//! own decoder. The wrapper co-emits with the existing
//! `tracing::warn!`/`tracing::error!` sites in the consumer
//! (`heron-pipeline::pipeline::run_stt`); metrics are for
//! dashboards, tracing for logs.

/// Histogram: wall-clock seconds the backend's `transcribe` call
/// took. Label `backend` is one of the pinned `redacted!` literals
/// (`whisperkit`, `sherpa`, `whisperkit_stub`); never a free-form
/// string. Recorded on both the success and the failure path so a
/// dashboard can correlate failure rate with latency (a backend
/// hitting a timeout shows up as both a `stt_failures_total{reason="timeout"}`
/// bump AND a long-tail `stt_duration_seconds` observation).
pub const STT_DURATION_SECONDS: &str = "stt_duration_seconds";

/// Counter: STT failures bucketed by an enum-shaped `reason` label
/// AND a `backend` label (mirroring the `stt_duration_seconds`
/// histogram dimension so dashboards can compute failure-rate-per-
/// backend with a clean `sum by (backend)` aggregation). Both
/// label values flow through pinned `redacted!` literals; free-form
/// strings are forbidden by the foundation's privacy posture.
///
/// Currently-emitted reasons:
///
/// - `model_unavailable` — the backend itself reports the model is
///   missing or the platform is not supported (e.g. WhisperKit on
///   Intel macOS pre-14). Maps from `SttError::ModelMissing`,
///   `SttError::Unavailable`, and (off-Apple stub only)
///   `SttError::NotYetImplemented`.
/// - `transcription_empty` — the backend produced zero turns.
///   Distinct from a hard failure because today the consumer treats
///   it as a soft fail (continues with the empty-transcript note),
///   but the dashboard answer "are we silently producing empty
///   transcripts?" is still load-bearing.
/// - `failed` — the backend's `transcribe` call returned
///   [`crate::SttError::Failed`]. The error message is NOT carried
///   on the label (it would cardinality-explode the time series and
///   smuggle user-content-derived strings); the matching
///   `tracing::warn!` carries the human-readable text.
/// - `io` — filesystem error reading the WAV / writing the partial
///   JSONL.
///
/// Reserved (NOT emitted today; named ahead of the corresponding
/// integration so a follow-up does not rename the dimension):
///
/// - `timeout` — reserved for a future timeout-wrapping layer.
///   None of the current `SttError` variants map to it; this
///   bucket is named here so the doc/dashboard surface is stable
///   when the timeout integration lands.
pub const STT_FAILURES_TOTAL: &str = "stt_failures_total";

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_metrics::validate_metric_name;

    #[test]
    fn all_names_pass_validation() {
        for name in [STT_DURATION_SECONDS, STT_FAILURES_TOTAL] {
            validate_metric_name(name)
                .unwrap_or_else(|e| panic!("metric name '{name}' violates convention: {e}"));
        }
    }
}
