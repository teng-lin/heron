//! `GET /events` — Server-Sent Events projection of the orchestrator
//! event bus.
//!
//! Wire shape per `docs/api-desktop-openapi.yaml`:
//! - Response is `text/event-stream`. Each event carries `id:`,
//!   `event:`, and `data:` lines; `data:` is the JSON-encoded
//!   [`heron_session::EventEnvelope`].
//! - The SSE framing's `id` / `event` duplicate the values inside
//!   `data` — intentional, so non-SSE projections (webhook, MCP) can
//!   carry the envelope as JSON without losing typing. The SSE-only
//!   audience can switch on the framing; the JSON-only audience can
//!   switch on the envelope.
//! - Heartbeats: `:heartbeat\n\n` SSE comment frames every 15s.
//!   Comments are ignored by spec-compliant clients; they exist to
//!   defeat idle-connection drops.
//! - Resume: `Last-Event-ID` (auto-sent by user agents on reconnect)
//!   and `?since_event_id` are honored. The replay cache (if the
//!   orchestrator provides one) replays events strictly after the
//!   named ID. If the named ID is older than the cache's window, the
//!   daemon returns `410 Gone`; consumers reconnect without resume
//!   and accept the gap as unrecoverable.
//! - The `X-Heron-Replay-Window-Seconds` response header advertises
//!   the cache's retention so a long-lived consumer can size its
//!   reconnect logic. Omitted when the orchestrator opts out of
//!   replay.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{Stream, StreamExt};
use heron_event::{EventId, ReplayError};
use heron_session::EventPayload;
use serde::Deserialize;
use std::str::FromStr;
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;
use crate::error::WireError;

/// Default heartbeat interval. Spec says 15s; the daemon may want to
/// dial this for testing but production should match what the spec
/// advertises.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

pub fn router() -> Router<AppState> {
    Router::new().route("/events", get(get_events))
}

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// Comma-separated topic globs (e.g. `meeting.*,transcript.final`).
    /// Reserved for v1.1 — currently ignored. Documented in the
    /// OpenAPI so consumers can start sending it.
    #[allow(dead_code)]
    pub topics: Option<String>,
    /// Replay events strictly after this `evt_*` ID. `Last-Event-ID`
    /// header wins on conflict.
    pub since_event_id: Option<String>,
}

async fn get_events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> Response {
    // SSE-standard `Last-Event-ID` header beats `?since_event_id`
    // per the OpenAPI: the spec is explicit that user agents send
    // the header automatically on reconnect, and we want consumers
    // to be able to round-trip a reconnect without parsing the URL.
    let resume_raw = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or(q.since_event_id);

    let resume = match resume_raw.as_deref().map(EventId::from_str) {
        Some(Ok(id)) => Some(id),
        Some(Err(err)) => {
            // A malformed resume marker is a client bug, not a
            // recoverable condition — don't silently fall through to
            // a fresh tail (the consumer would think it caught up
            // when it hadn't).
            return WireError::new(
                "Validation",
                "HERON_E_VALIDATION",
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("malformed Last-Event-ID / since_event_id: {err}"),
            )
            .into_response();
        }
        None => None,
    };

    // Replay first, then live tail. The order matters: subscribe
    // BEFORE running the replay query so any event published in the
    // gap between "cache returns" and "live stream starts" is
    // observed by the live subscriber rather than dropped. (Receiver
    // dedup against the replayed prefix is handled implicitly: the
    // bus only delivers events emitted AFTER subscribe(), and the
    // cache returns events emitted strictly AFTER `resume`. Any
    // event whose ID falls in both ranges is observed twice; the
    // typed `event_id` lets consumers deduplicate. We accept that
    // micro-overlap rather than a single-flight lock that would
    // serialize every reconnect against publish.)
    let bus = state.orchestrator.event_bus();
    let live = BroadcastStream::new(bus.subscribe());

    let mut headers_out = HeaderMap::new();

    let replayed = if let Some(since) = resume {
        match state.orchestrator.replay_cache() {
            Some(cache) => {
                if let Some(window) = window_secs(cache.window()) {
                    headers_out.insert("x-heron-replay-window-seconds", HeaderValue::from(window));
                }
                match cache.replay_since(since).await {
                    Ok(events) => events,
                    Err(ReplayError::WindowExceeded { .. }) => {
                        return WireError::new(
                            "ReplayWindowExceeded",
                            "HERON_E_REPLAY_WINDOW_EXCEEDED",
                            StatusCode::GONE,
                            "requested event id is older than the replay window; \
                             reconnect without resume and treat the gap as unrecoverable",
                        )
                        .into_response();
                    }
                    Err(ReplayError::Unavailable(detail)) => {
                        return WireError::new(
                            "ReplayUnavailable",
                            "HERON_E_REPLAY_UNAVAILABLE",
                            StatusCode::SERVICE_UNAVAILABLE,
                            format!("replay cache unavailable: {detail}"),
                        )
                        .into_response();
                    }
                }
            }
            None => {
                // No cache configured; resume is best-effort. Spec:
                // "the HTTP projection then declines resume and
                // clients get a fresh tail on every reconnect."
                // Don't error — consumers reconnect routinely and
                // we don't want every reconnect to fail.
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let replay_stream = futures_util::stream::iter(replayed.into_iter().map(Ok::<_, Infallible>));
    let live_stream = live.filter_map(|res| async move {
        match res {
            Ok(env) => Some(Ok::<_, Infallible>(env)),
            Err(_lagged) => {
                // BroadcastStream::Lagged means this subscriber fell
                // behind the channel ring. The HTTP projection has
                // no in-band way to surface that beyond closing the
                // stream and forcing a reconnect; for the vertical
                // slice we drop the lag notice and let the consumer
                // notice gaps via event_id discontinuity. A future
                // pass can emit a synthetic `daemon.error
                // HERON_E_LAGGED` event before closing.
                None
            }
        }
    });
    let merged = replay_stream.chain(live_stream);

    let event_stream = merged.map(|res| {
        res.map(|env| {
            // event_type rides flattened on the envelope; surface it
            // up so the SSE framing carries a typed `event:` field
            // that matches the OpenAPI taxonomy. Match the enum
            // directly rather than round-tripping the payload
            // through `serde_json::Value` just to read the tag —
            // that saves a heap allocation per event on the SSE
            // hot path.
            let event_type = event_type_of(&env.payload);
            let id = env.event_id.to_string();
            let data = serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_owned());
            Event::default().id(id).event(event_type).data(data)
        })
    });

    let pinned: Pinned<Result<Event, Infallible>> = Box::pin(event_stream);
    let sse = Sse::new(pinned).keep_alive(
        KeepAlive::new()
            .interval(HEARTBEAT_INTERVAL)
            .text("heartbeat"),
    );

    (headers_out, sse).into_response()
}

/// Box-pin alias to keep the `Sse::new` argument type tractable.
/// Erases the chain-of-stream-adapters type so a future change to
/// the upstream stream pipeline doesn't propagate into a type
/// signature explosion.
type Pinned<T> = std::pin::Pin<Box<dyn Stream<Item = T> + Send>>;

fn window_secs(d: Duration) -> Option<u64> {
    Some(d.as_secs())
}

/// Map an `EventPayload` variant to its OpenAPI `event_type` literal.
/// Hand-coded match: keeps the SSE projection on the hot path
/// allocation-free and pins the wire taxonomy in code so a future
/// variant added without updating this fn fails the exhaustive-match
/// check.
fn event_type_of(p: &EventPayload) -> &'static str {
    match p {
        EventPayload::MeetingDetected(_) => "meeting.detected",
        EventPayload::MeetingArmed(_) => "meeting.armed",
        EventPayload::MeetingStarted(_) => "meeting.started",
        EventPayload::MeetingEnded(_) => "meeting.ended",
        EventPayload::MeetingCompleted(_) => "meeting.completed",
        EventPayload::MeetingParticipantJoined(_) => "meeting.participant_joined",
        EventPayload::TranscriptPartial(_) => "transcript.partial",
        EventPayload::TranscriptFinal(_) => "transcript.final",
        EventPayload::SummaryReady(_) => "summary.ready",
        EventPayload::ActionItemsReady(_) => "action_items.ready",
        EventPayload::DoctorWarning(_) => "doctor.warning",
        EventPayload::DaemonError(_) => "daemon.error",
    }
}
