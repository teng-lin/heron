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
    CalendarEvent, EventPayload, Health, ListMeetingsPage, ListMeetingsQuery, Meeting, MeetingId,
    PreMeetingContextRequest, SessionError, SessionEventBus, SessionOrchestrator, StartCaptureArgs,
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
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
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
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
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
        .oneshot(Request::get("/meetings").body(Body::empty()).unwrap())
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
            Request::get("/meetings")
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
            Request::get("/meetings")
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
            Request::get("/health")
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
async fn unimpl_endpoints_all_return_501() {
    let app = build_app(stub_state());
    let cases = [
        ("GET", "/meetings/mtg_x"),
        ("GET", "/meetings/mtg_x/transcript"),
        ("GET", "/meetings/mtg_x/summary"),
        ("GET", "/meetings/mtg_x/audio"),
        ("GET", "/calendar/upcoming"),
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
    // Silence unused-binding warning when the loop owns `app`.
    let _ = app;
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
    // publish — broadcast::Sender drops events with no live receivers.
    let req = Request::get("/events")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    // Yield enough times for the subscriber to register. Without
    // this the publish lands before the broadcast subscribe, and
    // the test flakes.
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;

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
    let req = Request::get(format!("/events?since_event_id={resume_from}"))
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Drop the orchestrator handle so the bus closes and SSE ends.
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
async fn events_returns_410_when_resume_window_exceeded() {
    let cache = Arc::new(WindowExceededCache);
    let orch = Arc::new(WithCache::new(cache));
    let app = build_app(test_state(orch));

    let resume_from = EventId::now_v7();
    let res = app
        .oneshot(
            Request::get(format!("/events?since_event_id={resume_from}"))
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
async fn events_rejects_malformed_resume_marker() {
    let app = build_app(stub_state());
    let res = app
        .oneshot(
            Request::get("/events?since_event_id=garbage")
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
    };
    Envelope::new(EventPayload::MeetingDetected(meeting.clone()))
        .with_meeting(meeting.id.to_string())
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
