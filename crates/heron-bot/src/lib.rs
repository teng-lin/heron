//! `heron-bot` — meeting-bot driver trait.
//!
//! Layer 1 of the four-layer v2 architecture per
//! [`docs/api-design-spec.md`](../../../docs/api-design-spec.md) §1.
//! The boundary between heron-the-app and the meeting platform.
//!
//! Real implementations wrap a vendor: Recall.ai, Attendee, MeetingBaaS,
//! or a native Zoom SDK. Choice deferred until the spike (spec §13
//! "Next steps").
//!
//! Invariants this trait must uphold:
//! - Vendor quirks live ONLY in the impl (spec Invariant 1).
//! - `bot_create()` rejects without a `DisclosureProfile` (Invariant 6).
//! - `bot_create()` rejects without a `PersonaId` (Invariant 8).
//! - Singleton in v2.0; second create returns `BotAlreadyActive`
//!   (Invariant 7).
//! - Vendor-API discipline (idempotency, HMAC verify, retry) lives
//!   inside the impl, not the caller (Invariant 14).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub mod context;
pub mod disclosure;
pub mod fsm;
pub mod ids;
pub use context::{ContextError, MAX_CONTEXT_BYTES, render as render_context};
pub use disclosure::{is_objection, match_objection};
pub use fsm::{BotEvent, BotFsm, TransitionError};
pub use ids::IdParseError;

// ── identity ──────────────────────────────────────────────────────────

prefixed_id! {
    /// Stripe-style prefixed UUID for a bot. Wire form `bot_<uuid>`.
    /// Spec §2 Invariant 4: internal canonical identity. Composite
    /// keys / URLs are resolver inputs, never primary identity.
    pub BotId, "bot"
}

prefixed_id! {
    /// Stripe-style prefixed UUID for a persona. Wire form
    /// `persona_<uuid>`. A misrouted JSON parse (persona ID into a
    /// BotId field) fails at deserialize time rather than running
    /// through the system as a wrong-typed UUID.
    pub PersonaId, "persona"
}

prefixed_id! {
    /// Stripe-style prefixed UUID for a meeting. Wire form
    /// `meeting_<uuid>`.
    pub MeetingId, "meeting"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Zoom,
    GoogleMeet,
    MicrosoftTeams,
    Webex,
}

// ── lifecycle FSM ─────────────────────────────────────────────────────

/// Spec §3. The valid states of a bot. `in_meeting` is the only state
/// in which speech-control calls are accepted (Invariant 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BotState {
    Init,
    LoadingPersona,
    TtsWarming,
    Joining,
    Disclosing,
    InMeeting,
    Reconnecting,
    Leaving,
    Completed,
    Failed { error: String },
    Ejected { reason: EjectReason },
    HostEnded,
}

/// Spec §7. Why the platform ejected the bot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EjectReason {
    HostRemoved,
    RecordingPermissionDenied,
    AdmissionRefused,
    PolicyViolation,
    Unknown,
}

// ── create-time arguments ─────────────────────────────────────────────

/// Spec §4 + §6 + §8. The full set of create-time configuration. A bot
/// without disclosure / persona / context is a bug; this struct makes
/// that compile-time-enforced.
#[derive(Debug, Clone)]
pub struct BotCreateArgs {
    /// Meeting URL the bot should join. Parsed/validated by the impl.
    pub meeting_url: String,
    pub persona_id: PersonaId,
    pub disclosure: DisclosureProfile,
    pub context: PreMeetingContext,
    /// Echoed back on every event published about this bot. Spec §10.
    pub metadata: serde_json::Value,
    /// Caller-supplied for outbound retry safety. The impl forwards
    /// this verbatim to the vendor (Recall / Attendee / etc.) as
    /// `Idempotency-Key`. Spec §11 Invariant 14.
    pub idempotency_key: Uuid,
}

#[derive(Debug, Clone)]
pub struct DisclosureProfile {
    /// Handlebars-templated; rendered with `{user_name}`,
    /// `{meeting_title}` etc. Empty template rejected by `bot_create`.
    pub text_template: String,
    pub objection_patterns: Vec<String>,
    pub objection_timeout_secs: u64,
    pub re_announce_on_join: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PreMeetingContext {
    pub agenda: Option<String>,
    pub attendees_known: Vec<AttendeeContext>,
    /// Vault paths only; content loaded lazily by `heron-realtime`.
    pub related_notes: Vec<String>,
    pub user_briefing: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttendeeContext {
    pub name: String,
    pub email: Option<String>,
    pub last_seen_in: Option<MeetingId>,
    pub relationship: Option<String>,
    pub notes: Option<String>,
}

// ── errors ────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum BotError {
    #[error("not yet implemented (driver wired during spike — see spec §13)")]
    NotYetImplemented,

    /// Spec Invariant 7. Singleton enforcement.
    #[error("bot already active: {existing:?}")]
    BotAlreadyActive { existing: BotId },

    /// Spec Invariant 6. No silent bot.
    #[error("disclosure profile missing or empty")]
    NoDisclosureProfile,

    /// Spec Invariant 5. Wrong state for the requested operation.
    #[error("not in meeting (current state: {current_state:?})")]
    NotInMeeting { current_state: BotState },

    #[error("vendor API error: {0}")]
    Vendor(String),

    #[error("network: {0}")]
    Network(String),

    /// Vendor returned a structured capacity / quota error (Recall 507,
    /// MeetingBaaS plan-limit). Distinct retry strategy from rate-limit.
    #[error("capacity exhausted; retry after {retry_after_secs}s")]
    CapacityExhausted { retry_after_secs: u64 },

    /// Vendor rate-limit (429). Distinct from capacity.
    #[error("rate limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
}

// ── trait surface ─────────────────────────────────────────────────────

/// Vendor-neutral driver trait. Implementations: `RecallDriver`,
/// `AttendeeDriver`, `NativeZoomDriver` (post-spike).
///
/// Audio I/O is exposed via `heron-bridge` channels owned by the impl;
/// this trait does not surface PCM frames directly because vendors
/// expose them differently (Recall: WebSocket bytes; Native: callback).
#[async_trait]
pub trait MeetingBotDriver: Send + Sync {
    /// Spec §3, §4, §6, §8. Creates a bot and begins the FSM transition
    /// `init → loading_persona → tts_warming → joining → disclosing →
    /// in_meeting`. Returns immediately; observe lifecycle via
    /// [`Self::subscribe_state`].
    ///
    /// Errors:
    /// - [`BotError::BotAlreadyActive`] if singleton (Invariant 7) violated
    /// - [`BotError::NoDisclosureProfile`] if disclosure empty (Invariant 6)
    async fn bot_create(&self, args: BotCreateArgs) -> Result<BotId, BotError>;

    /// Spec §3. Graceful leave. Bot speaks goodbye, finalizes vault,
    /// transitions `in_meeting → leaving → completed`. Idempotent.
    async fn bot_leave(&self, id: BotId) -> Result<(), BotError>;

    /// Spec §3. Hard kill. Only legal in `init | loading_persona |
    /// tts_warming | joining` — refuses to terminate a bot that is
    /// `in_meeting` (use `bot_leave` instead). Following Recall's
    /// "DELETE-only-on-pre-join" semantics.
    async fn bot_terminate(&self, id: BotId) -> Result<(), BotError>;

    fn current_state(&self, id: BotId) -> Option<BotState>;

    /// Subscribe to state transitions for a single bot. Stream ends
    /// when the bot reaches a terminal state.
    fn subscribe_state(&self, id: BotId) -> tokio::sync::broadcast::Receiver<BotStateEvent>;

    fn capabilities(&self) -> DriverCapabilities;
}

/// Spec §3, §7. State-change event. Always carries the bot ID and a
/// timestamp; transitions to terminal states carry the outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotStateEvent {
    pub bot_id: BotId,
    pub at: DateTime<Utc>,
    pub state: BotState,
    /// Echoed `BotCreateArgs::metadata`. Spec §10.
    pub metadata: serde_json::Value,
}

/// What the driver can promise. Caller (`heron-policy`) inspects
/// before deciding which features to expose to the LLM.
#[derive(Debug, Clone, Copy)]
pub struct DriverCapabilities {
    pub platforms: &'static [Platform],
    /// True for Recall, MeetingBaaS, Vexa; false for Attendee voice-agent.
    pub live_partial_transcripts: bool,
    /// True if the vendor distinguishes EjectReason granularly. Many
    /// collapse everything to "ejected, reason unknown."
    pub granular_eject_reasons: bool,
    /// Some drivers expose audio frames to higher layers; others don't
    /// (Recall hides them behind WebSocket transcripts).
    pub raw_pcm_access: bool,
}
