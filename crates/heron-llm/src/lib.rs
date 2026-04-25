//! `heron-llm` — meeting summarization.
//!
//! v0 surface from [`docs/implementation.md`](../../../docs/implementation.md)
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

pub mod cost;
pub use cost::{CostError, ModelPricing, ModelRate, RATE_TABLE, compute_cost, lookup_pricing};

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
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Backends per `plan.md` §5 weeks 7–8 + `docs/implementation.md`
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

/// Build a [`Summarizer`] for the requested backend.
///
/// Returns stub impls until week 9 lands the real wires; each stub
/// returns [`LlmError::NotYetImplemented`] from `summarize` so
/// downstream type signatures resolve without an API key.
pub fn build_summarizer(backend: Backend) -> Box<dyn Summarizer> {
    match backend {
        Backend::Anthropic => Box::new(stub::AnthropicStub),
        Backend::ClaudeCodeCli => Box::new(stub::ClaudeCodeStub),
        Backend::CodexCli => Box::new(stub::CodexStub),
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

    pub struct AnthropicStub;
    pub struct ClaudeCodeStub;
    pub struct CodexStub;

    #[async_trait]
    impl Summarizer for AnthropicStub {
        async fn summarize(
            &self,
            _input: SummarizerInput<'_>,
        ) -> Result<SummarizerOutput, LlmError> {
            Err(LlmError::NotYetImplemented)
        }
    }

    #[async_trait]
    impl Summarizer for ClaudeCodeStub {
        async fn summarize(
            &self,
            _input: SummarizerInput<'_>,
        ) -> Result<SummarizerOutput, LlmError> {
            Err(LlmError::NotYetImplemented)
        }
    }

    #[async_trait]
    impl Summarizer for CodexStub {
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
    async fn all_stub_backends_report_not_yet_implemented() {
        for backend in [
            Backend::Anthropic,
            Backend::ClaudeCodeCli,
            Backend::CodexCli,
        ] {
            let s = build_summarizer(backend);
            let path = PathBuf::from("/tmp/x.jsonl");
            let result = s
                .summarize(SummarizerInput {
                    transcript: &path,
                    meeting_type: MeetingType::Client,
                    existing_action_items: None,
                    existing_attendees: None,
                })
                .await;
            assert!(matches!(result, Err(LlmError::NotYetImplemented)));
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
