//! Wiremock-driven tests for [`heron_cli::daemon::DaemonClient`].
//!
//! No real `herond` required — every test stands up its own
//! `MockServer`, configures matching expectations, and drives the
//! client against the mock URI. Mirrors the structure
//! [`heron_bot::recall::tests`] uses for its Recall integration
//! tests so the two layers stay readable side-by-side.
//!
//! The tests are placed under `tests/` rather than as `#[cfg(test)]`
//! inline because the heron-cli lib-test binary on this machine has
//! a known dyld issue loading `libonnxruntime.1.17.1.dylib` (see the
//! repo CLAUDE.md "Known local exception"). Integration tests are
//! standalone bin targets that compile + link independently, which
//! keeps these network-only tests off the lib-test critical path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use heron_cli::daemon::{ClientConfig, DaemonClient, DaemonError};
use heron_session::{
    ComponentState, Health, ListMeetingsPage, Meeting, MeetingId, MeetingStatus, Platform,
    StartCaptureArgs, SummaryLifecycle, TranscriptLifecycle,
};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_BEARER: &str = "test-bearer-abcdef-1234";

fn client_for(server: &MockServer) -> DaemonClient {
    let config = ClientConfig {
        bearer: TEST_BEARER.into(),
        // wiremock binds at `http://127.0.0.1:<port>` — the daemon's
        // canonical `/v1` prefix is included so URL-construction
        // semantics match the production base.
        base_url: format!("{}/v1", server.uri()),
        timeout: Duration::from_secs(5),
    };
    DaemonClient::new(config).expect("build client")
}

fn sample_meeting(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "status": "recording",
        "platform": "zoom",
        "title": null,
        "calendar_event_id": null,
        "started_at": "2026-04-26T00:00:00Z",
        "ended_at": null,
        "duration_secs": null,
        "participants": [],
        "transcript_status": "pending",
        "summary_status": "pending",
    })
}

#[tokio::test]
async fn health_returns_status_envelope() {
    let server = MockServer::start().await;
    let component = json!({
        "state": "ok",
        "message": null,
        "last_check": null,
    });
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "degraded",
            "version": null,
            "components": {
                "capture": {
                    "state": "permission_missing",
                    "message": null,
                    "last_check": null,
                },
                "whisperkit": component.clone(),
                "vault": component.clone(),
                "eventkit": component.clone(),
                "llm": component,
            },
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let h: Health = client.health().await.expect("health");
    assert_eq!(
        h.components.capture.state,
        ComponentState::PermissionMissing
    );
}

#[tokio::test]
async fn start_capture_sends_bearer_and_decodes_meeting() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/meetings"))
        .and(header("authorization", &*format!("Bearer {TEST_BEARER}")))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(sample_meeting("mtg_018f9c00-0000-7000-8000-000000000001")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server);
    let m: Meeting = client
        .start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: Some("Daily standup".into()),
            calendar_event_id: None,
        })
        .await
        .expect("start_capture");
    assert_eq!(m.platform, Platform::Zoom);
    assert_eq!(m.status, MeetingStatus::Recording);
    assert!(matches!(m.transcript_status, TranscriptLifecycle::Pending));
    assert!(matches!(m.summary_status, SummaryLifecycle::Pending));
}

#[tokio::test]
async fn missing_bearer_yields_typed_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/meetings"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "success": false,
            "error": "Unauthorized",
            "code": "HERON_E_UNAUTHORIZED",
            "message": "bearer token missing or invalid",
            "statusCode": 401,
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = client
        .start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: None,
            calendar_event_id: None,
        })
        .await
        .expect_err("should fail");
    match err {
        DaemonError::Api { status, code, .. } => {
            assert_eq!(status, 401);
            assert_eq!(code, "HERON_E_UNAUTHORIZED");
        }
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn end_meeting_returns_unit_on_204() {
    let server = MockServer::start().await;
    let id = "mtg_018f9c00-0000-7000-8000-000000000123";
    Mock::given(method("POST"))
        .and(path(format!("/v1/meetings/{id}/end")))
        .and(header("authorization", &*format!("Bearer {TEST_BEARER}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let mid: MeetingId = id.parse().unwrap();
    client.end_meeting(&mid).await.expect("end_meeting");
}

#[tokio::test]
async fn end_meeting_invalid_state_propagates_envelope() {
    let server = MockServer::start().await;
    let id = "mtg_018f9c00-0000-7000-8000-000000000456";
    Mock::given(method("POST"))
        .and(path(format!("/v1/meetings/{id}/end")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "success": false,
            "error": "InvalidState",
            "code": "HERON_E_INVALID_STATE",
            "message": "invalid state transition",
            "statusCode": 409,
            "details": { "current_state": "done" },
        })))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let mid: MeetingId = id.parse().unwrap();
    let err = client.end_meeting(&mid).await.expect_err("should fail");
    match err {
        DaemonError::Api {
            status,
            code,
            details,
            ..
        } => {
            assert_eq!(status, 409);
            assert_eq!(code, "HERON_E_INVALID_STATE");
            assert_eq!(details["current_state"], "done");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn list_meetings_forwards_platform_and_limit_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/meetings"))
        .and(query_param("platform", "zoom"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [],
            "next_cursor": null,
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let page: ListMeetingsPage = client
        .list_meetings(Some(Platform::Zoom), Some(5))
        .await
        .expect("list");
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn get_meeting_decodes_envelope() {
    let server = MockServer::start().await;
    let id = "mtg_018f9c00-0000-7000-8000-000000000777";
    Mock::given(method("GET"))
        .and(path(format!("/v1/meetings/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_meeting(id)))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let mid: MeetingId = id.parse().unwrap();
    let m = client.get_meeting(&mid).await.expect("get_meeting");
    assert_eq!(m.id.to_string(), id);
}

#[tokio::test]
async fn unreachable_daemon_yields_typed_error() {
    // Point the client at an address that is virtually guaranteed
    // not to accept TCP — `127.0.0.1:1` (privileged, not listening
    // for any sane test process) gives reqwest a fast
    // connection-refused, which we map to
    // `DaemonError::Unreachable` — the actionable "is herond
    // running?" path the CLI prints.
    let client = DaemonClient::new(ClientConfig {
        bearer: TEST_BEARER.into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        timeout: Duration::from_millis(500),
    })
    .expect("build client");
    let err = client.health().await.expect_err("should fail");
    match err {
        DaemonError::Unreachable { .. } => {}
        // Some platforms surface a generic transport error rather
        // than a connect-classified one; both indicate "daemon not
        // running" and both lead the CLI to the same actionable
        // message.
        DaemonError::Http(_) => {}
        other => panic!("expected Unreachable/Http, got {other:?}"),
    }
}

#[tokio::test]
async fn events_stream_yields_typed_envelopes() {
    // SSE body. Three frames: one heartbeat (comment, ignored), one
    // typed event, then end-of-stream. The wire shape mirrors what
    // herond's `/v1/events` SSE handler emits.
    let body = "\
:heartbeat\n\n\
id: evt_018f9c00-0000-7000-8000-0000000000aa\n\
event: meeting.detected\n\
data: {\"event_id\":\"evt_018f9c00-0000-7000-8000-0000000000aa\",\"api_version\":\"2026-04-25\",\"created_at\":\"2026-04-26T00:00:00Z\",\"meeting_id\":null,\"event_type\":\"meeting.detected\",\"data\":{\"id\":\"mtg_018f9c00-0000-7000-8000-000000000001\",\"status\":\"detected\",\"platform\":\"zoom\",\"title\":null,\"calendar_event_id\":null,\"started_at\":\"2026-04-26T00:00:00Z\",\"ended_at\":null,\"duration_secs\":null,\"participants\":[],\"transcript_status\":\"pending\",\"summary_status\":\"pending\"}}\n\n\
";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/events"))
        .and(header("accept", "text/event-stream"))
        .and(header("authorization", &*format!("Bearer {TEST_BEARER}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let client = client_for(&server);
    let mut stream = client.events(None).await.expect("events");
    let evt = stream.next().await.expect("first event").expect("decoded");
    assert_eq!(evt.payload.event_type(), "meeting.detected");
    // Stream ends after the body is exhausted.
    assert!(stream.next().await.is_none(), "stream should close");
}
