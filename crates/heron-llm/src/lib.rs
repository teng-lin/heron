//! `heron-llm` — meeting summarization.
//!
//! v0 surface from [`docs/archives/implementation.md`](../../../docs/archives/implementation.md)
//! §11.1 + §11.2. The real backends (Anthropic API, Claude Code CLI,
//! Codex CLI) plug into the [`Summarizer`] trait in week 9; the trait
//! shape and the `meeting.hbs` template ship now so `heron_vault`
//! merge integration tests can exercise the ID-preservation contract
//! end-to-end without an API key.

use std::path::Path;

use async_trait::async_trait;
use handlebars::Handlebars;
use heron_types::{ActionItem, Attendee, Cost, MeetingType};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod anthropic;
pub mod claude_code;
pub mod codex;
mod content;
pub mod cost;
pub mod key_resolver;
pub mod select;
pub mod transcript;

/// Crate-wide test helpers. Today this only ships `ENV_LOCK` — the
/// single mutex that every test that mutates `ANTHROPIC_API_KEY` /
/// `OPENAI_API_KEY` MUST hold. Multiple modules (`anthropic`,
/// `key_resolver`) touch the same env vars; two module-private mutexes
/// would race each other and produce flaky CI. PR-μ / phase 74.
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::Mutex;

    /// Single source of truth for env-mutation serialization across
    /// the crate. Locking this mutex MUST happen before any
    /// `std::env::set_var` / `std::env::remove_var` so two tests don't
    /// stomp on each other's view of the same env var.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());
}

pub use anthropic::{AnthropicClient, AnthropicClientConfig};
pub use claude_code::{ClaudeCodeClient, ClaudeCodeClientConfig};
pub use codex::{CodexClient, CodexClientConfig};
pub use cost::{CostError, ModelPricing, ModelRate, RATE_TABLE, compute_cost, lookup_pricing};
pub use key_resolver::{EnvKeyResolver, KeyName, KeyResolveError, KeyResolver};
pub use select::{
    Availability, Preference, SelectError, SelectionReason, select_backend, select_summarizer,
    select_summarizer_with_resolver,
};

/// Convenience alias so the public surface doesn't leak `String` for
/// tag fields. Not a newtype yet; v1.1 may tighten.
pub type Tag = String;

const MEETING_TEMPLATE: &str = include_str!("../templates/meeting.hbs");
const MEETING_TEMPLATE_NAME: &str = "meeting";

/// Inputs to a single summarize call.
///
/// On first summarize, `existing_action_items` and
/// `existing_attendees` are `None`. On re-summarize the caller passes
/// them from the **current** `<note>.md` (not `.md.bak`) — see §11.2.
#[derive(Debug)]
pub struct SummarizerInput<'a> {
    pub transcript: &'a Path,
    pub meeting_type: MeetingType,
    pub existing_action_items: Option<&'a [ActionItem]>,
    pub existing_attendees: Option<&'a [Attendee]>,
}

/// Structured LLM output. See `meeting.hbs` for the JSON shape the
/// LLM is asked to produce; this is the parsed form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummarizerOutput {
    pub body: String,
    pub company: Option<String>,
    pub meeting_type: MeetingType,
    pub tags: Vec<Tag>,
    pub action_items: Vec<ActionItem>,
    pub attendees: Vec<Attendee>,
    pub cost: Cost,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("not yet implemented (arrives week 9 per §11)")]
    NotYetImplemented,
    #[error("backend HTTP / IO error: {0}")]
    Backend(String),
    #[error("LLM returned malformed JSON: {0}")]
    Parse(String),
    #[error("ID preservation rate {observed:.0}% < required {required:.0}%; week-8 §10.5")]
    IdPreservationTooLow { observed: f32, required: f32 },
    #[error("ANTHROPIC_API_KEY is unset or empty; export it before running summarize")]
    MissingApiKey,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Backends per `plan.md` §5 weeks 7–8 + `docs/archives/implementation.md`
/// §11.1. Selection is runtime-configurable so the user can pick the
/// cheapest viable option per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Bare-`reqwest` Anthropic API client (primary).
    Anthropic,
    /// Spawn `claude -p` and parse its output.
    ClaudeCodeCli,
    /// Spawn `codex exec` and parse its output.
    CodexCli,
}

#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError>;
}

/// Build a [`Summarizer`] for the requested backend, reading API keys
/// from the process environment via [`EnvKeyResolver`].
///
/// `Anthropic` returns the real reqwest-backed client when
/// `ANTHROPIC_API_KEY` is present in the environment; without the
/// key (CI / offline) it falls back to a stub that returns
/// [`LlmError::NotYetImplemented`]. The other two backends still
/// stub through to the wire shape land in their respective phases.
///
/// Callers that need a configured client (custom base URL, model,
/// timeout) should construct [`AnthropicClient`] directly and pass
/// it as a boxed [`Summarizer`].
///
/// Desktop callers that want the macOS Keychain to act as a fallback
/// when the env var is unset should use
/// [`build_summarizer_with_resolver`] with an
/// `EnvThenKeychainResolver` (defined in
/// `apps/desktop/src-tauri/src/keychain_resolver.rs`).
pub fn build_summarizer(backend: Backend) -> Box<dyn Summarizer> {
    build_summarizer_with_resolver(backend, &EnvKeyResolver)
}

/// Build a [`Summarizer`] for the requested backend, resolving API
/// keys via the supplied [`KeyResolver`].
///
/// PR-μ (phase 74) hook: the desktop crate threads its
/// `EnvThenKeychainResolver` through here so a user who pasted their
/// key into Settings → Summarizer (PR-θ) can summarize without ever
/// exporting the env var. The CLI binary keeps using `EnvKeyResolver`
/// via [`build_summarizer`].
pub fn build_summarizer_with_resolver(
    backend: Backend,
    resolver: &dyn KeyResolver,
) -> Box<dyn Summarizer> {
    match backend {
        Backend::Anthropic => match AnthropicClientConfig::from_resolver(resolver) {
            Ok(cfg) => match AnthropicClient::new(cfg) {
                Ok(c) => Box::new(c),
                // Fall through to the stub on construction failure
                // rather than panicking — the orchestrator should
                // never explode on startup just because rustls
                // didn't load in this build.
                Err(e) => {
                    tracing::warn!(
                        "AnthropicClient construction failed; falling back to stub: {e}"
                    );
                    Box::new(stub::AnthropicStub)
                }
            },
            Err(e) => {
                // Loud-but-non-fatal: a user who picked
                // `Backend::Anthropic` and forgot the env var
                // should know the call will return
                // NotYetImplemented rather than 401.
                tracing::warn!(
                    "Anthropic backend selected but {e}; \
                     summarize calls will return NotYetImplemented"
                );
                Box::new(stub::AnthropicStub)
            }
        },
        Backend::ClaudeCodeCli => {
            // Construct lazily — `claude` not being on PATH at
            // launch shouldn't crash the orchestrator. The first
            // summarize call will surface the spawn error if the
            // binary actually goes missing.
            Box::new(ClaudeCodeClient::new(ClaudeCodeClientConfig::default()))
        }
        Backend::CodexCli => Box::new(CodexClient::new(CodexClientConfig::default())),
    }
}

/// Render the meeting prompt for the given input. Used by every
/// backend. Exposed so consumers can inspect the prompt the LLM
/// will see (useful for the diagnostics tab in §15.4).
pub fn render_meeting_prompt(input: &SummarizerInput<'_>) -> Result<String, LlmError> {
    let mut hb = Handlebars::new();
    hb.set_strict_mode(true);
    hb.register_helper("eq", Box::new(helpers::eq));
    hb.register_template_string(MEETING_TEMPLATE_NAME, MEETING_TEMPLATE)
        .map_err(|e| LlmError::Backend(format!("template register: {e}")))?;

    let ctx = serde_json::json!({
        "transcript": input.transcript.display().to_string(),
        "meeting_type": meeting_type_str(input.meeting_type),
        "existing_action_items": input.existing_action_items.map(|s| {
            s.iter()
                .map(|a| serde_json::json!({
                    "id": a.id.to_string(),
                    "owner": a.owner,
                    "text": a.text,
                    "due": a.due,
                }))
                .collect::<Vec<_>>()
        }),
        "existing_attendees": input.existing_attendees.map(|s| {
            s.iter()
                .map(|a| serde_json::json!({
                    "id": a.id.to_string(),
                    "name": a.name,
                    "company": a.company,
                }))
                .collect::<Vec<_>>()
        }),
    });
    hb.render(MEETING_TEMPLATE_NAME, &ctx)
        .map_err(|e| LlmError::Backend(format!("template render: {e}")))
}

fn meeting_type_str(m: MeetingType) -> &'static str {
    match m {
        MeetingType::Client => "client",
        MeetingType::Internal => "internal",
        MeetingType::OneOnOne => "1:1",
        MeetingType::Other => "other",
    }
}

mod helpers {
    use handlebars::{
        Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
    };

    pub fn eq(
        h: &Helper<'_>,
        _: &Handlebars<'_>,
        _: &Context,
        _: &mut RenderContext<'_, '_>,
        out: &mut dyn Output,
    ) -> HelperResult {
        let lhs = h
            .param(0)
            .ok_or(RenderErrorReason::ParamNotFoundForIndex("eq", 0))?
            .value();
        let rhs = h
            .param(1)
            .ok_or(RenderErrorReason::ParamNotFoundForIndex("eq", 1))?
            .value();
        out.write(if lhs == rhs { "true" } else { "" })?;
        Ok(())
    }
}

mod stub {
    use super::{LlmError, Summarizer, SummarizerInput, SummarizerOutput};
    use async_trait::async_trait;

    /// Returned by `build_summarizer(Anthropic)` when
    /// `ANTHROPIC_API_KEY` is missing — the orchestrator can still
    /// build, the failure surfaces at first summarize call.
    pub struct AnthropicStub;

    #[async_trait]
    impl Summarizer for AnthropicStub {
        async fn summarize(
            &self,
            _input: SummarizerInput<'_>,
        ) -> Result<SummarizerOutput, LlmError> {
            Err(LlmError::NotYetImplemented)
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use heron_types::{ActionItem, Attendee, ItemId, MeetingType};

    fn next_id() -> ItemId {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ItemId::from_u128(u128::from(COUNTER.fetch_add(1, Ordering::Relaxed)))
    }

    #[test]
    fn template_includes_id_block_when_action_items_present() {
        let id = next_id();
        let items = vec![ActionItem {
            id,
            owner: "me".into(),
            text: "Send pricing deck".into(),
            due: None,
        }];
        let path = PathBuf::from("/tmp/x.jsonl");
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: Some(&items),
            existing_attendees: None,
        })
        .expect("render");
        assert!(prompt.contains("RETURN THE EXACT SAME `id`"));
        assert!(prompt.contains(&id.to_string()));
        assert!(prompt.contains("Send pricing deck"));
    }

    #[test]
    fn template_omits_id_block_on_first_summarize() {
        let path = PathBuf::from("/tmp/x.jsonl");
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
        })
        .expect("render");
        // The block header must NOT appear when there are no priors —
        // otherwise the LLM may invent UUIDs trying to "preserve"
        // things we never told it about.
        assert!(!prompt.contains("RETURN THE EXACT SAME `id`"));
    }

    #[test]
    fn template_includes_attendees_block_when_present() {
        let id = next_id();
        let attendees = vec![Attendee {
            id,
            name: "Alice".into(),
            company: Some("Acme".into()),
        }];
        let path = PathBuf::from("/tmp/x.jsonl");
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: Some(&attendees),
        })
        .expect("render");
        assert!(prompt.contains(&id.to_string()));
        assert!(prompt.contains("Alice"));
        assert!(prompt.contains("Acme"));
    }

    #[test]
    fn template_branches_on_meeting_type() {
        let path = PathBuf::from("/tmp/x.jsonl");
        let client = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
        })
        .expect("client");
        assert!(client.contains("EXTERNAL client meeting"));

        let internal = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Internal,
            existing_action_items: None,
            existing_attendees: None,
        })
        .expect("internal");
        assert!(internal.contains("INTERNAL team meeting"));

        let one_on_one = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::OneOnOne,
            existing_action_items: None,
            existing_attendees: None,
        })
        .expect("1:1");
        assert!(one_on_one.contains("This is a 1:1"));
    }

    #[tokio::test]
    async fn codex_cli_backend_attempts_real_spawn() {
        // CodexCli now wires a real CodexClient. Without `codex` on
        // PATH (CI runners don't have it), the call surfaces a
        // Backend variant rather than NotYetImplemented. Mirror the
        // ClaudeCodeCli test by pointing at a known-missing binary
        // for determinism.
        use crate::codex::CodexClientConfig;
        let cfg = CodexClientConfig {
            binary: std::path::PathBuf::from("/nonexistent/codex-binary"),
            timeout: std::time::Duration::from_secs(2),
            ..CodexClientConfig::default()
        };
        let client: Box<dyn Summarizer> = Box::new(crate::CodexClient::new(cfg));
        let path = PathBuf::from("/tmp/x.jsonl");
        let result = client
            .summarize(SummarizerInput {
                transcript: &path,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
            })
            .await;
        match result {
            Err(LlmError::Backend(_)) => {}
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claude_code_cli_backend_attempts_real_spawn() {
        // ClaudeCodeCli now wires a real ClaudeCodeClient. Without
        // a real `claude` on PATH, the call surfaces a spawn error
        // (Backend variant) rather than NotYetImplemented. We point
        // at a known-missing path to make the test deterministic.
        use crate::claude_code::ClaudeCodeClientConfig;
        let cfg = ClaudeCodeClientConfig {
            binary: std::path::PathBuf::from("/nonexistent/claude-binary"),
            timeout: std::time::Duration::from_secs(2),
            ..ClaudeCodeClientConfig::default()
        };
        let client: Box<dyn Summarizer> = Box::new(crate::ClaudeCodeClient::new(cfg));
        let path = PathBuf::from("/tmp/x.jsonl");
        let result = client
            .summarize(SummarizerInput {
                transcript: &path,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
            })
            .await;
        // Two possible failures depending on order of operations:
        // (a) read_transcript_capped on /tmp/x.jsonl fails first
        //     with Backend("open transcript ..."), or
        // (b) spawn fails first with Backend("spawn ...").
        // Either is fine — they're both Backend variants, neither
        // is NotYetImplemented.
        match result {
            Err(LlmError::Backend(_)) => {}
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn summarizer_output_serializes_round_trip() {
        let out = SummarizerOutput {
            body: "hello world".into(),
            company: Some("Acme".into()),
            meeting_type: MeetingType::Client,
            tags: vec!["acme".into()],
            action_items: vec![],
            attendees: vec![],
            cost: Cost {
                summary_usd: 0.04,
                tokens_in: 10_000,
                tokens_out: 500,
                model: "claude-sonnet-4-6".into(),
            },
        };
        let json = serde_json::to_string(&out).expect("serialize");
        let back: SummarizerOutput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.body, "hello world");
        assert_eq!(back.cost.summary_usd, 0.04);
    }
}
