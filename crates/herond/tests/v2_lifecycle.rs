//! Cross-crate lifecycle tests: `herond` HTTP layer ↔
//! `heron-orchestrator::LocalSessionOrchestrator` ↔ `heron-event` bus
//! ↔ `heron-event-http` replay cache.
//!
//! The existing `tests/api.rs` suite drives the HTTP router against a
//! `StubOrchestrator` whose every method returns `NotYetImplemented`.
//! These tests instead wire the *real* in-process orchestrator behind
//! the daemon so a regression in the wiring between
//! `routes::meetings` → `SessionOrchestrator` → bus → SSE projection
//! is caught — not just the daemon's stub-handler shape.
//!
//! Per `docs/codebase-gaps.md` item #10, seams #1 and #2:
//! - `POST /meetings` starts a real session owner and publishes the
//!   expected `meeting.detected | armed | started` envelopes.
//! - `POST /meetings/{id}/end` drives the FSM through `ended →
//!   completed` and the events show up on the bus.
//!
//! The transcript / summary / audio persistence half of seam #2 is
//! NOT yet implementable: the orchestrator's `end_meeting` runs the
//! FSM through `transcribing → summarizing → idle` synchronously
//! because no real STT / LLM / vault writer is wired. The
//! `#[ignore]`-marked test below pins the gap so the seam wakes up
//! when the production wiring lands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use heron_orchestrator::LocalSessionOrchestrator;
use heron_session::{EventPayload, MeetingId, MeetingOutcome, MeetingStatus, SessionOrchestrator};
use herond::{AppState, AuthConfig, build_app};
use tower::ServiceExt;

const TEST_BEARER: &str = "test-bearer-abcdef";

fn live_state() -> (AppState, Arc<LocalSessionOrchestrator>) {
    let orch = Arc::new(LocalSessionOrchestrator::new());
    let state = AppState {
        orchestrator: orch.clone(),
        auth: Arc::new(AuthConfig {
            bearer: TEST_BEARER.to_owned(),
        }),
        metrics: heron_metrics::init_prometheus_recorder()
            .expect("install Prometheus recorder for test state"),
    };
    (state, orch)
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.expect("body");
    serde_json::from_slice(&bytes).expect("body json")
}

/// Drain everything currently buffered for `rx`. The orchestrator's
/// `start_capture` / `end_meeting` publish synchronously, so once the
/// HTTP request that triggered them resolves, every envelope is
/// already queued in `rx`'s broadcast slot — a single non-blocking
/// drain suffices.
fn drain_payloads(
    rx: &mut tokio::sync::broadcast::Receiver<heron_event::Envelope<EventPayload>>,
) -> Vec<EventPayload> {
    let mut out = Vec::new();
    while let Ok(env) = rx.try_recv() {
        out.push(env.payload);
    }
    out
}

fn drain_payload_kinds(
    rx: &mut tokio::sync::broadcast::Receiver<heron_event::Envelope<EventPayload>>,
) -> Vec<String> {
    drain_payloads(rx)
        .into_iter()
        .map(|p| p.event_type().to_owned())
        .collect()
}

#[tokio::test]
async fn post_meetings_with_live_orchestrator_publishes_lifecycle_events() {
    let (state, orch) = live_state();
    let app = build_app(state);
    let mut bus_rx = orch.event_bus().subscribe();

    let res = app
        .oneshot(
            Request::post("/v1/meetings")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"platform":"zoom","hint":"Standup"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let location = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("Location header set on 202");
    assert!(
        location.starts_with("/v1/meetings/mtg_"),
        "unexpected Location: {location}",
    );
    let body = body_json(res).await;
    assert_eq!(body["status"], "recording");
    assert_eq!(body["platform"], "zoom");
    assert_eq!(body["title"], "Standup");

    let kinds = drain_payload_kinds(&mut bus_rx);
    assert_eq!(
        kinds,
        ["meeting.detected", "meeting.armed", "meeting.started"],
        "live orchestrator must publish three lifecycle events",
    );

    orch.shutdown().await.expect("shutdown clean");
}

#[tokio::test]
async fn post_meetings_end_with_live_orchestrator_publishes_terminal_events() {
    let (state, orch) = live_state();
    let mut bus_rx = orch.event_bus().subscribe();

    let started = orch
        .start_capture(heron_session::StartCaptureArgs {
            platform: heron_session::Platform::Zoom,
            hint: None,
            calendar_event_id: None,
        })
        .await
        .expect("start_capture");
    // Drop start_capture's lifecycle envelopes; we're scoping the
    // assertions below to end_meeting's emissions only.
    let _ = drain_payloads(&mut bus_rx);

    let res = build_app(state)
        .oneshot(
            Request::post(format!("/v1/meetings/{}/end", started.id))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let payloads = drain_payloads(&mut bus_rx);
    assert_eq!(payloads.len(), 2, "expected ended + completed");
    assert!(matches!(payloads[0], EventPayload::MeetingEnded(_)));
    match &payloads[1] {
        EventPayload::MeetingCompleted(data) => {
            assert!(matches!(data.outcome, MeetingOutcome::Success));
            assert!(matches!(data.meeting.status, MeetingStatus::Done));
        }
        other => panic!("expected MeetingCompleted, got {}", other.event_type()),
    }

    orch.shutdown().await.expect("shutdown clean");
}

#[tokio::test]
async fn post_meetings_end_unknown_id_returns_404_through_router() {
    // Confirms the daemon's HTTP error projection actually surfaces
    // the orchestrator's typed error: a `SessionError::NotFound` from
    // `end_meeting` must land as `404` + `HERON_E_NOT_FOUND`. With the
    // stub this can't be tested because the stub returns
    // `NotYetImplemented` (501) regardless of meeting id. Live wiring
    // distinguishes the two.
    let (state, orch) = live_state();
    let res = build_app(state)
        .oneshot(
            Request::post(format!("/v1/meetings/{}/end", MeetingId::now_v7()))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = body_json(res).await;
    assert_eq!(body["code"], "HERON_E_NOT_FOUND");

    orch.shutdown().await.expect("shutdown clean");
}

#[tokio::test]
#[ignore = "TODO: requires production wiring of `RecallDriver` (or any `MeetingBotDriver`) into `LocalSessionOrchestrator` (codebase-gaps.md items #1, #3) — orchestrator-driven shutdown can't be observed end-to-end until the orchestrator owns a driver. The unit-level guarantee (driver.shutdown() leaves no active bots) is already pinned in `heron-bot::recall::tests::shutdown_calls_leave_on_active_bots_and_drains_polling_tasks`"]
async fn graceful_shutdown_leaves_no_active_vendor_bot() {
    // When the orchestrator owns a `RecallDriver` (or peer driver),
    // this test should:
    //   1. Build a LocalSessionOrchestrator wired to a wiremock-backed
    //      RecallDriver and a mocked RealtimeBackend.
    //   2. POST /meetings; observe a vendor `POST /api/v1/bot/` on the
    //      mock server (proof a bot was actually created).
    //   3. Trigger `LocalSessionOrchestrator::shutdown()`.
    //   4. Assert wiremock recorded a `POST /leave_call/` (or `DELETE
    //      /bot/{id}/` for pre-meeting bots) for every created bot —
    //      i.e. no bot was orphaned on the vendor side. The
    //      `RecallDriver`-only equivalent already passes; this test
    //      pins the orchestrator-level contract.
}

#[tokio::test]
#[ignore = "TODO: requires production wiring of bot+bridge+realtime+policy into LocalSessionOrchestrator (codebase-gaps.md items #1, #3) — `end_meeting` does not yet drive a real STT/LLM/vault pipeline so transcript/summary/audio persistence cannot be asserted end-to-end"]
async fn end_meeting_persists_transcript_summary_audio_references() {
    // When the production session-owner composition lands, this test
    // should:
    //   1. POST /meetings with a deterministic platform/hint.
    //   2. Drive a scripted MockRealtimeBackend session through
    //      InputTranscriptDelta -> ResponseDone, with a fake vault
    //      writer attached to the orchestrator builder.
    //   3. POST /meetings/{id}/end and await meeting.completed on the
    //      bus.
    //   4. GET /meetings/{id}/transcript -> 200 with non-empty
    //      segments. GET /meetings/{id}/summary -> 200. GET
    //      /meetings/{id}/audio -> 200 with a non-zero Content-Length.
    //
    // Until the orchestrator owns those subsystems the read endpoints
    // either 501 (no vault configured) or NotFound (vault configured
    // but the active-meeting was not finalized to disk). Neither is a
    // useful integration assertion.
}
