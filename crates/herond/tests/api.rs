//! In-process integration tests.
//!
//! Drives the daemon via `tower::ServiceExt::oneshot` against a built
//! [`axum::Router`] — no real port binds, no real time waits. SSE is
//! tested by reading the response body bytes and asserting on the
//! event-stream framing, which makes the test deterministic without
//! needing a 15-second sleep for heartbeats.

// Tests use unwrap/expect freely; the workspace clippy lints deny
// them in production code but tests are the explicit exception
// (matches the convention in heron-policy etc.).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Utc};
use heron_event::{Envelope, EventId, ReplayCache, ReplayError};
use heron_session::{
    AudioLevelChannel, AudioLevelData, CalendarEvent, EventPayload, Health, ListMeetingsPage,
    ListMeetingsQuery, Meeting, MeetingId, PreMeetingContextRequest, PrepareContextRequest,
    SessionError, SessionEventBus, SessionOrchestrator, SpeakerChangedData, StartCaptureArgs,
    Summary, Transcript,
};
use herond::stub::StubOrchestrator;
use herond::{AppState, AuthConfig, build_app};
use tokio::sync::Mutex;
use tower::ServiceExt;

const TEST_BEARER: &str = "test-bearer-abcdef";

fn test_state(orch: Arc<dyn SessionOrchestrator>) -> AppState {
    AppState {
        orchestrator: orch,
        auth: Arc::new(AuthConfig {
            bearer: TEST_BEARER.to_owned(),
        }),
    }
}

fn stub_state() -> AppState {
    test_state(Arc::new(StubOrchestrator::new()))
}

#[tokio::test]
async fn health_returns_200_without_bearer() {
    // Per the OpenAPI: GET /health is `security: []`. The auth
    // middleware allowlists this exact path; if that ever
    // regresses, this test catches it.
    let app = build_app(stub_state());
    let res = app
        .oneshot(Request::get("/v1/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["status"], "degraded");
    assert!(body["components"]["capture"]["state"] == "permission_missing");
}

#[tokio::test]
async fn health_response_serializes_health_shape() {
    // Pin the OpenAPI Health envelope shape — the stub claim is
    // that wire form matches `components.schemas.Health` exactly.
    let app = build_app(stub_state());
    let res = app
        .oneshot(Request::get("/v1/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_json(res).await;
    let components = body["components"].as_object().expect("components");
    for key in ["capture", "whisperkit", "vault", "eventkit", "llm"] {
        assert!(components.contains_key(key), "missing component {key}");
        assert!(components[key]["state"].is_string());
    }
}

#[tokio::test]
async fn protected_endpoint_rejects_missing_bearer() {
    let app = build_app(stub_state());
    let res = app
        .oneshot(Request::get("/v1/meetings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_UNAUTHORIZED");
    assert_eq!(body["success"], false);
}

#[tokio::test]
async fn protected_endpoint_rejects_wrong_bearer() {
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/v1/meetings")
                .header(header::AUTHORIZATION, "Bearer wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_endpoint_with_correct_bearer_reaches_handler() {
    // The stub returns NotYetImplemented → HTTP 501. The test
    // doesn't care about the body; it cares that auth let the
    // request through.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/v1/meetings")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_NOT_YET_IMPLEMENTED");
}

#[tokio::test]
async fn origin_header_is_rejected_even_for_health() {
    // The OpenAPI is explicit: any browser-style fetch is denied
    // at the daemon, regardless of which endpoint. The middleware
    // runs before /health's security: [] allowance.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/v1/health")
                .header(header::ORIGIN, "http://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_ORIGIN_DENIED");
}

#[tokio::test]
async fn stub_orchestrator_endpoints_all_return_501() {
    // The herond test suite uses `StubOrchestrator`, which returns
    // `NotYetImplemented` for every operation. With the v1 routes
    // now actually wired through to the orchestrator, that means
    // every endpoint returns 501 here. The `MeetingId` in the path
    // is a well-formed `mtg_<uuid>` so the typed `Path<MeetingId>`
    // extractor passes — we want to test the orchestrator response,
    // not the extractor's malformed-id rejection.
    const VALID_MTG: &str = "mtg_01234567-89ab-7def-8000-000000000001";
    let cases = [
        ("GET", &format!("/v1/meetings/{VALID_MTG}")[..]),
        ("GET", &format!("/v1/meetings/{VALID_MTG}/transcript")[..]),
        ("GET", &format!("/v1/meetings/{VALID_MTG}/summary")[..]),
        ("GET", &format!("/v1/meetings/{VALID_MTG}/audio")[..]),
        ("GET", "/v1/calendar/upcoming"),
    ];
    for (method, path) in cases {
        let res = build_app(stub_state())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED, "{method} {path}");
    }
}

#[tokio::test]
async fn prepare_context_route_requires_bearer() {
    // The new `POST /v1/context/prepare` route lives next to
    // `PUT /v1/context` and inherits the same bearer middleware. Pin
    // the auth gate so a future router refactor can't accidentally
    // expose it on the unauthenticated surface.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::post("/v1/context/prepare")
                .header(header::AUTHORIZATION, "Bearer wrong")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"calendar_event_id":"evt_x","attendees":[]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn prepare_context_route_reaches_orchestrator() {
    // Pin the URL/method/auth/extractor wiring end-to-end. The stub
    // returns `NotYetImplemented` → 501; what we're locking in is
    // that the route is reachable with valid auth and a valid body
    // (so the JSON extractor accepts the shape) — not the eventual
    // 204 success path, which the orchestrator unit tests cover.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::post("/v1/context/prepare")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"calendar_event_id":"evt_route_test","attendees":[]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_NOT_YET_IMPLEMENTED");
}

#[tokio::test]
async fn malformed_meeting_id_in_path_returns_400() {
    // Path<MeetingId> rejects non-`mtg_<uuid>` values with a 400.
    // Pin the rejection so a future change to the path-extractor
    // shape is caught — silently changing this to 404 (route
    // doesn't match) would let path-traversal-shaped ids reach
    // the handler.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/v1/meetings/not-an-mtg-id")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn events_emits_published_envelope_in_sse_framing() {
    // Subscribe via /events, publish a meeting.detected envelope to
    // the bus, drain the response body, and confirm the SSE framing
    // carries the event_id, event_type, and JSON-encoded data.
    let stub = Arc::new(StubOrchestrator::new());
    let bus: SessionEventBus = stub.event_bus();
    let app = build_app(test_state(stub.clone()));

    // Spawn the request first so the subscriber is up before we
    // publish — broadcast::Sender drops events with no live
    // receivers. We deterministically wait for the SSE handler to
    // reach `bus.subscribe()` by polling `subscriber_count` rather
    // than racing on a fixed sleep.
    let req = Request::get("/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus).await;

    let envelope = sample_envelope();
    let delivered = bus.publish(envelope.clone());
    assert_eq!(delivered, 1, "subscriber not registered before publish");

    // Drop the bus handle so the broadcast channel closes once the
    // last subscriber receives our event — that ends the SSE stream
    // and lets `to_bytes` return.
    drop(bus);
    drop(stub);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;

    // SSE framing: must contain `id: evt_…`, `event: meeting.detected`,
    // `data: {…}`. We're not parsing the wire format — just spot-
    // checking the frames so a future change to the framing breaks
    // here loudly.
    assert!(
        body.contains(&format!("id: {}", envelope.event_id)),
        "missing id frame in: {body}"
    );
    assert!(
        body.contains("event: meeting.detected"),
        "missing event-type frame in: {body}"
    );
    assert!(
        body.contains("\"event_type\":\"meeting.detected\""),
        "missing payload event_type field in: {body}"
    );
}

#[tokio::test]
async fn events_emits_speaker_changed_in_sse_framing() {
    // Tier 0b #4 bridge guard: a `speaker.changed` envelope pushed
    // through the bus must reach the SSE stream with the documented
    // wire shape (`event: speaker.changed`, `data: {…t,name,started}`).
    // The AX observer in `heron-zoom` has emitted these events for
    // months but they died inside the offline aligner; this is the
    // assertion that the bridge through `heron-cli::pipeline` →
    // `SessionEventBus` → `herond::routes::events` is wired.
    let stub = Arc::new(StubOrchestrator::new());
    let bus: SessionEventBus = stub.event_bus();
    let app = build_app(test_state(stub.clone()));

    let req = Request::get("/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus).await;

    let meeting_id = MeetingId::now_v7();
    let envelope = Envelope::new(EventPayload::SpeakerChanged(SpeakerChangedData {
        t: 12.5,
        name: "Alice".to_owned(),
        started: true,
    }))
    .with_meeting(meeting_id.to_string());
    let delivered = bus.publish(envelope.clone());
    assert_eq!(delivered, 1, "subscriber not registered before publish");

    drop(bus);
    drop(stub);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;

    assert!(
        body.contains(&format!("id: {}", envelope.event_id)),
        "missing id frame in: {body}",
    );
    assert!(
        body.contains("event: speaker.changed"),
        "missing speaker.changed event-type frame in: {body}",
    );
    assert!(
        body.contains("\"event_type\":\"speaker.changed\""),
        "missing event_type field in payload: {body}",
    );
    assert!(
        body.contains("\"name\":\"Alice\""),
        "speaker name missing from payload: {body}",
    );
    assert!(
        body.contains("\"started\":true"),
        "started flag missing from payload: {body}",
    );
}

#[tokio::test]
async fn events_emits_audio_level_in_sse_framing() {
    // Tier 3 #15 bridge guard: an `audio.level` envelope pushed
    // through the bus must reach the SSE stream with the documented
    // wire shape (`event: audio.level`, `data: {…t,channel,peak_dbfs,
    // rms_dbfs}`). Mirrors `events_emits_speaker_changed_in_sse_framing`
    // — same projection, different payload — so a future refactor of
    // the SSE encode that special-cases one variant breaks the other
    // loudly here.
    let stub = Arc::new(StubOrchestrator::new());
    let bus: SessionEventBus = stub.event_bus();
    let app = build_app(test_state(stub.clone()));

    let req = Request::get("/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus).await;

    let meeting_id = MeetingId::now_v7();
    let envelope = Envelope::new(EventPayload::AudioLevel(AudioLevelData {
        t: 4.25,
        channel: AudioLevelChannel::MicClean,
        peak_dbfs: -3.5,
        rms_dbfs: -18.0,
    }))
    .with_meeting(meeting_id.to_string());
    let delivered = bus.publish(envelope.clone());
    assert_eq!(delivered, 1, "subscriber not registered before publish");

    drop(bus);
    drop(stub);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;

    assert!(
        body.contains(&format!("id: {}", envelope.event_id)),
        "missing id frame in: {body}",
    );
    assert!(
        body.contains("event: audio.level"),
        "missing audio.level event-type frame in: {body}",
    );
    assert!(
        body.contains("\"event_type\":\"audio.level\""),
        "missing event_type field in payload: {body}",
    );
    assert!(
        body.contains("\"channel\":\"mic_clean\""),
        "channel missing or wrong on wire: {body}",
    );
    assert!(
        body.contains("\"peak_dbfs\":-3.5"),
        "peak_dbfs missing or wrong on wire: {body}",
    );
}

#[tokio::test]
async fn events_topics_filter_passes_speaker_changed() {
    // Pair to the test above: a client subscribing with
    // `?topics=speaker.*` should still receive `speaker.changed`
    // envelopes — regression guard against a future glob refactor
    // that accidentally narrows the wildcard match.
    let stub = Arc::new(StubOrchestrator::new());
    let bus: SessionEventBus = stub.event_bus();
    let app = build_app(test_state(stub.clone()));

    let req = Request::get("/v1/events?topics=speaker.*")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus).await;

    let envelope = Envelope::new(EventPayload::SpeakerChanged(SpeakerChangedData {
        t: 1.0,
        name: "them".to_owned(),
        started: false,
    }))
    .with_meeting(MeetingId::now_v7().to_string());
    bus.publish(envelope);
    drop(bus);
    drop(stub);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;
    assert!(
        body.contains("event: speaker.changed"),
        "speaker.changed filtered out by speaker.* glob: {body}",
    );
}

#[tokio::test]
async fn events_replays_from_replay_cache_then_takes_live_tail() {
    let recorded = sample_envelope();
    let cache = Arc::new(InMemoryCache::new(vec![recorded.clone()]));
    let orch = Arc::new(WithCache::new(cache.clone()));
    let app = build_app(test_state(orch.clone()));

    // Resume from a known-old `EventId` distinct from what the
    // cache holds; the cache's policy is "ignore the since marker
    // and replay everything we have" for simplicity. That's fine
    // for asserting the replay path is wired.
    let resume_from = EventId::now_v7();
    let bus_handle = orch.event_bus();
    let req = Request::get(format!("/v1/events?since_event_id={resume_from}"))
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus_handle).await;

    // Drop the orchestrator handle so the bus closes and SSE ends.
    drop(bus_handle);
    drop(orch);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let window = response
        .headers()
        .get("x-heron-replay-window-seconds")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("replay window header missing");
    assert_eq!(window, "3600");
    let body = collect_body_to_string(response).await;
    assert!(
        body.contains(&format!("id: {}", recorded.event_id)),
        "replayed event missing in stream: {body}"
    );
}

#[tokio::test]
async fn events_topics_filter_passes_matching_event_type() {
    // `?topics=meeting.*` should let `meeting.detected` through.
    // Same shape as the unfiltered test, just with the filter
    // attached — confirms the filter compile + application path
    // is wired and the wildcard glob matches as the OpenAPI
    // documents.
    let stub = Arc::new(StubOrchestrator::new());
    let bus: SessionEventBus = stub.event_bus();
    let app = build_app(test_state(stub.clone()));

    let req = Request::get("/v1/events?topics=meeting.*")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus).await;

    let envelope = sample_envelope();
    bus.publish(envelope.clone());
    drop(bus);
    drop(stub);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;
    assert!(
        body.contains("event: meeting.detected"),
        "matching event filtered out: {body}"
    );
}

#[tokio::test]
async fn events_topics_filter_drops_non_matching_event_type() {
    // `?topics=transcript.*` should drop a `meeting.detected`. The
    // SSE stream closes once we drop the bus, so the body must be
    // empty of any `event: meeting.detected` frame — heartbeats
    // (15s interval) won't fire in this test's lifetime.
    let stub = Arc::new(StubOrchestrator::new());
    let bus: SessionEventBus = stub.event_bus();
    let app = build_app(test_state(stub.clone()));

    let req = Request::get("/v1/events?topics=transcript.*")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus).await;

    let envelope = sample_envelope();
    bus.publish(envelope);
    drop(bus);
    drop(stub);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;
    assert!(
        !body.contains("event: meeting.detected"),
        "non-matching event leaked through filter: {body}"
    );
    assert!(
        !body.contains("\"event_type\":\"meeting.detected\""),
        "filtered event payload leaked through: {body}"
    );
}

#[tokio::test]
async fn events_topics_filter_applies_to_replayed_events_too() {
    // Regression guard for the `events.rs` warning: filter must
    // apply to BOTH replay and live. A future refactor moving the
    // filter onto only `live_stream` would still pass the live-
    // tail filter tests above; this one catches that.
    //
    // Setup: cache holds a `meeting.detected` envelope; request
    // comes in with `?topics=transcript.*` which excludes it. The
    // body must not contain the meeting frame.
    let recorded = sample_envelope();
    let cache = Arc::new(InMemoryCache::new(vec![recorded.clone()]));
    let orch = Arc::new(WithCache::new(cache));
    let app = build_app(test_state(orch.clone()));

    let resume_from = EventId::now_v7();
    let bus_handle = orch.event_bus();
    let req = Request::get(format!(
        "/v1/events?topics=transcript.*&since_event_id={resume_from}"
    ))
    .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
    .body(Body::empty())
    .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus_handle).await;

    drop(bus_handle);
    drop(orch);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;
    assert!(
        !body.contains("event: meeting.detected"),
        "replayed event leaked through topic filter: {body}"
    );
    assert!(
        !body.contains(&format!("id: {}", recorded.event_id)),
        "replayed envelope's id leaked through filter: {body}"
    );
}

#[tokio::test]
async fn events_topics_filter_supports_multiple_globs() {
    // `?topics=meeting.*,doctor.warning` — two globs, comma-
    // separated. Confirms list semantics (a `meeting.detected`
    // envelope matches the first glob, even though it doesn't match
    // the second).
    let stub = Arc::new(StubOrchestrator::new());
    let bus: SessionEventBus = stub.event_bus();
    let app = build_app(test_state(stub.clone()));

    let req = Request::get("/v1/events?topics=meeting.*,doctor.warning")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    wait_for_subscriber(&bus).await;

    let envelope = sample_envelope();
    bus.publish(envelope);
    drop(bus);
    drop(stub);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body_to_string(response).await;
    assert!(
        body.contains("event: meeting.detected"),
        "first-glob match dropped under multi-glob filter: {body}"
    );
}

#[tokio::test]
async fn events_returns_410_when_resume_window_exceeded() {
    let cache = Arc::new(WindowExceededCache);
    let orch = Arc::new(WithCache::new(cache));
    let app = build_app(test_state(orch));

    let resume_from = EventId::now_v7();
    let res = app
        .oneshot(
            Request::get(format!("/v1/events?since_event_id={resume_from}"))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::GONE);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_REPLAY_WINDOW_EXCEEDED");
}

#[tokio::test]
async fn unprefixed_path_returns_404_after_v1_nest() {
    // The router lives under /v1; bare /health predates the
    // post-review fix and a regression that drops the nest would
    // silently route to bare /health again. Pin the new shape.
    //
    // We send a valid bearer so we're testing the router's
    // not-found, not the auth middleware's not-`/v1/health`
    // 401-by-default — those are different signals and bundling
    // them confuses the regression check.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/health")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn origin_pre_empts_auth_on_protected_endpoint() {
    // Layer order regression check: `Origin` denial must run BEFORE
    // bearer auth. Pre-fix, axum's last-added-is-outermost made
    // `require_bearer` the outer layer, so a hostile-origin + no-
    // bearer request returned 401 (revealing that the endpoint
    // requires creds) instead of the 403 the OpenAPI mandates.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/v1/meetings")
                .header(header::ORIGIN, "http://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_ORIGIN_DENIED");
}

#[tokio::test]
async fn bearer_scheme_is_case_insensitive() {
    // RFC 7235 §2.1: auth-scheme is case-insensitive. Pre-fix the
    // strict `strip_prefix("Bearer ")` rejected `bearer xyz`.
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/v1/meetings")
                .header(header::AUTHORIZATION, format!("bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn events_last_event_id_header_wins_over_query() {
    // Spec contract: `Last-Event-ID` header beats `?since_event_id`
    // when both are present. The cache below records which `since`
    // it was asked about so the test asserts the header (not the
    // query) reached `replay_since`.
    let header_marker = EventId::now_v7();
    let query_marker = EventId::now_v7();
    assert_ne!(header_marker, query_marker);

    let cache = Arc::new(RecordingCache::default());
    let orch = Arc::new(WithCache::new(cache.clone()));
    let app = build_app(test_state(orch));

    let res = app
        .oneshot(
            Request::get(format!("/v1/events?since_event_id={query_marker}"))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .header("last-event-id", header_marker.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The handler returns 200 with an SSE body; the body itself
    // doesn't matter here, only that we hit the replay path.
    assert_eq!(res.status(), StatusCode::OK);
    drop(res);
    let observed = *cache.last_seen.lock().await;
    assert_eq!(
        observed,
        Some(header_marker),
        "replay_since received the wrong resume marker — Last-Event-ID should win"
    );
}

#[tokio::test]
async fn events_rejects_malformed_resume_marker() {
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/v1/events?since_event_id=garbage")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_VALIDATION");
}

// ── helpers ───────────────────────────────────────────────────────────

fn sample_envelope() -> Envelope<EventPayload> {
    let meeting = Meeting {
        id: MeetingId::now_v7(),
        status: heron_session::MeetingStatus::Detected,
        platform: heron_session::Platform::Zoom,
        title: Some("Standup".to_owned()),
        calendar_event_id: None,
        started_at: Utc::now(),
        ended_at: None,
        duration_secs: None,
        participants: vec![],
        transcript_status: heron_session::TranscriptLifecycle::Pending,
        summary_status: heron_session::SummaryLifecycle::Pending,
        tags: vec![],
        processing: None,
        action_items: vec![],
    };
    Envelope::new(EventPayload::MeetingDetected(meeting.clone()))
        .with_meeting(meeting.id.to_string())
}

/// Poll the broadcast bus until the SSE handler has registered its
/// subscriber. Replaces a fixed-duration sleep so the test is
/// deterministic — we wait exactly as long as needed and no longer.
/// Falls back to a generous total budget so a hung handler fails
/// the test instead of hanging the test runner.
async fn wait_for_subscriber(bus: &SessionEventBus) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while bus.subscriber_count() == 0 {
        if std::time::Instant::now() >= deadline {
            panic!("SSE handler never registered a subscriber within 5s");
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let body = collect_body_to_string(res).await;
    serde_json::from_str(&body).expect("body is JSON")
}

async fn collect_body_to_string(res: axum::response::Response) -> String {
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

// ── test-only orchestrator wrappers ───────────────────────────────────

/// Decorates [`StubOrchestrator`] with a real [`ReplayCache`].
struct WithCache {
    inner: StubOrchestrator,
    cache: Arc<dyn ReplayCache<EventPayload>>,
}

impl WithCache {
    fn new(cache: Arc<dyn ReplayCache<EventPayload>>) -> Self {
        Self {
            inner: StubOrchestrator::new(),
            cache,
        }
    }
}

#[async_trait]
impl SessionOrchestrator for WithCache {
    async fn list_meetings(&self, q: ListMeetingsQuery) -> Result<ListMeetingsPage, SessionError> {
        self.inner.list_meetings(q).await
    }
    async fn get_meeting(&self, id: &MeetingId) -> Result<Meeting, SessionError> {
        self.inner.get_meeting(id).await
    }
    async fn start_capture(&self, args: StartCaptureArgs) -> Result<Meeting, SessionError> {
        self.inner.start_capture(args).await
    }
    async fn end_meeting(&self, id: &MeetingId) -> Result<(), SessionError> {
        self.inner.end_meeting(id).await
    }
    async fn read_transcript(&self, id: &MeetingId) -> Result<Transcript, SessionError> {
        self.inner.read_transcript(id).await
    }
    async fn read_summary(&self, id: &MeetingId) -> Result<Option<Summary>, SessionError> {
        self.inner.read_summary(id).await
    }
    async fn audio_path(&self, id: &MeetingId) -> Result<PathBuf, SessionError> {
        self.inner.audio_path(id).await
    }
    async fn list_upcoming_calendar(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: Option<u32>,
    ) -> Result<Vec<CalendarEvent>, SessionError> {
        self.inner.list_upcoming_calendar(from, to, limit).await
    }
    async fn attach_context(&self, req: PreMeetingContextRequest) -> Result<(), SessionError> {
        self.inner.attach_context(req).await
    }
    async fn prepare_context(&self, req: PrepareContextRequest) -> Result<(), SessionError> {
        self.inner.prepare_context(req).await
    }
    async fn health(&self) -> Health {
        self.inner.health().await
    }
    fn event_bus(&self) -> SessionEventBus {
        self.inner.event_bus()
    }
    fn replay_cache(&self) -> Option<&dyn ReplayCache<EventPayload>> {
        Some(&*self.cache)
    }
}

/// Cache that ignores the `since` marker and replays a fixed list
/// once. Sufficient to verify the wiring; the real cache lands when
/// the orchestrator consolidation does.
struct InMemoryCache {
    items: Mutex<Vec<Envelope<EventPayload>>>,
}
impl InMemoryCache {
    fn new(items: Vec<Envelope<EventPayload>>) -> Self {
        Self {
            items: Mutex::new(items),
        }
    }
}

#[async_trait]
impl ReplayCache<EventPayload> for InMemoryCache {
    async fn replay_since(
        &self,
        _since: EventId,
    ) -> Result<Vec<Envelope<EventPayload>>, ReplayError> {
        Ok(self.items.lock().await.clone())
    }
    fn window(&self) -> Duration {
        Duration::from_secs(3600)
    }
}

/// Cache that records which `since` marker `replay_since` was
/// invoked with, then returns an empty replay. Used to pin the
/// Last-Event-ID-vs-query precedence contract.
#[derive(Default)]
struct RecordingCache {
    last_seen: Mutex<Option<EventId>>,
}

#[async_trait]
impl ReplayCache<EventPayload> for RecordingCache {
    async fn replay_since(
        &self,
        since: EventId,
    ) -> Result<Vec<Envelope<EventPayload>>, ReplayError> {
        *self.last_seen.lock().await = Some(since);
        Ok(Vec::new())
    }
}

/// Cache that always reports the resume marker is too old.
struct WindowExceededCache;
#[async_trait]
impl ReplayCache<EventPayload> for WindowExceededCache {
    async fn replay_since(
        &self,
        since: EventId,
    ) -> Result<Vec<Envelope<EventPayload>>, ReplayError> {
        Err(ReplayError::WindowExceeded {
            requested: since,
            window_secs: 3600,
        })
    }
}
