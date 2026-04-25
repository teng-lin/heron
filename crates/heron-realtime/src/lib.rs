//! `heron-realtime` — bidirectional realtime LLM session.
//!
//! Layer 4 of the four-layer v2 architecture per
//! [`docs/api-design-spec.md`](../../../docs/api-design-spec.md) §1.
//! Audio in / audio out / tool calls, low latency.
//!
//! Implementations wrap a realtime backend: `OpenAiRealtime` (the
//! cleanest reference vocabulary; this trait mirrors it where possible),
//! `GeminiLive`, `LiveKitAgent`, `Pipecat`. Choice deferred per spec
//! §13 "Next steps."
//!
//! Audio I/O does not flow through this crate's surface — it flows
//! through `heron-bridge` channels owned by the orchestrator. This
//! trait owns the *session* (load persona, tool schema, lifecycle)
//! and the *event stream* (responses, transcripts, tool calls).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

pub use heron_types::prefixed_id::IdParseError;

heron_types::prefixed_id! {
    /// Stripe-style prefixed UUID for one realtime session. Wire
    /// form `session_<uuid>`. Distinct from `heron_types::SessionId`
    /// (which is the v1 recording-session alias) — this is the v2
    /// realtime-LLM-session identity.
    pub SessionId, "session"
}

heron_types::prefixed_id! {
    /// Stripe-style prefixed UUID for one in-flight model response.
    /// Wire form `resp_<uuid>`. Tied to a `SessionId` for the
    /// duration of `ResponseCreated → ResponseDone`.
    pub ResponseId, "resp"
}

/// Session-init configuration. Spec §6 + §8: persona system prompt and
/// pre-meeting context are baked in at init; mid-session changes flow
/// as turn events, not config updates.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Pre-rendered system prompt (persona + context). Spec §8
    /// Invariant 10: ≤16K tokens — caller summarizes if larger.
    pub system_prompt: String,
    /// JSON-schema list of tools the LLM may call mid-session.
    pub tools: Vec<ToolSpec>,
    /// Server-side VAD config. Mirrors OpenAI Realtime `turn_detection`.
    pub turn_detection: TurnDetection,
    /// Voice the model speaks with. Backend-specific opaque ID.
    pub voice: String,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema (Draft 2020-12) for arguments.
    pub parameters_schema: serde_json::Value,
}

/// Mirrors OpenAI Realtime `session.update.turn_detection`. Spec §9:
/// `interrupt_response: true` is the server-side barge-in primitive
/// the speech contract relies on when the backend supports it.
#[derive(Debug, Clone, Copy)]
pub struct TurnDetection {
    pub vad_threshold: f32,
    pub prefix_padding_ms: u32,
    pub silence_duration_ms: u32,
    pub interrupt_response: bool,
    pub auto_create_response: bool,
}

#[derive(Debug, Error)]
pub enum RealtimeError {
    #[error("not yet implemented")]
    NotYetImplemented,
    #[error("session config rejected by backend: {0}")]
    BadConfig(String),
    /// Spec §8 Invariant 10. Caller didn't summarize before init.
    #[error("system prompt exceeds 16K token cap")]
    PromptTooLarge,
    #[error("network: {0}")]
    Network(String),
    #[error("backend error: {0}")]
    Backend(String),
    /// Tool schema invalid; check `parameters_schema`.
    #[error("invalid tool spec: {0}")]
    InvalidToolSpec(String),
}

/// The realtime backend trait. Each session is one bot's lifetime;
/// teardown happens automatically when `bot.completed` fires per spec
/// Invariant 9.
#[async_trait]
pub trait RealtimeBackend: Send + Sync {
    /// Open a session. Returns immediately; observe lifecycle via
    /// [`Self::subscribe_events`]. Audio I/O is bound externally via
    /// `heron-bridge` channels passed at construction.
    async fn session_open(&self, config: SessionConfig) -> Result<SessionId, RealtimeError>;

    /// Close gracefully. Backend flushes any in-flight response.
    async fn session_close(&self, id: SessionId) -> Result<(), RealtimeError>;

    /// Cancel a specific in-flight response (utterance the LLM is
    /// currently producing). Mirrors OpenAI Realtime `response.cancel`.
    /// Idempotent: `Ok(())` if already done.
    async fn response_cancel(
        &self,
        session: SessionId,
        response: ResponseId,
    ) -> Result<(), RealtimeError>;

    /// Truncate the model's current item mid-speech. Mirrors
    /// `conversation.item.truncate`. Used by `heron-policy` when the
    /// human starts speaking partway through the agent's response.
    async fn truncate_current(
        &self,
        session: SessionId,
        audio_end_ms: u32,
    ) -> Result<(), RealtimeError>;

    /// Inject a tool-call result back into the conversation.
    async fn tool_result(
        &self,
        session: SessionId,
        tool_call_id: String,
        result: serde_json::Value,
    ) -> Result<(), RealtimeError>;

    fn subscribe_events(&self, id: SessionId) -> broadcast::Receiver<RealtimeEvent>;

    fn capabilities(&self) -> RealtimeCapabilities;
}

/// Mirrors OpenAI Realtime's server-event vocabulary where possible.
/// `heron-policy` consumes these to drive turn-taking decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RealtimeEvent {
    /// Server VAD: human just started speaking. `heron-policy` should
    /// barge-in if the agent is mid-utterance.
    InputSpeechStarted {
        session: SessionId,
        at: DateTime<Utc>,
    },

    InputSpeechStopped {
        session: SessionId,
        at: DateTime<Utc>,
    },

    /// Partial transcript of the human's speech.
    InputTranscriptDelta {
        session: SessionId,
        text: String,
        is_final: bool,
    },

    /// Model started a response. `response_id` correlates with later
    /// audio output and tool calls.
    ResponseCreated {
        session: SessionId,
        response: ResponseId,
        at: DateTime<Utc>,
    },

    ResponseTextDelta {
        session: SessionId,
        response: ResponseId,
        text: String,
    },

    /// Audio chunk available; in practice routed through `heron-bridge`,
    /// but the *event* fires here so policy knows the response is
    /// actually speaking (not just queued).
    ResponseAudioStarted {
        session: SessionId,
        response: ResponseId,
        at: DateTime<Utc>,
    },

    ResponseDone {
        session: SessionId,
        response: ResponseId,
        at: DateTime<Utc>,
    },

    /// Model wants to call a tool. Caller fulfills with `tool_result`.
    ToolCall {
        session: SessionId,
        response: ResponseId,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    Error {
        session: SessionId,
        error: String,
    },
}

/// Spec §9. The capability matrix `heron-policy` consults.
/// See [`docs/api-design-research.md`](../../../docs/api-design-research.md)
/// "Layer 3" matrix for vendor-by-vendor truth values.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealtimeCapabilities {
    pub bidirectional_audio: bool,
    pub server_vad: bool,
    pub atomic_response_cancel: bool,
    pub tool_calling: bool,
    /// Some backends (OpenAI Realtime, Gemini Live) emit text deltas
    /// alongside audio; others (raw TTS pipelines) don't.
    pub text_deltas: bool,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod prefix_tests {
    //! Per-consumer wire-shape regression guards for phase-48 IDs.
    //! The macro's own tests in `heron-types` cover codegen; these
    //! pin that the realtime crate still gets the prefixes
    //! documented in `docs/api-design-spec.md`.

    use super::*;

    #[test]
    fn session_id_uses_session_prefix_on_the_wire() {
        let id = SessionId::now_v7();
        let json = serde_json::to_string(&id).expect("serialize");
        assert!(json.starts_with(r#""session_"#), "got: {json}");
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn response_id_uses_resp_prefix_on_the_wire() {
        let id = ResponseId::now_v7();
        let json = serde_json::to_string(&id).expect("serialize");
        assert!(json.starts_with(r#""resp_"#), "got: {json}");
        let back: ResponseId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn cross_type_misroute_session_into_response_fails_at_deserialize() {
        let session = SessionId::now_v7();
        let json = serde_json::to_string(&session).expect("serialize");
        let err = serde_json::from_str::<ResponseId>(&json).expect_err("misroute");
        let msg = err.to_string();
        assert!(msg.contains("resp"), "missing expected prefix: {msg}");
        assert!(msg.contains("session"), "missing actual prefix: {msg}");
    }

    #[test]
    fn realtime_event_round_trips_with_prefixed_ids() {
        // A `RealtimeEvent::ResponseCreated` carries both ids; pin
        // the wire shape so a future event-payload rename surfaces
        // here rather than at a vendor edge.
        let event = RealtimeEvent::ResponseCreated {
            session: SessionId::now_v7(),
            response: ResponseId::now_v7(),
            at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(
            json.contains(r#""session":"session_"#),
            "missing session prefix: {json}"
        );
        assert!(
            json.contains(r#""response":"resp_"#),
            "missing response prefix: {json}"
        );
        let _back: RealtimeEvent = serde_json::from_str(&json).expect("deserialize");
    }
}
