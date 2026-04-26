//! `heron-event` — canonical event bus + transport-projection contract.
//!
//! The bus that every other crate publishes through and that every
//! consumer (in-proc Rust subscriber, Tauri IPC, MCP notifications,
//! HTTP/SSE on `herond`, outbound webhook) projects from. Per
//! [`docs/archives/api-design-spec.md`](../../../docs/archives/api-design-spec.md) §10
//! and Invariants 12–13:
//!
//! - **Invariant 12.** All events flow through `heron-event` first. No
//!   crate publishes events on its own private channel.
//! - **Invariant 13.** The trait is canonical; transports are
//!   projections. A new transport (gRPC, NATS, …) is purely additive;
//!   no adapter-specific event types exist.
//!
//! This crate carries *only the bus mechanics* — IDs, the generic
//! [`Envelope<P>`] framing, the [`EventBus<P>`] hub, the [`EventSink`]
//! trait that adapter crates implement, and the [`ReplayCache`]
//! contract that backs SSE `Last-Event-ID` resume on `/events`. The
//! domain payload types (`Meeting`, `TranscriptSegment`, `Summary`,
//! …) and the typed [`Envelope`] alias live in `heron-session`,
//! because the bus has no business knowing about meetings.
//!
//! The wire shape rendered by HTTP/SSE on `herond` is pinned in
//! [`docs/api-desktop-openapi.yaml`](../../../docs/api-desktop-openapi.yaml)
//! (`EventEnvelopeBase` / `EventEnvelope`); this Rust surface is the
//! authoritative source of which fields exist, the OpenAPI is the
//! authoritative source of how they appear on the wire. If they
//! disagree, the YAML file is the bug — see the header of that file.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

pub use heron_types::prefixed_id::IdParseError;

// ── identity ──────────────────────────────────────────────────────────

heron_types::prefixed_id! {
    /// Stripe-style prefixed UUIDv7 for an event. Wire form
    /// `evt_<lowercase-hyphenated-uuid>`. Time-ordered, so a sort
    /// by `EventId` is a sort by emission time — the property the
    /// SSE replay window relies on. Per `api-design-spec.md` §2 and
    /// the `EventId` schema in the OpenAPI.
    pub EventId, "evt"
}

// ── envelope ──────────────────────────────────────────────────────────

/// Default API version string baked into envelopes minted by this
/// build. Long-lived consumers should pin via the
/// `Heron-API-Version` header on HTTP / Tauri / MCP transports
/// rather than relying on this default. Bump in lockstep with the
/// OpenAPI `info.version`.
pub const CURRENT_API_VERSION: &str = "2026-04-25";

/// Common envelope carried by every event, parameterized by the
/// payload type `P`. Mirrors the OpenAPI `EventEnvelopeBase` plus the
/// `data` field. Envelope authors set everything in the header; the
/// payload variant is the discriminator.
///
/// `meeting_id` is `Option` because not every event is meeting-scoped
/// (`daemon.error`, `doctor.warning` may arrive before any capture
/// has started). It is typed as a free `String` here rather than a
/// `MeetingId`: this crate has no domain knowledge, and the bus
/// must accept events from layers that haven't decided on an ID
/// shape yet (e.g. `heron-bot` v2 uses a different prefix). Wrappers
/// in `heron-session` re-type it.
///
/// **Consistency contract.** Publishers MUST set `meeting_id` to
/// match whatever ID the payload carries (e.g. for
/// `transcript.partial`, the meeting the segment belongs to). The
/// trait can't enforce this — `TranscriptSegment` etc. don't
/// embed a meeting ID — so a misaligned envelope is a publisher
/// bug. Subscribers correlating across event types rely on this
/// invariant.
///
/// `payload` is flattened on the wire so `event_type` / `data`
/// appear as top-level fields in JSON, matching the YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<P> {
    pub event_id: EventId,
    pub api_version: String,
    pub created_at: DateTime<Utc>,
    pub meeting_id: Option<String>,
    #[serde(flatten)]
    pub payload: P,
}

impl<P> Envelope<P> {
    /// Mint an envelope with a fresh UUIDv7 ID, the current API
    /// version, `created_at = now`, and no `meeting_id`. Use
    /// [`Self::with_meeting`] to scope it to a meeting.
    pub fn new(payload: P) -> Self {
        Self {
            event_id: EventId::now_v7(),
            api_version: CURRENT_API_VERSION.to_owned(),
            created_at: Utc::now(),
            meeting_id: None,
            payload,
        }
    }

    /// Builder: scope this envelope to a meeting. Caller passes the
    /// stringified ID (the bus is domain-agnostic; see struct
    /// docs).
    pub fn with_meeting(mut self, meeting_id: impl Into<String>) -> Self {
        self.meeting_id = Some(meeting_id.into());
        self
    }
}

// ── bus ───────────────────────────────────────────────────────────────

/// Canonical in-process bus per spec §10. A thin facade over a Tokio
/// broadcast channel so that publishers can fire-and-forget without
/// caring how many subscribers are listening, and so that every
/// transport projection (HTTP/SSE, Tauri, MCP, webhook) is built by
/// subscribing to the same stream.
///
/// Generic over the envelope payload `P` so that `heron-session` can
/// instantiate a typed bus (`EventBus<EventEnvelope>`) while a future
/// crate that wants type-erased events can use `EventBus<Value>`.
///
/// `Capacity` is a fixed ring; a slow subscriber that lags by more
/// than `capacity` events sees a `RecvError::Lagged` and must
/// reconcile via [`ReplayCache`] (the same contract HTTP/SSE
/// `Last-Event-ID` rides on top of).
pub struct EventBus<P: Clone + Send + 'static> {
    sender: broadcast::Sender<Envelope<P>>,
}

impl<P: Clone + Send + 'static> EventBus<P> {
    /// Construct with a ring capacity. 1024 is a reasonable default
    /// for the desktop daemon — covers a long meeting's worth of
    /// `transcript.partial` deltas without dropping. Pick larger
    /// only if you measure subscriber lag.
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0`. `tokio::sync::broadcast::channel`
    /// panics on a zero capacity with a generic message; the
    /// assertion here surfaces a heron-specific one at the layer
    /// the misuse actually originated from.
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "EventBus::new requires capacity > 0 (a zero-capacity \
             broadcast channel can never deliver an event)",
        );
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an envelope. Returns the number of subscribers the
    /// event was delivered to. A return of `0` is not an error —
    /// it just means nobody was listening at this instant. Per
    /// Invariant 12 publishers should not branch on subscriber
    /// presence; the bus is fire-and-forget.
    pub fn publish(&self, envelope: Envelope<P>) -> usize {
        // `send` errors only when there are zero receivers; we still
        // want to report "delivered to N" so callers can record
        // metrics. Treat the no-receiver case as 0 deliveries.
        self.sender.send(envelope).unwrap_or(0)
    }

    /// Subscribe to the bus. The returned receiver is a fresh tail
    /// — historical events are not replayed here. Use
    /// [`ReplayCache`] if you need replay semantics (the SSE
    /// projection always does).
    pub fn subscribe(&self) -> broadcast::Receiver<Envelope<P>> {
        self.sender.subscribe()
    }

    /// How many subscribers currently hold a receiver. Diagnostic
    /// only; no behaviour should depend on this.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl<P: Clone + Send + 'static> Clone for EventBus<P> {
    /// Cheap clone — `tokio::sync::broadcast::Sender` is `Arc`-backed.
    /// Cloned handles publish into the same underlying channel.
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

// ── replay (SSE Last-Event-ID contract) ───────────────────────────────

/// Errors a [`ReplayCache`] may surface to a subscriber that asked
/// for resume.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// The named event ID falls outside the cache's retention
    /// window. The HTTP/SSE projection translates this to `410 Gone`
    /// per the `/events` spec; consumers reconnect without resume
    /// and accept the gap as unrecoverable.
    #[error(
        "replay window exceeded: requested event {requested} \
         is older than retention ({window_secs}s)"
    )]
    WindowExceeded {
        requested: EventId,
        window_secs: u64,
    },

    /// Cache temporarily unavailable (e.g. backed by a file the OS
    /// briefly evicted). Caller may retry.
    #[error("replay cache unavailable: {0}")]
    Unavailable(String),
}

/// Backs the `Last-Event-ID` / `?since_event_id` resume contract on
/// the `/events` SSE endpoint. Implementations hold a bounded
/// window of recent envelopes (default 3600s, surfaced via the
/// `X-Heron-Replay-Window-Seconds` response header) so that a
/// reconnecting subscriber can ask "give me everything strictly
/// after `evt_X`."
///
/// Lives in `heron-event` so any future bus transport (gRPC stream,
/// NATS, …) can opt in to the same resume semantics rather than
/// reinvent them.
#[async_trait]
pub trait ReplayCache<P: Clone + Send + 'static>: Send + Sync {
    /// Return events strictly after `since`, in emission order.
    ///
    /// Outcomes:
    /// - `Ok(vec![])` — `since` is at or after the cache's newest
    ///   entry (subscriber is already caught up).
    /// - `Ok(vec![…])` — events strictly after `since`, ordered.
    /// - `Err(WindowExceeded)` — `since` is older than retention
    ///   **OR** the cache has never seen `since`. Both map to the
    ///   same recovery (reconnect without resume), so they
    ///   collapse into one error variant rather than introducing a
    ///   spurious `Unknown` case the caller would handle
    ///   identically.
    async fn replay_since(&self, since: EventId) -> Result<Vec<Envelope<P>>, ReplayError>;

    /// Retention window in seconds. The HTTP layer copies this into
    /// `X-Heron-Replay-Window-Seconds` so a long-lived subscriber
    /// can size its reconnect logic. Default 3600.
    fn window(&self) -> Duration {
        Duration::from_secs(3600)
    }
}

// ── transport sinks ───────────────────────────────────────────────────

/// What can go wrong when an adapter (HTTP/SSE, Tauri IPC, MCP,
/// webhook) tries to forward an envelope to its consumer.
#[derive(Debug, Error)]
pub enum SinkError {
    /// The downstream connection went away. The bus subscription is
    /// still healthy; the adapter should drop and let the next
    /// reconnect re-establish.
    #[error("downstream consumer disconnected")]
    Disconnected,

    /// The adapter is back-pressured and chose to drop rather than
    /// block the bus. Webhook sinks with retry budgets surface this
    /// after exhausting retries.
    #[error("dropped after backpressure / retry budget exhausted")]
    Dropped,

    /// Adapter-specific transport error (HTTP 5xx, Tauri IPC error,
    /// MCP protocol violation). `String` is the projection's own
    /// message; this crate doesn't model transport-specific
    /// taxonomies (Invariant 13).
    #[error("transport: {0}")]
    Transport(String),
}

/// Implemented by every transport projection. The bus handle is
/// passed in at construction; the sink subscribes itself and forwards
/// envelopes onto its wire format. Per Invariant 13 the trait is
/// canonical and adapters are pure projections — they MUST NOT
/// invent event types or filter fields the bus doesn't know about.
///
/// Concrete impls live in adapter crates (e.g. a future
/// `heron-event-http` for the SSE projection, `heron-event-tauri`
/// for desktop IPC). The trait stays here so adapters can be added
/// without touching publishers.
#[async_trait]
pub trait EventSink<P: Clone + Send + 'static>: Send + Sync {
    /// Forward one envelope. Called on every event the sink's
    /// subscription receives. The sink owns its own retry / batching
    /// strategy; the bus does not buffer on its behalf beyond the
    /// broadcast ring.
    async fn forward(&self, envelope: &Envelope<P>) -> Result<(), SinkError>;

    /// A short opaque label the daemon uses for logging / metrics
    /// (`"http-sse"`, `"tauri-ipc"`, `"mcp"`, `"webhook:<host>"`).
    fn label(&self) -> &str;
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    //! Pin the bus mechanics independently of any payload type: a
    //! future heron-bot v2 bus or a third-party domain bus will
    //! depend on the same publish / subscribe / clone behaviour.

    use super::*;

    #[test]
    fn envelope_new_sets_defaults() {
        let env = Envelope::new("hello".to_owned());
        assert_eq!(env.api_version, CURRENT_API_VERSION);
        assert!(env.meeting_id.is_none());
        assert_eq!(env.payload, "hello");
    }

    #[test]
    fn envelope_with_meeting_sets_id() {
        let env = Envelope::new(()).with_meeting("mtg_xyz");
        assert_eq!(env.meeting_id.as_deref(), Some("mtg_xyz"));
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_returns_zero() {
        let bus: EventBus<u32> = EventBus::new(8);
        let delivered = bus.publish(Envelope::new(7));
        assert_eq!(delivered, 0);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn publish_delivers_to_each_subscriber() {
        let bus: EventBus<u32> = EventBus::new(8);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        let delivered = bus.publish(Envelope::new(42));
        assert_eq!(delivered, 2);
        assert_eq!(a.recv().await.expect("a recv").payload, 42);
        assert_eq!(b.recv().await.expect("b recv").payload, 42);
    }

    #[tokio::test]
    async fn cloned_handle_publishes_to_same_channel() {
        // Cheap-clone is the contract that lets each adapter hold
        // its own handle; if a clone secretly created a new
        // channel, sinks would silently miss events.
        let bus: EventBus<u32> = EventBus::new(8);
        let mut sub = bus.subscribe();
        let clone = bus.clone();
        let delivered = clone.publish(Envelope::new(99));
        assert_eq!(delivered, 1);
        assert_eq!(sub.recv().await.expect("recv").payload, 99);
    }

    #[test]
    fn event_id_uses_evt_prefix_on_the_wire() {
        let id = EventId::now_v7();
        let json = serde_json::to_string(&id).expect("serialize");
        assert!(json.starts_with(r#""evt_"#), "got: {json}");
        let back: EventId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    #[should_panic(expected = "capacity > 0")]
    fn zero_capacity_panics_with_clear_message() {
        // Surface the misuse at this layer with a heron-specific
        // message instead of letting tokio's generic
        // `assertion failed: capacity > 0` bubble out.
        let _: EventBus<u32> = EventBus::new(0);
    }
}
