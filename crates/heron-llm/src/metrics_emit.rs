//! Metric emission for LLM call sites.
//!
//! Owns the metric *name* constants and the `record_*` helpers for
//! the post-call counters (tokens in/out, cost). The shared timing
//! wrapper [`heron_metrics::timed_io_async`] handles
//! `llm_call_duration_seconds` and `llm_call_failures_total`; this
//! module covers the success-only emissions that depend on
//! `Usage`/`Cost` fields not visible to a generic timing wrapper.
//!
//! See `docs/observability.md` §"LLM metrics" for the full surface.

use heron_metrics::{ClassifyFailure, RedactedLabel, redacted};

use crate::LlmError;

// Metric names. Convention validation happens in `metric_names_match_convention`
// below — it covers every const here so a drifted literal flunks
// `cargo test -p heron-llm` before reaching production.
pub(crate) const LLM_CALL_DURATION_SECONDS: &str = "llm_call_duration_seconds";
pub(crate) const LLM_CALL_FAILURES_TOTAL: &str = "llm_call_failures_total";
pub(crate) const LLM_TOKENS_INPUT_TOTAL: &str = "llm_tokens_input_total";
pub(crate) const LLM_TOKENS_OUTPUT_TOTAL: &str = "llm_tokens_output_total";
/// Cost is reported as integer micro-USD (USD × 1_000_000) so a
/// strictly-monotonic counter is well-defined and the unit prefix
/// matches the metric name. See `docs/observability.md` §"LLM cost
/// counter shape" for rationale and dashboard division (divide by
/// 1_000_000 to recover USD).
pub(crate) const LLM_COST_USD_MICRO_TOTAL: &str = "llm_cost_usd_micro_total";

impl ClassifyFailure for LlmError {
    fn failure_reason(&self) -> RedactedLabel {
        match self {
            Self::NotYetImplemented => redacted!("not_yet_implemented"),
            Self::Backend(_) => redacted!("backend_error"),
            Self::Parse(_) => redacted!("parse_error"),
            Self::IdPreservationTooLow { .. } => redacted!("id_preservation_too_low"),
            Self::MissingApiKey => redacted!("missing_api_key"),
            Self::Io(_) => redacted!("io_error"),
        }
    }
}

/// Record post-success token counts + cost. Called from each backend's
/// success path with the `RedactedLabel`s produced by
/// [`crate::metrics_labels`]. The cost counter uses integer
/// micro-USD (USD × 1_000_000) so it's monotonic and lossless for the
/// 4-decimal precision `cost::compute_cost` rounds to.
pub(crate) fn record_call_success(
    backend: RedactedLabel,
    model: RedactedLabel,
    tokens_in: u64,
    tokens_out: u64,
    cost_usd: f64,
) {
    metrics::counter!(
        LLM_TOKENS_INPUT_TOTAL,
        "backend" => backend.clone().into_inner(),
        "model" => model.clone().into_inner(),
    )
    .increment(tokens_in);
    metrics::counter!(
        LLM_TOKENS_OUTPUT_TOTAL,
        "backend" => backend.clone().into_inner(),
        "model" => model.clone().into_inner(),
    )
    .increment(tokens_out);
    // 4-decimal USD → micro-USD by ×1_000_000. `compute_cost` already
    // rounds to 4dp so the multiplication is exact-integer for the
    // values we observe; saturate to u64 as a final guard against
    // a future cost-model change introducing larger fractional
    // values. The 1_000_000 factor matches the `_micro_` unit prefix
    // (1 USD = 10^6 micro-USD) and gives extra fidelity for any
    // future rate model that quotes beyond 4dp.
    let micro_usd = (cost_usd * 1_000_000.0).round();
    let micro_usd_u64 = if micro_usd.is_finite() && micro_usd >= 0.0 {
        // Saturating cast: f64 → u64 saturates rather than wrapping
        // on overflow for `as u64` since Rust 1.45, so a NaN-or-inf
        // protected positive value lands in [0, u64::MAX] safely.
        micro_usd as u64
    } else {
        0
    };
    metrics::counter!(
        LLM_COST_USD_MICRO_TOTAL,
        "backend" => backend.into_inner(),
        "model" => model.into_inner(),
    )
    .increment(micro_usd_u64);
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_metrics::{init_prometheus_recorder, redacted, validate_metric_name};

    #[test]
    fn metric_names_match_convention() {
        // Drifted literals (missing `_total`, hyphens, uppercase) flunk
        // here before reaching production. The names live as `const &str`
        // because `metric_name!` macro expansion isn't const-evaluable in
        // `static` contexts on stable; this test is the equivalent
        // first-call validator.
        for name in [
            LLM_CALL_DURATION_SECONDS,
            LLM_CALL_FAILURES_TOTAL,
            LLM_TOKENS_INPUT_TOTAL,
            LLM_TOKENS_OUTPUT_TOTAL,
            LLM_COST_USD_MICRO_TOTAL,
        ] {
            validate_metric_name(name)
                .unwrap_or_else(|e| panic!("metric name {name:?} drifted: {e}"));
        }
    }

    #[test]
    fn classify_failure_covers_every_variant() {
        // Exhaustive: every variant of `LlmError` must produce a
        // distinct enum-shaped reason. A new variant added without
        // updating the impl would compile-fail in `match`.
        let cases: &[(LlmError, &str)] = &[
            (LlmError::NotYetImplemented, "not_yet_implemented"),
            (LlmError::Backend("synthetic".into()), "backend_error"),
            (LlmError::Parse("synthetic".into()), "parse_error"),
            (
                LlmError::IdPreservationTooLow {
                    observed: 0.0,
                    required: 95.0,
                },
                "id_preservation_too_low",
            ),
            (LlmError::MissingApiKey, "missing_api_key"),
            (LlmError::Io(std::io::Error::other("synthetic")), "io_error"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.failure_reason().as_str(), *expected, "for {err:?}");
        }
    }

    #[test]
    fn record_call_success_emits_three_counters() {
        let handle = init_prometheus_recorder().expect("recorder");
        record_call_success(
            redacted!("anthropic"),
            redacted!("claude_sonnet_4_6"),
            1_500,
            300,
            0.0123,
        );
        let body = handle.render();
        assert!(
            body.contains("llm_tokens_input_total"),
            "missing tokens_input: {body}"
        );
        assert!(
            body.contains("llm_tokens_output_total"),
            "missing tokens_output: {body}"
        );
        assert!(
            body.contains("llm_cost_usd_micro_total"),
            "missing cost counter: {body}"
        );
        // Pin the labels so a future privacy-leak attempt (a free-form
        // model string sneaking into the call site) would surface here
        // as a spurious time series.
        assert!(
            body.contains("backend=\"anthropic\""),
            "backend label drift: {body}"
        );
        assert!(
            body.contains("model=\"claude_sonnet_4_6\""),
            "model label drift: {body}"
        );
    }

    #[test]
    fn cost_counter_handles_non_finite_input() {
        // Defence-in-depth: a NaN / infinite cost (which compute_cost
        // doesn't produce today, but a future calibration tweak might
        // accidentally) must not panic and must not poison the counter
        // with an enormous value. Use a unique backend label so prior
        // tests in the same process can't pollute the assertion.
        let handle = init_prometheus_recorder().expect("recorder");
        record_call_success(
            redacted!("nan_test_backend"),
            redacted!("nan_test_model"),
            10,
            10,
            f64::NAN,
        );
        let body = handle.render();
        // The cost counter MUST report 0 for a NaN input, not a
        // wrapped/saturated huge value. Pin the exact line so any
        // future tweak to the saturation logic that re-introduces a
        // poisoned sample fails closed.
        let cost_line = body
            .lines()
            .find(|l| {
                l.starts_with("llm_cost_usd_micro_total{")
                    && l.contains("backend=\"nan_test_backend\"")
            })
            .unwrap_or_else(|| panic!("missing cost counter line:\n{body}"));
        let value: u64 = cost_line
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("malformed cost counter line: {cost_line}"));
        assert_eq!(value, 0, "NaN cost must clamp to zero, got {value}");
    }
}
