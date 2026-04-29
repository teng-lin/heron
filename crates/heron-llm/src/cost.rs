//! Cost calibration per §11.4.
//!
//! Pure offline math: convert `(model, tokens_in, tokens_out)` into a
//! USD `Cost`. The on-the-wire model rates ship as compile-time
//! constants and *can* drift; the §11.4 calibration loop in week 9
//! tightens them against real Anthropic invoices. Until then the
//! values come from the published price list at the time of writing.
//!
//! The function is offline + deterministic, so the diagnostics tab
//! (§15.4) and the session-summary log line (§19.2) can both compute
//! cost without an API call.

use heron_types::Cost;
use thiserror::Error;

/// USD price-per-million-tokens, broken out by direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// Input (prompt) tokens.
    pub input_per_million: f64,
    /// Output (completion) tokens.
    pub output_per_million: f64,
}

/// One entry in the §11.4 calibration table. The model strings match
/// what the Anthropic API returns in `usage.model` / what the Codex
/// CLI prints, so callers can pass that through verbatim.
#[derive(Debug, Clone, Copy)]
pub struct ModelRate {
    pub model: &'static str,
    pub pricing: ModelPricing,
}

/// Published rates as of writing (USD per million tokens). The §11.4
/// calibration loop tightens these against real invoices in week 9 and
/// updates this table; `compute_cost` always reads from this list.
///
/// OpenAI rates (early 2026 published pricing):
///   gpt-4o-mini: $0.15/M input, $0.60/M output
///   gpt-4o:      $2.50/M input, $10.00/M output
/// Anthropic rates sourced from the public pricing page at time of writing.
pub const RATE_TABLE: &[ModelRate] = &[
    ModelRate {
        model: "claude-opus-4-7",
        pricing: ModelPricing {
            input_per_million: 15.0,
            output_per_million: 75.0,
        },
    },
    ModelRate {
        model: "claude-sonnet-4-6",
        pricing: ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        },
    },
    ModelRate {
        model: "claude-haiku-4-5",
        pricing: ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
        },
    },
    // OpenAI models (early 2026 public pricing).
    ModelRate {
        model: "gpt-4o-mini",
        pricing: ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
        },
    },
    ModelRate {
        model: "gpt-4o",
        pricing: ModelPricing {
            input_per_million: 2.50,
            output_per_million: 10.00,
        },
    },
];

#[derive(Debug, Error, PartialEq)]
pub enum CostError {
    #[error("unknown model {0:?}; add to heron_llm::cost::RATE_TABLE")]
    UnknownModel(String),
}

/// Look up pricing for a model name. Matches the prefix to tolerate
/// model-version suffixes the API attaches (e.g. `-20251001`).
pub fn lookup_pricing(model: &str) -> Result<ModelPricing, CostError> {
    if let Some(rate) = RATE_TABLE.iter().find(|r| model.starts_with(r.model)) {
        Ok(rate.pricing)
    } else {
        Err(CostError::UnknownModel(model.to_owned()))
    }
}

/// Compute the USD cost of a single LLM call.
///
/// Returns a fully-populated [`Cost`] including the model name so the
/// review UI's diagnostics tab can render `$0.0123 / 14231 in /
/// 612 out / claude-sonnet-4-6` directly without re-deriving anything.
pub fn compute_cost(model: &str, tokens_in: u64, tokens_out: u64) -> Result<Cost, CostError> {
    let p = lookup_pricing(model)?;
    let summary_usd = (tokens_in as f64) * p.input_per_million / 1_000_000.0
        + (tokens_out as f64) * p.output_per_million / 1_000_000.0;
    Ok(Cost {
        summary_usd: round_cents(summary_usd, 4),
        tokens_in,
        tokens_out,
        model: model.to_owned(),
    })
}

/// Round to N decimal places. Used to keep the displayed dollar amount
/// stable across runs (avoid `0.040999999999999998`).
fn round_cents(usd: f64, places: u32) -> f64 {
    let factor = 10f64.powi(places as i32);
    (usd * factor).round() / factor
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sonnet_4_6_priced_as_published() {
        // 1k in + 1k out at $3/$15 per M ⇒ $0.003 + $0.015 = $0.018.
        let cost = compute_cost("claude-sonnet-4-6", 1_000, 1_000).expect("cost");
        assert_eq!(cost.summary_usd, 0.018);
        assert_eq!(cost.tokens_in, 1_000);
        assert_eq!(cost.tokens_out, 1_000);
        assert_eq!(cost.model, "claude-sonnet-4-6");
    }

    #[test]
    fn opus_4_7_pricier_than_sonnet() {
        let opus = compute_cost("claude-opus-4-7", 10_000, 1_000).expect("opus");
        let sonnet = compute_cost("claude-sonnet-4-6", 10_000, 1_000).expect("sonnet");
        assert!(
            opus.summary_usd > sonnet.summary_usd,
            "opus must cost more than sonnet for the same prompt"
        );
    }

    #[test]
    fn version_suffix_resolves_to_base_model() {
        // The Anthropic API often returns model identifiers with a
        // date suffix (e.g. claude-haiku-4-5-20251001). The lookup
        // should still find the rate.
        let cost = compute_cost("claude-haiku-4-5-20251001", 1_000, 0).expect("haiku-suffixed");
        assert_eq!(cost.summary_usd, 0.001);
    }

    #[test]
    fn unknown_model_errors_with_name() {
        let err = compute_cost("gpt-99-supergiant", 1, 1).expect_err("unknown");
        assert_eq!(err, CostError::UnknownModel("gpt-99-supergiant".to_owned()));
    }

    #[test]
    fn zero_tokens_costs_zero() {
        let cost = compute_cost("claude-sonnet-4-6", 0, 0).expect("zero");
        assert_eq!(cost.summary_usd, 0.0);
    }

    #[test]
    fn cost_table_includes_openai_models() {
        // Pins the table so a future model rename silently zeros cost.
        let mini = compute_cost("gpt-4o-mini", 1_000_000, 1_000_000).expect("gpt-4o-mini");
        assert_eq!(mini.summary_usd, 0.15 + 0.60);
        assert_eq!(mini.model, "gpt-4o-mini");

        let full = compute_cost("gpt-4o", 1_000_000, 1_000_000).expect("gpt-4o");
        assert_eq!(full.summary_usd, 2.50 + 10.00);
        assert_eq!(full.model, "gpt-4o");

        // gpt-4o must be more expensive than gpt-4o-mini
        assert!(
            full.summary_usd > mini.summary_usd,
            "gpt-4o must cost more than gpt-4o-mini"
        );
    }

    #[test]
    fn rounding_truncates_floating_point_noise() {
        // 14_231 in × $3/M  = $0.042693
        // 612    out × $15/M = $0.009180
        //                      $0.051873 → 4dp = $0.0519
        let cost = compute_cost("claude-sonnet-4-6", 14_231, 612).expect("calibration");
        assert_eq!(cost.summary_usd, 0.0519);
    }
}
