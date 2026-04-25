//! `heron-policy` — speech-control contract + agent policy.
//!
//! Layer 3 of the four-layer v2 architecture per
//! [`docs/api-design-spec.md`](../../../docs/api-design-spec.md) §1.
//! The load-bearing middle layer that protects product behavior from
//! vendor quirks. **No vendor ships this layer**; it is heron's
//! contribution per [`docs/api-design-research.md`](../../../docs/api-design-research.md)
//! "Layer 2: Policy / Turn vendors."
//!
//! Owns three concerns the realtime backend can't make for itself:
//! 1. *When* should the agent speak (turn-taking, addressed-by-name)
//! 2. *What* is the agent allowed to say (allow/deny, escalation)
//! 3. *How* is queueing & cancellation modeled (the speech contract)

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use uuid::Uuid;

// ── identity ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UtteranceId(pub Uuid);

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VoiceId(pub Uuid);

// ── speech contract ───────────────────────────────────────────────────

/// Spec §9. Three priorities; `Replace` is the load-bearing one — the
/// single primitive that avoids the cancel-then-speak race
/// (Invariant 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Append to the end of the queue.
    Append,
    /// Atomically: cancel current + clear queue + speak. Single op,
    /// no audible gap. The fix for the cancel-then-speak race.
    Replace,
    /// Cancel current only + speak. Queue stays. For corrections.
    Interrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpeechEvent {
    Started {
        id: UtteranceId,
        started_at: DateTime<Utc>,
    },
    Progress {
        id: UtteranceId,
        words_spoken: u32,
    },
    Completed {
        id: UtteranceId,
        duration_ms: u64,
    },
    Cancelled {
        id: UtteranceId,
        reason: CancelReason,
    },
    Failed {
        id: UtteranceId,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CancelReason {
    UserRequested,
    /// Spec §9. The `Priority::Replace` operation that displaced this.
    Replaced {
        by: UtteranceId,
    },
    /// Human spoke; barge-in fired. Speaker is the one who interrupted.
    BargedIn {
        by_speaker: SpeakerId,
    },
    PolicyDenied {
        rule: String,
    },
    Failed,
}

#[derive(Debug, Error)]
pub enum SpeechError {
    #[error("not yet implemented")]
    NotYetImplemented,

    /// The chosen realtime backend can't honor this primitive. Spec §9
    /// "Capability degradation." Caller (`heron-policy`) decides
    /// whether to fall back or fail.
    #[error("capability not supported by backend: {0}")]
    CapabilityNotSupported(&'static str),

    /// Spec §4. Disclosure not yet acknowledged; speaking forbidden.
    #[error("disclosure not yet complete")]
    DisclosurePending,

    /// Policy rule rejected the utterance pre-emission.
    #[error("policy denied: {rule}")]
    PolicyDenied { rule: String },

    #[error("realtime backend error: {0}")]
    Backend(String),
}

/// The vendor-neutral speech-control trait. Spec §9.
///
/// Implementations wrap a realtime backend (`OpenAiRealtime`,
/// `LiveKitAgent`, `Pipecat`) and a policy filter. Backends that
/// can't honor a primitive return [`SpeechError::CapabilityNotSupported`].
#[async_trait]
pub trait SpeechController: Send + Sync {
    async fn speak(
        &self,
        text: &str,
        priority: Priority,
        voice_override: Option<VoiceId>,
    ) -> Result<UtteranceId, SpeechError>;

    /// Idempotent: `Ok(())` if utterance already done / cancelled / unknown.
    async fn cancel(&self, id: UtteranceId) -> Result<(), SpeechError>;

    /// Clear queue but let current finish.
    async fn cancel_all_queued(&self) -> Result<(), SpeechError>;

    /// Panic-stop: cancel current + clear queue. The "barge-in by user"
    /// reflex.
    async fn cancel_current_and_clear(&self) -> Result<(), SpeechError>;

    fn subscribe_events(&self) -> broadcast::Receiver<SpeechEvent>;

    fn capabilities(&self) -> SpeechCapabilities;
}

/// Spec §9. What the underlying realtime backend can promise.
/// `heron-policy` picks degradation strategies per `false` field.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpeechCapabilities {
    /// False = all `speak()` calls return `Uuid::nil()`-shaped IDs.
    pub utterance_ids: bool,
    pub per_utterance_cancel: bool,
    /// False = `Append` behaves like `Replace`.
    pub queue: bool,
    /// False = `Replace` is emulated as cancel+speak (race exists).
    pub atomic_replace: bool,
    /// False = client must run VAD / barge-in detection.
    pub barge_in_detect: bool,
}

// ── policy rules (orthogonal to the speech contract) ──────────────────

/// What the agent may say, when, on whose behalf. Authored once per
/// session; consulted before every `speak()` emission.
#[derive(Debug, Clone)]
pub struct PolicyProfile {
    /// Free-form topics the agent may discuss.
    pub allow_topics: Vec<String>,
    /// Topics that escalate to the human. E.g. ["compensation",
    /// "termination", "legal"].
    pub deny_topics: Vec<String>,
    /// Hard mute. If `true`, every `speak()` returns `PolicyDenied`.
    pub mute: bool,
    /// How the agent escalates. Webhook? Push notification? Vault note?
    pub escalation: EscalationMode,
}

#[derive(Debug, Clone)]
pub enum EscalationMode {
    None,
    Notify { destination: String },
    LeaveMeeting,
}
