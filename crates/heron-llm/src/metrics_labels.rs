//! Pinned `RedactedLabel` values for LLM metrics.
//!
//! Per #225's privacy contract: every metric label flowing out of
//! this crate MUST be one of the literal values defined here. There
//! is no `format!`, no `model_id.as_str()`-style escape — every
//! `redacted!("…")` call site is a string literal that the macro's
//! `:literal` matcher rejects at parse time if anything dynamic is
//! passed.
//!
//! Reviewers can cross-check this module against the four backends
//! enumerated by [`crate::Backend`] and the rate table in
//! [`crate::cost::RATE_TABLE`]. A new backend or a new model in the
//! rate table needs a corresponding label here, otherwise its metric
//! emissions fall through to the `unknown` arms — visible in
//! dashboards as `model="unknown_model"` so an operator can spot the
//! omission at audit time.

use heron_metrics::{RedactedLabel, redacted};

use crate::Backend;

/// Map [`Backend`] to the pinned `backend` label. Closed enum →
/// every variant has a literal mapping.
pub(crate) fn backend_label(backend: Backend) -> RedactedLabel {
    match backend {
        Backend::Anthropic => redacted!("anthropic"),
        Backend::OpenAI => redacted!("openai"),
        Backend::ClaudeCodeCli => redacted!("claude_code_cli"),
        Backend::CodexCli => redacted!("codex_cli"),
    }
}

/// Map a model identifier (as returned by the API or stamped onto a
/// CLI cost record) to the pinned `model` label.
///
/// Matches by prefix so the Anthropic API's date-suffixed identifiers
/// (e.g. `claude-haiku-4-5-20251001`) collapse onto the family label.
/// **Never** returns the raw input: an unknown model produces
/// `redacted!("unknown_model")` so the metric label cardinality stays
/// bounded even when a future model ships before the table is updated.
pub(crate) fn model_label(model: &str) -> RedactedLabel {
    if model.starts_with("claude-opus-4-7") {
        redacted!("claude_opus_4_7")
    } else if model.starts_with("claude-sonnet-4-6") {
        redacted!("claude_sonnet_4_6")
    } else if model.starts_with("claude-haiku-4-5") {
        redacted!("claude_haiku_4_5")
    } else if model.starts_with("gpt-4o-mini") {
        redacted!("gpt_4o_mini")
    } else if model.starts_with("gpt-4o") {
        redacted!("gpt_4o")
    } else if model.starts_with("claude-code-cli") {
        redacted!("claude_code_cli")
    } else if model.starts_with("codex-cli") {
        redacted!("codex_cli")
    } else {
        redacted!("unknown_model")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn backend_label_covers_all_variants() {
        // Compile-time exhaustiveness: a new `Backend` variant added
        // without updating this module fails the match.
        for backend in [
            Backend::Anthropic,
            Backend::OpenAI,
            Backend::ClaudeCodeCli,
            Backend::CodexCli,
        ] {
            let label = backend_label(backend);
            assert!(!label.as_str().is_empty());
            // Charset enforced at construction; double-check the label
            // doesn't contain a hyphen (snake_case discipline).
            assert!(!label.as_str().contains('-'));
        }
    }

    #[test]
    fn model_label_collapses_known_prefixes() {
        assert_eq!(
            model_label("claude-sonnet-4-6").as_str(),
            "claude_sonnet_4_6"
        );
        assert_eq!(
            model_label("claude-haiku-4-5-20251001").as_str(),
            "claude_haiku_4_5",
            "date-suffixed identifiers must collapse onto family label"
        );
        assert_eq!(model_label("gpt-4o-mini").as_str(), "gpt_4o_mini");
        assert_eq!(model_label("gpt-4o").as_str(), "gpt_4o");
        assert_eq!(model_label("gpt-4o-2024-08-06").as_str(), "gpt_4o");
    }

    #[test]
    fn model_label_unknown_returns_bounded_placeholder() {
        // Critical privacy / cardinality invariant: an unrecognized
        // model must NOT echo back the input string; instead a fixed
        // bucket name. Otherwise an attacker who can choose a model
        // string in settings could flood the time-series DB.
        assert_eq!(model_label("foo-bar-baz").as_str(), "unknown_model");
        assert_eq!(model_label("../../etc/passwd").as_str(), "unknown_model");
        assert_eq!(model_label("").as_str(), "unknown_model");
    }
}
