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
//!   replay. The header name is shared with [`heron_event_http`] via
//!   the [`heron_event_http::REPLAY_WINDOW_HEADER`] constant — never
//!   stringified inline so a future spec rename only touches one
//!   place.
//!
//! ## Why we don't use `heron_event_http::SseEventSink` directly
//!
//! [`heron_event_http::SseEventSink`] is a per-connection
//! [`heron_event::EventSink`] that writes [`heron_event_http::SseFrame`]
//! strings into a `tokio::sync::mpsc` channel. That shape fits a
//! transport whose body is built by the daemon itself (raw socket
//! writer, webhook poster) — but the axum handler here lives inside
//! a framework that wants `Stream<Item = axum::response::sse::Event>`
//! plus a built-in [`axum::response::sse::KeepAlive`] scheduler. Going
//! through `SseFrame` would require either parsing the formatted
//! string back into an `Event` (silly) or bypassing axum's `Sse`
//! response entirely (and re-implementing the heartbeat scheduler).
//! Net: at the HTTP-handler layer we use axum's idiom; the
//! `SseEventSink` building block is reserved for the non-HTTP
//! transports (webhook, MCP, raw TCP) where there's no framework to
//! work with. We *do* reuse [`heron_event_http`]'s pieces that are
//! framework-agnostic — [`REPLAY_WINDOW_HEADER`] and
//! [`heron_event_http::TopicFilter`].
//!
//! [`REPLAY_WINDOW_HEADER`]: heron_event_http::REPLAY_WINDOW_HEADER

use std::convert::Infallible;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{Stream, StreamExt};
use heron_event::{EventId, ReplayError};
use heron_event_http::{REPLAY_WINDOW_HEADER, TopicFilter};
use serde::Deserialize;
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
    /// Compiled via [`heron_event_http::TopicFilter::parse`]; missing
    /// / empty / `*` collapses to "all events" per the OpenAPI
    /// default. Applied uniformly to replayed and live events so a
    /// reconnect-with-resume sees the same filter as steady state.
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

    // Compile the topic filter once outside the per-event hot path.
    // `parse` collapses missing / empty / wildcard to `All`, so a
    // request without `?topics=` short-circuits to "match every
    // event_type" without running the glob engine.
    //
    // Wrapped in `Arc` because the per-event `filter_map` closure
    // produces a fresh future per item; cloning the inner
    // `TopicFilter::Globs(Vec<TopicGlob>)` would allocate a new Vec
    // and clone every pattern `String` on every event. Arc clone is
    // a single refcount bump, regardless of the glob count.
    let topics = Arc::new(TopicFilter::parse(q.topics.as_deref().unwrap_or("")));

    let replayed = if let Some(since) = resume {
        match state.orchestrator.replay_cache() {
            Some(cache) => {
                headers_out.insert(
                    REPLAY_WINDOW_HEADER,
                    HeaderValue::from(cache.window().as_secs()),
                );
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

    // Map (and drop) — if envelope serialization fails (which is
    // infallible for the typed `EventPayload` variants today, but
    // we don't want a future variant introducing a `serde_json::Error`
    // to silently emit a `{}` frame that violates the
    // `EventEnvelope` shape on the wire), we skip the frame entirely
    // and log the failure so the daemon's stream contract is never
    // corrupted. Subscribers notice gaps via `event_id`
    // discontinuity, which is recoverable; a malformed envelope is
    // not.
    //
    // NOTE for future authors: any per-event filter (the `topics`
    // query param will route through here) MUST be applied to BOTH
    // `replay_stream` and `live_stream`. The current chain shape
    // makes it tempting to filter only `live`; that would silently
    // leak filtered events through replay. Apply the filter on
    // `merged` to guarantee uniformity.
    let event_stream = merged.filter_map(move |res| {
        // `Arc::clone` per iteration is a single refcount bump — the
        // compiled filter is shared, not duplicated, even when the
        // `Globs` variant carries a non-trivial `Vec<TopicGlob>`.
        let topics = Arc::clone(&topics);
        async move {
            let env = res.ok()?;
            let event_type = env.payload.event_type();
            // Topic filter applies BEFORE serialization so a
            // wide-tail subscriber's narrow filter doesn't pay the
            // serialization cost on dropped events. Mirrors
            // [`heron_event_http::SseEventSink::forward`]'s
            // filter-then-serialize order.
            if !topics.matches(event_type) {
                return None;
            }
            let id = env.event_id.to_string();
            let data = match serde_json::to_string(&env) {
                Ok(s) => s,
                Err(err) => {
                    tracing::error!(
                        %err,
                        event_id = %env.event_id,
                        event_type,
                        "envelope serialization failed; dropping SSE frame",
                    );
                    return None;
                }
            };
            Some(Ok::<_, Infallible>(
                Event::default().id(id).event(event_type).data(data),
            ))
        }
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
