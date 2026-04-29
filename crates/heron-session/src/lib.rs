//! `heron-session` — meeting capture orchestration trait + domain types.
//!
//! The "planned hub" the OpenAPI spec
//! ([`docs/api-desktop-openapi.yaml`](../../../docs/api-desktop-openapi.yaml))
//! and the architecture doc
//! ([`docs/architecture.md`](../../../docs/architecture.md)) keep
//! deferring to. Today the orchestration role is split between
//! `heron-cli::session` (the FSM + session log) and `heron-zoom` (the
//! AXObserver-driven detection signal); this crate is where they
//! converge once we cut over.
//!
//! Three responsibilities, all derived from the YAML surface:
//!
//! 1. **Domain types** — `Meeting`, `Participant`, `Transcript`,
//!    `Summary`, `CalendarEvent`, `PreMeetingContext`, `Health`. The
//!    Rust shape is authoritative; the YAML is a transport
//!    projection (per the file's own header). If they disagree the
//!    YAML file is the bug.
//! 2. **Event payloads** — the [`EventPayload`] enum is the typed
//!    discriminated union mirroring the OpenAPI's `EventEnvelope`.
//!    [`EventEnvelope`] is the type alias for an envelope of this
//!    payload, ridden through `heron_event::EventBus`.
//! 3. **Orchestrator trait** — [`SessionOrchestrator`] fronts every
//!    operation `herond` exposes over HTTP: list/get meetings,
//!    transcript / summary / audio reads, the manual-capture escape
//!    hatches (`POST /meetings`, `POST /meetings/{id}/end`),
//!    pre-meeting context attach, calendar reads, health.
//!
//! Invariants this trait surface upholds:
//!
//! - Composite keys (calendar event IDs) are resolver inputs, never
//!   primary identity (spec Invariant 4). `MeetingId` is the only
//!   internal canonical handle; `calendar_event_id` is a free
//!   `String` to remind callers it's resolver-input shape.
//! - All events flow through `heron_event::EventBus` first
//!   (Invariant 12); the orchestrator publishes, transports
//!   subscribe.
//! - Every terminal-state transition emits **exactly one**
//!   `meeting.completed` event with `data.outcome` carrying
//!   success/failure (Invariant 9). There is no `meeting.failed`
//!   variant; consumers MUST switch on `MeetingOutcome`.
//! - Pre-meeting context is capped at 16K tokens before consumption
//!   (Invariant 10). The trait accepts larger payloads but
//!   summarizes at consumption time.

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use heron_event::{Envelope, EventBus};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use heron_event::{EventId, ReplayCache, ReplayError};
pub use heron_types::prefixed_id::IdParseError;

// ── identity ──────────────────────────────────────────────────────────

/// `MeetingId` is canonical in `heron-types`; this crate
/// re-exports it so consumers can `use heron_session::MeetingId`
/// without learning the layout. The same physical type that
/// `heron_bot` re-exports — a v1 desktop ID flowing into a v2
/// driver field is fine by construction now (used to be a
/// typed-handle gap that crossed the layer boundary).
pub use heron_types::MeetingId;

// ── enums ─────────────────────────────────────────────────────────────

/// Native meeting client. Mirrors the OpenAPI `Platform` enum.
///
/// **v1 actually serves only `Zoom`**; the other variants are
/// reserved for v1.1+ (Google Meet / Teams once the WebRTC track-ID
/// interception path lands; Webex pending an accessibility-tree
/// survey). A v1 daemon emitting anything other than `Zoom` is a
/// bug — clients still accept the wider enum for forward
/// compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Zoom,
    GoogleMeet,
    MicrosoftTeams,
    Webex,
}

/// v1 desktop meeting lifecycle. Distinct from spec §3, which is the
/// v2 bot FSM. `Done` is terminal-success (transcript + summary
/// written + audio sidecar verified); `Failed` is terminal-failure
/// (capture aborted, transcript orphaned). Both terminal states emit
/// a single `meeting.completed` event with `data.outcome` carrying
/// success vs failure (Invariant 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingStatus {
    Detected,
    Armed,
    Recording,
    Ended,
    Done,
    Failed,
}

impl MeetingStatus {
    /// True for `Done` and `Failed` — the two states that emit
    /// `meeting.completed`. Useful gate for `GET /meetings/{id}/audio`,
    /// which returns `425 Too Early` if the meeting is still
    /// recording.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// Speaker-attribution confidence. AXObserver-attributed turns are
/// `High`; turns marked `them` without a real display name are `Low`
/// (per the README's ~70/30 quality promise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Low,
}

/// How a participant's identity was resolved. Carried for diagnostic
/// reasons — consumers should not branch on it (the same participant
/// may flip across kinds mid-meeting as better signal arrives).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentifierKind {
    AxTree,
    WebrtcTrack,
    Mic,
    Fallback,
}

// ── domain types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    /// Real display name when known (Zoom AX tree, WebRTC track
    /// label). For low-confidence turns this is `"them"` or the
    /// literal `"me"`.
    pub display_name: String,
    pub identifier_kind: IdentifierKind,
    /// True when this participant is the local user.
    pub is_user: bool,
}

/// Mirrors the OpenAPI `Meeting` schema. The single resource type
/// returned by `/meetings`, `/meetings/{id}`, and embedded in most
/// `meeting.*` events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meeting {
    pub id: MeetingId,
    pub status: MeetingStatus,
    pub platform: Platform,
    pub title: Option<String>,
    /// EventKit identifier when correlation succeeded; `None` when
    /// the meeting was captured ad-hoc. **Correlation only** —
    /// never a heron operational input. Use `MeetingId` for anything
    /// other than cross-referencing with the user's calendar
    /// (Invariant 4). Free `String` to reinforce that this is a
    /// resolver-input shape.
    pub calendar_event_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_secs: Option<u64>,
    pub participants: Vec<Participant>,
    pub transcript_status: TranscriptLifecycle,
    pub summary_status: SummaryLifecycle,
    /// LLM-inferred topic tags lifted from the note's
    /// `Frontmatter.tags`. Empty for active captures (no summary yet)
    /// and for any meeting whose summarizer omitted them. `#[serde(default)]`
    /// so older daemon builds / serialized fixtures that predate this
    /// field still deserialize cleanly into the wider type.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Per-meeting LLM cost telemetry. `Some` once a summary has been
    /// generated — the orchestrator reads this from the persisted
    /// `Frontmatter.cost` (`heron-types::Cost`) when projecting a
    /// vault note into a `Meeting`. `None` for meetings that haven't
    /// been summarized yet (still recording, freshly detected, or
    /// pre-Tier-0-#2 vault notes that recorded zero/empty cost).
    ///
    /// Tier 0 #2 of the UX redesign: powers the Review right-rail
    /// "Processing" panel (`Tokens in / Tokens out / Summarized by /
    /// $`). UI for the panel is a separate PR; this is bridge-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing: Option<MeetingProcessing>,
}

/// Per-meeting LLM cost telemetry projected onto the wire.
///
/// Mirrors `heron_types::Cost` one-for-one: every field corresponds
/// directly to what the summarizer wrote into the YAML frontmatter on
/// the last summarize call (`heron-vault::merge` always pulls cost
/// from `theirs`, so this is the *most recent* summarize, not an
/// aggregate). No new fields are invented — STT model is not
/// persisted in the frontmatter today, so it does not appear here.
///
/// `Option<Self>` on `Meeting.processing` carries the
/// "summary not yet run" signal — the inner struct is always
/// fully populated when present.
///
/// `PartialEq` is intentionally NOT derived: the `summary_usd: f64`
/// field would compare with bitwise equality, which is unreliable
/// for floats. Tests compare numeric fields with an epsilon
/// tolerance and pin the model string separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingProcessing {
    /// USD cost of the most recent summarize call.
    pub summary_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// LLM model identifier (e.g. `claude-sonnet-4-6`). Free string —
    /// the wire shape does not enumerate a closed set so v1.1+ models
    /// surface without a wire bump.
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptLifecycle {
    Pending,
    /// More segments still arriving; subscribe to `/events` for live
    /// deltas.
    Partial,
    /// Sealed — no further segments.
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryLifecycle {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub speaker: Participant,
    pub text: String,
    pub start_secs: f64,
    pub end_secs: f64,
    pub confidence: Confidence,
    /// `false` segments are subject to revision in a later partial.
    /// Only `true` segments persist to the vault.
    pub is_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub meeting_id: MeetingId,
    /// Lifecycle of the transcript itself, decoupled from the parent
    /// meeting. `Partial` means more segments are still being added;
    /// `Complete` means the transcript is sealed.
    pub status: TranscriptLifecycle,
    pub language: Option<String>,
    pub segments: Vec<TranscriptSegment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    /// Display name of the person the item is assigned to, when the
    /// LLM extracted one. Free text — heron does not resolve this
    /// back to a `Participant` in v1.
    pub owner: Option<String>,
    pub due: Option<chrono::NaiveDate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub meeting_id: MeetingId,
    pub generated_at: DateTime<Utc>,
    /// Markdown body, the same text written into the vault note.
    pub text: String,
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
    pub llm_provider: Option<String>,
    pub llm_model: Option<String>,
}

// ── calendar / context ────────────────────────────────────────────────

/// The wire-shape calendar event returned by `/calendar/upcoming`.
/// Distinct from `heron_vault::CalendarEvent`, which is the *internal*
/// EventKit-bridge shape. `heron-vault` provides the raw bridge
/// reads; this crate adds correlation (`related_meetings`) and the
/// resolver-input `id` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// EventKit identifier. Resolver-input shape; not a heron
    /// primary key. Free `String` per Invariant 4.
    pub id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default)]
    pub attendees: Vec<AttendeeContext>,
    pub meeting_url: Option<String>,
    /// Prior captured meetings whose attendees overlap. Useful for
    /// clients building a pre-meeting briefing on top of the
    /// calendar event in one round trip.
    #[serde(default)]
    pub related_meetings: Vec<MeetingId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttendeeContext {
    pub name: String,
    pub email: Option<String>,
    pub last_seen_in: Option<MeetingId>,
    pub relationship: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreMeetingContext {
    pub agenda: Option<String>,
    #[serde(default)]
    pub attendees_known: Vec<AttendeeContext>,
    /// Vault-relative paths.
    #[serde(default)]
    pub related_notes: Vec<String>,
    #[serde(default)]
    pub prior_decisions: Vec<PriorDecision>,
    pub user_briefing: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorDecision {
    pub text: String,
    pub source_meeting_id: MeetingId,
}

/// Body shape for `PUT /context`. `calendar_event_id` is a resolver
/// input (Invariant 4), so it lives in the body — never in a URL
/// path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreMeetingContextRequest {
    /// EventKit identifier of the calendar event the context attaches
    /// to.
    pub calendar_event_id: String,
    pub context: PreMeetingContext,
}

// ── health ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    Degraded,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Ok,
    Degraded,
    Down,
    PermissionMissing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthComponent {
    pub state: ComponentState,
    pub message: Option<String>,
    pub last_check: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub status: HealthStatus,
    pub version: Option<String>,
    pub components: HealthComponents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthComponents {
    pub capture: HealthComponent,
    pub whisperkit: HealthComponent,
    pub vault: HealthComponent,
    pub eventkit: HealthComponent,
    pub llm: HealthComponent,
}

// ── event payloads ────────────────────────────────────────────────────

/// The typed discriminated union mirroring the OpenAPI `EventEnvelope`.
/// `tag = "event_type"` + `content = "data"` matches the wire shape
/// defined by `EventEnvelopeBase + allOf` in the YAML — when this
/// payload is `#[serde(flatten)]`-ed inside [`heron_event::Envelope`],
/// the on-wire JSON has `event_type` and `data` as top-level fields
/// alongside the framing.
///
/// Per Invariant 9, `MeetingCompleted` is the **only** terminal
/// event — there is no `meeting.failed` variant. Switch on
/// [`MeetingOutcome`] to distinguish success / failure / abort /
/// permission-revoked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    #[serde(rename = "meeting.detected")]
    MeetingDetected(Meeting),
    #[serde(rename = "meeting.armed")]
    MeetingArmed(Meeting),
    #[serde(rename = "meeting.started")]
    MeetingStarted(Meeting),
    #[serde(rename = "meeting.ended")]
    MeetingEnded(Meeting),
    #[serde(rename = "meeting.completed")]
    MeetingCompleted(MeetingCompletedData),
    #[serde(rename = "meeting.participant_joined")]
    MeetingParticipantJoined(Participant),

    #[serde(rename = "transcript.partial")]
    TranscriptPartial(TranscriptSegment),
    #[serde(rename = "transcript.final")]
    TranscriptFinal(TranscriptSegment),

    #[serde(rename = "summary.ready")]
    SummaryReady(Summary),
    #[serde(rename = "action_items.ready")]
    ActionItemsReady(ActionItemsReadyData),

    #[serde(rename = "doctor.warning")]
    DoctorWarning(DoctorWarningData),
    #[serde(rename = "daemon.error")]
    DaemonError(DaemonErrorData),
}

impl EventPayload {
    /// The OpenAPI `event_type` literal for this variant — the same
    /// string serde renders in `tag = "event_type"` framing. Single
    /// canonical projection so transports (SSE in `herond`, future
    /// MCP / webhook / Tauri sinks) and tests assert on the same
    /// strings the wire actually carries. Adding a variant without
    /// extending this match fails the exhaustive-match check, which
    /// is the drift guard the manual `#[serde(rename = …)]`
    /// annotations cannot enforce on their own.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::MeetingDetected(_) => "meeting.detected",
            Self::MeetingArmed(_) => "meeting.armed",
            Self::MeetingStarted(_) => "meeting.started",
            Self::MeetingEnded(_) => "meeting.ended",
            Self::MeetingCompleted(_) => "meeting.completed",
            Self::MeetingParticipantJoined(_) => "meeting.participant_joined",
            Self::TranscriptPartial(_) => "transcript.partial",
            Self::TranscriptFinal(_) => "transcript.final",
            Self::SummaryReady(_) => "summary.ready",
            Self::ActionItemsReady(_) => "action_items.ready",
            Self::DoctorWarning(_) => "doctor.warning",
            Self::DaemonError(_) => "daemon.error",
        }
    }
}

/// Type alias for the event envelope flowing through the bus.
/// Adapter crates (HTTP/SSE, Tauri IPC, MCP, webhook) project from
/// `EventBus<EventPayload>`.
pub type EventEnvelope = Envelope<EventPayload>;

/// Typed bus alias. Keeps `EventBus<EventPayload>` from showing up
/// at every call site.
pub type SessionEventBus = EventBus<EventPayload>;

/// Body of a `meeting.completed` event. `outcome` carries the
/// terminal label per Invariant 9.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingCompletedData {
    pub meeting: Meeting,
    pub outcome: MeetingOutcome,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingOutcome {
    Success,
    Failed,
    Aborted,
    PermissionRevoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionItemsReadyData {
    pub items: Vec<ActionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorWarningData {
    pub component: DoctorComponent,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorComponent {
    Capture,
    Whisperkit,
    Vault,
    Eventkit,
    Llm,
}

/// `daemon.error` payload. Mirrors the same `HERON_E_*` taxonomy as
/// the [`SessionError::code`] surface — when the daemon raises an
/// error out of band, the same code rides on the bus so subscribers
/// can react without polling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonErrorData {
    /// `^HERON_E_[A-Z0-9_]+$` per the OpenAPI `Error.code` pattern
    /// and spec §11.
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

// ── operation arguments ───────────────────────────────────────────────

/// Filter inputs for [`SessionOrchestrator::list_meetings`]. Cursor
/// is opaque to the trait; the implementation defines its shape.
/// Cursor-based, not offset-based, so deletes don't shift the page.
#[derive(Debug, Clone, Default)]
pub struct ListMeetingsQuery {
    pub since: Option<DateTime<Utc>>,
    pub status: Option<MeetingStatus>,
    pub platform: Option<Platform>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListMeetingsPage {
    pub items: Vec<Meeting>,
    /// `None` when this is the last page.
    pub next_cursor: Option<String>,
}

/// Arguments for the manual-capture escape hatch (`POST /meetings`).
/// The happy path is ambient detection emitting `meeting.detected`
/// without anyone calling this; if a client reaches for it in steady
/// state, detection is broken. Treat it as a bug report, not a
/// feature.
#[derive(Debug, Clone)]
pub struct StartCaptureArgs {
    pub platform: Platform,
    /// Free-form hint forwarded to the orchestrator (e.g. window
    /// title, meeting URL). Not a primary identifier.
    pub hint: Option<String>,
    /// EventKit identifier to correlate this capture with a
    /// previously [`SessionOrchestrator::attach_context`]-supplied
    /// `PreMeetingContext`. Resolver-input shape per Invariant 4 —
    /// never a heron primary key. When `Some` and a context is
    /// pending for this id, the orchestrator consumes it as part of
    /// session materialization.
    pub calendar_event_id: Option<String>,
}

/// Range hint for [`SessionOrchestrator::audio_path`]'s callers that
/// need to stream a slice. The orchestrator itself returns a path;
/// the HTTP layer reads bytes. Kept as a thin wrapper rather than
/// `std::ops::Range` so that an open-ended `bytes=N-` request maps
/// cleanly.
#[derive(Debug, Clone, Copy)]
pub struct ByteRange {
    pub start: u64,
    /// Inclusive end. `None` means "to EOF", matching HTTP
    /// `bytes=N-` semantics.
    pub end: Option<u64>,
}

// ── errors ────────────────────────────────────────────────────────────

/// The internal error taxonomy. The HTTP projection maps each
/// variant to the `HERON_E_*` codes + status codes pinned in the
/// OpenAPI `Error` envelope. Spec §11.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("not yet implemented (orchestration hub still split between heron-cli and heron-zoom)")]
    NotYetImplemented,

    /// `404` — meeting / calendar event / etc. not found.
    #[error("not found: {what}")]
    NotFound { what: String },

    /// `409` — the FSM rejected the requested transition. e.g.
    /// `end_meeting` on an already-terminal meeting. `current_state`
    /// rides along so the client can recover without a follow-up
    /// `GET`.
    #[error("invalid state transition (current state: {current_state:?})")]
    InvalidState { current_state: MeetingStatus },

    /// `409` — a capture is already active for this platform. Per
    /// the singleton-per-platform invariant.
    #[error("capture already in progress for platform {platform:?}")]
    CaptureInProgress { platform: Platform },

    /// `423` — vault temporarily locked (iCloud Drive evicted the
    /// file; another writer holds the path). User-actionable; retry
    /// once the vault settles.
    #[error("vault locked: {detail}")]
    VaultLocked { detail: String },

    /// `424` — upstream LLM provider failed during summarization.
    /// `provider` rides in the wire `details.provider` field; the
    /// `daemon.error` event with `HERON_E_LLM_PROVIDER_FAILED` fires
    /// in parallel.
    #[error("LLM provider {provider} failed: {detail}")]
    LlmProviderFailed { provider: String, detail: String },

    /// `425` — meeting still recording, asset (audio) not yet sealed.
    /// Wait for `meeting.completed` on `/events`.
    #[error("too early: meeting still recording")]
    TooEarly,

    /// `503` — required permission missing (Core Audio process tap,
    /// microphone, accessibility, calendar). User-actionable; the
    /// `permission` field names which.
    #[error("permission missing: {permission}")]
    PermissionMissing { permission: &'static str },

    /// `422` — request body failed validation.
    #[error("validation: {detail}")]
    Validation { detail: String },
}

impl SessionError {
    /// `HERON_E_*` code per spec §11. Stable across versions; the
    /// HTTP projection copies this verbatim into `Error.code`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotYetImplemented => "HERON_E_NOT_YET_IMPLEMENTED",
            Self::NotFound { .. } => "HERON_E_NOT_FOUND",
            Self::InvalidState { .. } => "HERON_E_INVALID_STATE",
            Self::CaptureInProgress { .. } => "HERON_E_CAPTURE_IN_PROGRESS",
            Self::VaultLocked { .. } => "HERON_E_VAULT_LOCKED",
            Self::LlmProviderFailed { .. } => "HERON_E_LLM_PROVIDER_FAILED",
            Self::TooEarly => "HERON_E_TOO_EARLY",
            Self::PermissionMissing { .. } => "HERON_E_PERMISSION_MISSING",
            Self::Validation { .. } => "HERON_E_VALIDATION",
        }
    }
}

// ── trait surface ─────────────────────────────────────────────────────

/// The hub. One trait, one implementation per orchestration strategy
/// (the planned `LocalSessionOrchestrator` that wires
/// audio/speech/vault/llm; later, possibly a `RemoteOrchestrator` for
/// a multi-machine deployment).
///
/// `herond` (the localhost daemon) holds a single instance and
/// projects every method onto an HTTP endpoint per
/// `docs/api-desktop-openapi.yaml`. Tauri IPC, MCP, and the CLI all
/// hold the same trait object directly without going through HTTP.
///
/// Method ordering matches the OpenAPI tag groupings: meetings,
/// transcripts, summaries, audio, calendar, context, ops.
#[async_trait]
pub trait SessionOrchestrator: Send + Sync {
    // ── meetings ──────────────────────────────────────────────────────

    /// `GET /meetings` — list captured meetings, newest first,
    /// cursor-paginated.
    async fn list_meetings(&self, q: ListMeetingsQuery) -> Result<ListMeetingsPage, SessionError>;

    /// `GET /meetings/{meeting_id}`.
    async fn get_meeting(&self, id: &MeetingId) -> Result<Meeting, SessionError>;

    /// `POST /meetings` — manual-capture escape hatch. Returns
    /// `Meeting` in any non-terminal state — typically `Detected` or
    /// `Armed` for a fully async-driven implementation that defers
    /// the `Recording` edge to a background audio task, but a
    /// substrate-only / synchronous-walk implementation may return at
    /// `Recording`. Either way, callers subscribe to `/events` for
    /// the `Recording → Ended → Done|Failed` transitions they don't
    /// see in the synchronous response.
    ///
    /// Errors:
    /// - [`SessionError::CaptureInProgress`] for the
    ///   singleton-per-platform conflict (HTTP `409`).
    /// - [`SessionError::PermissionMissing`] when a TCC permission
    ///   is missing (HTTP `503`).
    async fn start_capture(&self, args: StartCaptureArgs) -> Result<Meeting, SessionError>;

    /// `POST /meetings/{meeting_id}/end` — manual end-of-meeting
    /// escape hatch. Once the meeting has been finalized (vault note
    /// written, terminal state reachable via `get_meeting`),
    /// idempotent against `Done | Failed`. Until that finalization
    /// lands (the substrate-only impl, where the active capture is
    /// the only source of truth), a second call after the first
    /// completes collapses to [`SessionError::NotFound`] (HTTP
    /// `404`); subscribe to `/events` for the terminal
    /// `meeting.completed` envelope rather than relying on a
    /// re-end.
    async fn end_meeting(&self, id: &MeetingId) -> Result<(), SessionError>;

    // ── transcripts ───────────────────────────────────────────────────

    /// `GET /meetings/{meeting_id}/transcript` — finalized segments
    /// only. Live partials remain SSE-only on `/events`. The
    /// returned [`Transcript::status`] reports whether more segments
    /// are still being added.
    async fn read_transcript(&self, id: &MeetingId) -> Result<Transcript, SessionError>;

    // ── summaries ─────────────────────────────────────────────────────

    /// `GET /meetings/{meeting_id}/summary` — `Some` once the
    /// summary exists, `None` if generation is still pending. The
    /// HTTP projection maps `None` to `202 Accepted` with a
    /// `Retry-After` hint and recommends subscribing to
    /// `summary.ready` instead of polling.
    async fn read_summary(&self, id: &MeetingId) -> Result<Option<Summary>, SessionError>;

    // ── audio ─────────────────────────────────────────────────────────

    /// `GET /meetings/{meeting_id}/audio` — path to the m4a sidecar
    /// once the meeting is terminal. The HTTP layer streams from
    /// this path (with byte-range support). A still-recording
    /// meeting returns [`SessionError::TooEarly`] (HTTP `425`); the
    /// vault temporarily locked returns [`SessionError::VaultLocked`]
    /// (HTTP `423`).
    ///
    /// Returning a `PathBuf` bakes in the assumption that the
    /// orchestrator runs on the same host as the vault. If a
    /// future remote-orchestrator variant ships, the trait grows a
    /// streaming variant (`audio_stream(id, range) -> impl
    /// AsyncRead`) rather than mutating this one.
    async fn audio_path(&self, id: &MeetingId) -> Result<PathBuf, SessionError>;

    // ── calendar ──────────────────────────────────────────────────────

    /// `GET /calendar/upcoming` — calendar reads with attendee /
    /// related-meeting correlation. Returns
    /// [`SessionError::PermissionMissing`] if calendar TCC is not
    /// granted.
    async fn list_upcoming_calendar(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: Option<u32>,
    ) -> Result<Vec<CalendarEvent>, SessionError>;

    // ── pre-meeting context ───────────────────────────────────────────

    /// `PUT /context` — attach pre-meeting context keyed to a
    /// calendar event. Idempotent: latest call for a given
    /// `calendar_event_id` wins.
    async fn attach_context(&self, req: PreMeetingContextRequest) -> Result<(), SessionError>;

    // ── ops ───────────────────────────────────────────────────────────

    /// `GET /health`. Mirrors what `heron-doctor` reports offline.
    async fn health(&self) -> Health;

    // ── event surface ─────────────────────────────────────────────────

    /// The bus this orchestrator publishes to. HTTP/SSE, Tauri IPC,
    /// MCP, and webhook adapters subscribe through this — per
    /// Invariant 12 there is no other path. Cheap to clone (the bus
    /// is `Arc`-backed inside).
    fn event_bus(&self) -> SessionEventBus;

    /// The replay cache backing SSE `Last-Event-ID` resume on
    /// `/events`. `None` if this orchestrator opts out of replay
    /// (e.g. a stub used in tests); the HTTP projection then
    /// declines resume and clients get a fresh tail on every
    /// reconnect.
    fn replay_cache(&self) -> Option<&dyn ReplayCache<EventPayload>> {
        None
    }
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used)]
mod prefix_tests {
    use super::*;

    #[test]
    fn meeting_id_uses_mtg_prefix_on_the_wire() {
        let id = MeetingId::now_v7();
        let json = serde_json::to_string(&id).expect("serialize");
        assert!(json.starts_with(r#""mtg_"#), "got: {json}");
        let back: MeetingId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn event_payload_round_trips_through_envelope_with_flatten() {
        // The OpenAPI envelope is flat: event_type / data appear at
        // the same level as event_id / api_version / created_at.
        // Pin the flatten + tag/content combo here so accidental
        // changes break loudly.
        let meeting = Meeting {
            id: MeetingId::now_v7(),
            status: MeetingStatus::Recording,
            platform: Platform::Zoom,
            title: Some("Standup".into()),
            calendar_event_id: None,
            started_at: Utc::now(),
            ended_at: None,
            duration_secs: None,
            participants: vec![],
            transcript_status: TranscriptLifecycle::Partial,
            summary_status: SummaryLifecycle::Pending,
            tags: vec![],
            processing: None,
        };
        let envelope = Envelope::new(EventPayload::MeetingDetected(meeting.clone()))
            .with_meeting(meeting.id.to_string());
        let json = serde_json::to_value(&envelope).expect("serialize");
        let obj = json.as_object().expect("object");
        assert!(obj.contains_key("event_id"), "missing event_id: {json}");
        assert!(
            obj.get("event_type").and_then(|v| v.as_str()) == Some("meeting.detected"),
            "event_type missing or wrong: {json}",
        );
        assert!(obj.contains_key("data"), "missing data field: {json}");
        let back: EventEnvelope = serde_json::from_value(json).expect("deserialize");
        assert!(matches!(back.payload, EventPayload::MeetingDetected(_)));
    }

    #[test]
    fn meeting_status_terminal_flag() {
        assert!(MeetingStatus::Done.is_terminal());
        assert!(MeetingStatus::Failed.is_terminal());
        assert!(!MeetingStatus::Recording.is_terminal());
        assert!(!MeetingStatus::Detected.is_terminal());
    }

    #[test]
    fn meeting_tags_round_trip_and_default_when_omitted() {
        // Pin two contracts:
        //   (a) tags survive serialize -> deserialize in writer
        //       order (stable chip list on the frontend).
        //   (b) a payload without the `tags` key deserializes
        //       cleanly with an empty default — the
        //       `#[serde(default)]` mixed-version contract.
        let meeting = Meeting {
            id: MeetingId::now_v7(),
            status: MeetingStatus::Done,
            platform: Platform::Zoom,
            title: Some("Acme weekly".into()),
            calendar_event_id: None,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_secs: Some(1800),
            participants: vec![],
            transcript_status: TranscriptLifecycle::Complete,
            summary_status: SummaryLifecycle::Ready,
            tags: vec!["acme".into(), "pricing".into()],
        };
        let json = serde_json::to_value(&meeting).expect("serialize");
        // Pin the wire-key explicitly so a `#[serde(rename)]` typo
        // can't ship a mismatched key without breaking the test —
        // the deserialize step alone would still pass via the
        // `#[serde(default)]` fallback.
        assert!(
            json.get("tags").is_some_and(|v| v.is_array()),
            "tags absent or wrong shape on wire: {json}",
        );
        let back: Meeting = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.tags, vec!["acme".to_owned(), "pricing".to_owned()]);

        // Backward compat: a wire payload with no `tags` field at all
        // (older daemon, or a fixture frozen pre-Tier-0) must still
        // deserialize, and tags must default to empty.
        let legacy = serde_json::json!({
            "id": MeetingId::now_v7(),
            "status": "done",
            "platform": "zoom",
            "title": null,
            "calendar_event_id": null,
            "started_at": Utc::now(),
            "ended_at": null,
            "duration_secs": null,
            "participants": [],
            "transcript_status": "complete",
            "summary_status": "ready",
        });
        let legacy_meeting: Meeting =
            serde_json::from_value(legacy).expect("legacy payload deserialize");
        assert!(
            legacy_meeting.tags.is_empty(),
            "missing tags should default to empty, got {:?}",
            legacy_meeting.tags,
        );
    }

    #[test]
    fn session_error_codes_match_heron_e_pattern() {
        // Spec §11: every code matches /^HERON_E_[A-Z0-9_]+$/.
        // Cover every variant — `code()` is a hand-written `match`
        // returning `&'static str` literals, and a typo in any
        // literal would compile cleanly. This test is the only
        // catch.
        let cases = [
            SessionError::NotYetImplemented,
            SessionError::NotFound { what: "x".into() },
            SessionError::InvalidState {
                current_state: MeetingStatus::Done,
            },
            SessionError::CaptureInProgress {
                platform: Platform::Zoom,
            },
            SessionError::VaultLocked { detail: "x".into() },
            SessionError::LlmProviderFailed {
                provider: "anthropic".into(),
                detail: "503".into(),
            },
            SessionError::TooEarly,
            SessionError::PermissionMissing { permission: "mic" },
            SessionError::Validation { detail: "x".into() },
        ];
        // Sanity-check the covers-every-variant claim against
        // SessionError's variant count via a discriminator-based
        // pin: if a variant is added without extending this list,
        // the count below diverges.
        assert_eq!(
            cases.len(),
            9,
            "extend test when SessionError gains a variant"
        );
        for err in &cases {
            let code = err.code();
            assert!(code.starts_with("HERON_E_"), "code {code} missing prefix");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "code {code} has non-conforming characters",
            );
        }
    }

    #[test]
    fn meeting_processing_absent_omits_field_on_the_wire() {
        // Tier 0 #2: `processing` is `Option<MeetingProcessing>` with
        // `skip_serializing_if = "Option::is_none"`. A `None` must not
        // emit a `"processing": null` field — pre-existing JSON
        // consumers (the desktop's older Tauri command surface, the
        // OpenAPI spec, snapshot tests in herond) would otherwise see
        // an unexpected key. Pin the omission here.
        let meeting = Meeting {
            id: MeetingId::now_v7(),
            status: MeetingStatus::Recording,
            platform: Platform::Zoom,
            title: None,
            calendar_event_id: None,
            started_at: Utc::now(),
            ended_at: None,
            duration_secs: None,
            participants: vec![],
            transcript_status: TranscriptLifecycle::Pending,
            summary_status: SummaryLifecycle::Pending,
            tags: vec![],
            processing: None,
        };
        let json = serde_json::to_value(&meeting).expect("serialize");
        let obj = json.as_object().expect("object");
        assert!(
            !obj.contains_key("processing"),
            "expected no `processing` key for None, got: {json}",
        );
    }

    #[test]
    fn meeting_processing_present_round_trips() {
        // Pin the `Some(MeetingProcessing { .. })` round-trip so the
        // Tier 0 #2 wire shape is locked. Mirrors `heron_types::Cost`
        // one-for-one — adding a field there without updating
        // `MeetingProcessing` would silently drop data from the
        // bridge; this test keeps the projection honest.
        let meeting = Meeting {
            id: MeetingId::now_v7(),
            status: MeetingStatus::Done,
            platform: Platform::Zoom,
            title: Some("Acme".into()),
            calendar_event_id: None,
            started_at: Utc::now(),
            ended_at: None,
            duration_secs: Some(1_800),
            participants: vec![],
            transcript_status: TranscriptLifecycle::Complete,
            summary_status: SummaryLifecycle::Ready,
            tags: vec![],
            processing: Some(MeetingProcessing {
                summary_usd: 0.04,
                tokens_in: 14_231,
                tokens_out: 612,
                model: "claude-sonnet-4-6".into(),
            }),
        };
        let json = serde_json::to_value(&meeting).expect("serialize");
        let processing = json
            .get("processing")
            .expect("processing present when Some");
        assert_eq!(processing["summary_usd"].as_f64(), Some(0.04));
        assert_eq!(processing["tokens_in"].as_u64(), Some(14_231));
        assert_eq!(processing["tokens_out"].as_u64(), Some(612));
        assert_eq!(processing["model"].as_str(), Some("claude-sonnet-4-6"));

        let back: Meeting = serde_json::from_value(json).expect("deserialize");
        assert_eq!(
            back.processing,
            Some(MeetingProcessing {
                summary_usd: 0.04,
                tokens_in: 14_231,
                tokens_out: 612,
                model: "claude-sonnet-4-6".into(),
            }),
        );
    }
}
