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
use heron_types::{ActionItem, Attendee, Cost, MeetingType, Persona};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod anthropic;
pub mod claude_code;
pub mod codex;
mod content;
pub mod cost;
pub mod key_resolver;
pub mod openai;
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
pub use openai::{OpenAIClient, OpenAIClientConfig};
pub use select::{
    Availability, Preference, SelectError, SelectionReason, parse_settings_backend, select_backend,
    select_summarizer, select_summarizer_with_resolver, select_summarizer_with_user_choice,
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
///
/// `pre_meeting_briefing` carries pre-rendered pre-meeting context
/// (agenda / attendees / prior decisions / user briefing) staged via
/// the daemon's `attach_context` route. When `Some`, the prompt
/// template emits a "Pre-meeting context" preamble so the LLM can
/// reference what the user knew going into the call. Stays `None` for
/// CLI / ad-hoc captures with no calendar correlation.
///
/// `persona` carries the user's self-context (name / role /
/// working-on) the summarizer can splice into the system / preamble
/// prompt (Tier 4 #18). When `None` OR every persona field is the
/// empty string, the rendered prompt is byte-identical to the
/// pre-Tier-4 template — pinned by
/// `template_with_empty_persona_matches_no_persona_baseline`.
///
/// `strip_names` (Tier 4 #21) replaces each unique
/// `participant.display_name` (the JSONL `speaker` field) with
/// `Speaker A`, `Speaker B`, … in the transcript text fed to the LLM.
/// Letters are assigned in first-appearance order so the same speaker
/// gets the same letter on every call. The strip applies *only* to
/// the LLM input — the orchestrator's `attendees` round-trip still
/// uses the real names.
#[derive(Debug)]
pub struct SummarizerInput<'a> {
    pub transcript: &'a Path,
    pub meeting_type: MeetingType,
    pub existing_action_items: Option<&'a [ActionItem]>,
    pub existing_attendees: Option<&'a [Attendee]>,
    pub pre_meeting_briefing: Option<&'a str>,
    pub persona: Option<&'a Persona>,
    pub strip_names: bool,
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
    #[error(
        "API key is unset or empty; export ANTHROPIC_API_KEY or OPENAI_API_KEY \
         before running summarize"
    )]
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
    /// Bare-`reqwest` OpenAI Chat Completions client (hosted API fallback).
    OpenAI,
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
        Backend::OpenAI => match OpenAIClientConfig::from_resolver(resolver) {
            Ok(cfg) => match OpenAIClient::new(cfg) {
                Ok(c) => Box::new(c),
                Err(e) => {
                    tracing::warn!("OpenAIClient construction failed; falling back to stub: {e}");
                    Box::new(stub::OpenAIStub)
                }
            },
            Err(e) => {
                tracing::warn!(
                    "OpenAI backend selected but {e}; \
                     summarize calls will return NotYetImplemented"
                );
                Box::new(stub::OpenAIStub)
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

    // Persona is suppressed when `None` OR every field is empty so
    // the rendered prompt stays byte-identical to the pre-Tier-4
    // baseline on the no-config path. Pinned by
    // `template_with_empty_persona_matches_no_persona_baseline`.
    let persona = input.persona.filter(|p| !p.is_empty()).map(|p| {
        serde_json::json!({
            "name": p.name,
            "role": p.role,
            "working_on": p.working_on,
            "has_name": !p.name.is_empty(),
            "has_role": !p.role.is_empty(),
            "has_working_on": !p.working_on.is_empty(),
        })
    });

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
        // `trim_end` so a caller passing context that already ends in
        // "\n" (the renderer's standard tail) doesn't produce a blank
        // line inside the rendered prompt block.
        "pre_meeting_briefing": input.pre_meeting_briefing
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        "persona": persona,
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

    /// Returned by `build_summarizer(OpenAI)` when `OPENAI_API_KEY`
    /// is missing — the orchestrator can still build, the failure
    /// surfaces at first summarize call.
    pub struct OpenAIStub;

    #[async_trait]
    impl Summarizer for OpenAIStub {
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
    use heron_types::{ActionItem, Attendee, ItemId, MeetingType, Persona};

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
            done: false,
        }];
        let path = PathBuf::from("/tmp/x.jsonl");
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: Some(&items),
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
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
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
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
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
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
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
        })
        .expect("client");
        assert!(client.contains("EXTERNAL client meeting"));

        let internal = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Internal,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
        })
        .expect("internal");
        assert!(internal.contains("INTERNAL team meeting"));

        let one_on_one = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::OneOnOne,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
        })
        .expect("1:1");
        assert!(one_on_one.contains("This is a 1:1"));
    }

    #[test]
    fn template_includes_pre_meeting_briefing_when_present() {
        let path = PathBuf::from("/tmp/x.jsonl");
        let briefing = "## Agenda\nQ3 launch review\n\n## Attendees\n- Alice (CFO)\n";
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: Some(briefing),
            persona: None,
            strip_names: false,
        })
        .expect("render with briefing");
        assert!(prompt.contains("## Pre-meeting context"));
        assert!(prompt.contains("Q3 launch review"));
        assert!(prompt.contains("Alice (CFO)"));
        // The transcript section is still authoritative — the prompt
        // says so explicitly.
        assert!(prompt.contains("transcript is still authoritative"));
    }

    #[test]
    fn template_omits_pre_meeting_briefing_when_absent() {
        let path = PathBuf::from("/tmp/x.jsonl");
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Other,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
        })
        .expect("render without briefing");
        assert!(!prompt.contains("Pre-meeting context"));
    }

    #[test]
    fn template_omits_pre_meeting_briefing_when_whitespace_only() {
        // A caller passing an all-whitespace briefing (every renderer
        // field empty) should not produce a stranded
        // "## Pre-meeting context" header.
        let path = PathBuf::from("/tmp/x.jsonl");
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Other,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: Some("   \n\n   "),
            persona: None,
            strip_names: false,
        })
        .expect("render with empty briefing");
        assert!(!prompt.contains("Pre-meeting context"));
    }

    // ── Tier 4 #18: persona injection ──────────────────────────────────

    /// **Regression contract.** The snapshot for the no-persona path
    /// must be byte-identical to the no-persona / no-briefing prompt
    /// regardless of whether the caller passed `persona: None` or a
    /// `Persona` whose every field is the empty string. This pins the
    /// migration story for users who upgrade past Tier 4 without ever
    /// filling in the Settings → Persona inputs: their summaries must
    /// keep using the same prompt the pre-Tier-4 build emitted.
    #[test]
    fn template_with_empty_persona_matches_no_persona_baseline() {
        let path = PathBuf::from("/tmp/x.jsonl");
        let baseline = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: None,
            strip_names: false,
        })
        .expect("render baseline");
        let with_empty_persona = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: Some(&Persona::default()),
            strip_names: false,
        })
        .expect("render with empty persona");
        assert_eq!(
            baseline, with_empty_persona,
            "an all-empty Persona must produce the same prompt as None — \
             otherwise users who upgraded without filling in Settings → \
             Persona will silently see prompt drift on their summaries"
        );
        // And the baseline itself must NOT contain the persona block
        // header. Anchor on a unique substring from the rendered block
        // so a future template tweak (e.g. heading rename) doesn't
        // silently weaken the contract.
        assert!(
            !baseline.contains("## About the user"),
            "baseline must not carry the persona block: {baseline}"
        );
    }

    #[test]
    fn template_renders_persona_block_when_all_fields_present() {
        let path = PathBuf::from("/tmp/x.jsonl");
        let persona = Persona {
            name: "Alice".into(),
            role: "Head of Sales".into(),
            working_on: "Q3 pipeline".into(),
        };
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: Some(&persona),
            strip_names: false,
        })
        .expect("render with persona");
        assert!(prompt.contains("## About the user"));
        // Each field must reach the rendered prompt — substring asserts
        // rather than exact-string so the surrounding sentence can be
        // tweaked without breaking the test.
        assert!(prompt.contains("Alice"), "name not in prompt: {prompt}");
        assert!(
            prompt.contains("Head of Sales"),
            "role not in prompt: {prompt}"
        );
        assert!(
            prompt.contains("Q3 pipeline"),
            "working_on not in prompt: {prompt}"
        );
    }

    #[test]
    fn template_renders_persona_block_with_partial_fields() {
        // Only `name` set; `role` and `working_on` empty. The block
        // must still render (since the user isn't `is_empty()`) but
        // shouldn't paste in stranded "They work as ." sentences.
        let path = PathBuf::from("/tmp/x.jsonl");
        let persona = Persona {
            name: "Alice".into(),
            ..Persona::default()
        };
        let prompt = render_meeting_prompt(&SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: Some(&persona),
            strip_names: false,
        })
        .expect("render with partial persona");
        assert!(prompt.contains("## About the user"));
        assert!(prompt.contains("Alice"));
        // Belt-and-suspenders: the empty-role / empty-working_on
        // sentences must not appear with a stranded period.
        assert!(
            !prompt.contains("They work as ."),
            "empty role must not produce a stranded sentence: {prompt}"
        );
        assert!(
            !prompt.contains("focused on: ."),
            "empty working_on must not produce a stranded sentence: {prompt}"
        );
    }

    // ── Tier 4 #21: strip_names flag plumbed through SummarizerInput ────

    #[test]
    fn summarizer_input_carries_strip_names_field() {
        // Smoke: the field exists, defaults to false, and a `true`
        // value round-trips through Debug. The actual transform is
        // exercised in `transcript::tests::strip_speaker_names_*`;
        // this test only pins that the struct field is wired.
        let path = PathBuf::from("/tmp/x.jsonl");
        let input = SummarizerInput {
            transcript: &path,
            meeting_type: MeetingType::Client,
            existing_action_items: None,
            existing_attendees: None,
            pre_meeting_briefing: None,
            persona: None,
            strip_names: true,
        };
        assert!(input.strip_names);
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
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
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
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
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
