//! Integration coverage for `heron record --daemon` (v2 delegation).
//!
//! The driver's contract:
//!
//! 1. Subscribe to `/v1/events` BEFORE `POST /v1/meetings` so no
//!    lifecycle envelopes for the new meeting are missed.
//! 2. `POST /v1/meetings` with the configured `StartCaptureArgs`.
//! 3. Stream filtered envelopes (matching `meeting_id`) until either
//!    `meeting.completed` arrives, the user-supplied `stop` future
//!    fires (in which case `POST /v1/meetings/{id}/end` is sent and
//!    the loop continues until `meeting.completed`), or the stream
//!    closes early (typed error).
//!
//! Each test stands up its own `MockServer` so they can run in
//! parallel without coordination. Mirrors the structure of
//! `tests/daemon_client.rs`. Lives under `tests/` (not inline
//! `#[cfg(test)]`) for the same dyld reason called out in the
//! daemon_client integration suite.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::pending;
use std::time::Duration;

use heron_cli::daemon::{ClientConfig, DaemonClient};
use heron_cli::record_delegate::{
    DelegateConfig, DelegateError, StopReason, drive_delegated_session,
};
use heron_session::{Platform, StartCaptureArgs};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_BEARER: &str = "test-bearer-record-delegate";
const MEETING_ID: &str = "mtg_018f9c00-0000-7000-8000-0000000000ab";
const OTHER_MEETING_ID: &str = "mtg_018f9c00-0000-7000-8000-00000000ffff";

fn client_for(server: &MockServer) -> DaemonClient {
    DaemonClient::new(ClientConfig {
        bearer: TEST_BEARER.into(),
        base_url: format!("{}/v1", server.uri()),
        // Generous test timeout so a slow CI runner doesn't false-
        // positive on the (synchronous) start_capture POST.
        timeout: Duration::from_secs(5),
    })
    .expect("build client")
}

fn meeting_envelope(status: &str) -> serde_json::Value {
    json!({
        "id": MEETING_ID,
        "status": status,
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

/// SSE body covering a full session lifecycle plus *strong*
/// cross-talk: the fixture interleaves one `meeting.started` AND one
/// `meeting.completed` for a *different* meeting between our
/// envelopes. A broken `meeting_id` filter would early-return on the
/// other meeting's `meeting.completed` (terminal event), so the
/// happy-path assertion that `outcome.meeting_id == MEETING_ID`
/// directly exercises the filter at `record_delegate.rs:env.meeting_id`.
/// The four canonical lifecycle envelopes for our meeting
/// (`detected → started → ended → completed`) follow. The `event_id`
/// strings are arbitrary v7-shaped UUIDs; the daemon never reuses an
/// id, so no sequencing constraints beyond ordering matter for this
/// fixture.
fn sse_body() -> String {
    fn frame(event_id: &str, kind: &str, data: &serde_json::Value) -> String {
        // SSE wire format. The `data:` line carries the full envelope
        // (framing fields + flattened payload), per the daemon's
        // projection. Two terminating newlines mark the end of the
        // frame.
        format!(
            "id: {event_id}\nevent: {kind}\ndata: {data}\n\n",
            data = serde_json::to_string(data).unwrap(),
        )
    }
    let detected = json!({
        "event_id": "evt_018f9c00-0000-7000-8000-0000000000a1",
        "api_version": "2026-04-25",
        "created_at": "2026-04-26T00:00:00Z",
        "meeting_id": MEETING_ID,
        "event_type": "meeting.detected",
        "data": meeting_envelope("detected"),
    });
    let started = json!({
        "event_id": "evt_018f9c00-0000-7000-8000-0000000000a2",
        "api_version": "2026-04-25",
        "created_at": "2026-04-26T00:00:01Z",
        "meeting_id": MEETING_ID,
        "event_type": "meeting.started",
        "data": meeting_envelope("recording"),
    });
    // Cross-talk: a different meeting's envelopes on the same bus.
    // The driver must skip these without flinching. Includes a
    // *terminal* `meeting.completed` for the other meeting so a
    // broken filter would surface as the wrong final outcome.
    let cross_talk_started = json!({
        "event_id": "evt_018f9c00-0000-7000-8000-0000000000b1",
        "api_version": "2026-04-25",
        "created_at": "2026-04-26T00:00:01.5Z",
        "meeting_id": OTHER_MEETING_ID,
        "event_type": "meeting.started",
        "data": {
            "id": OTHER_MEETING_ID,
            "status": "recording",
            "platform": "zoom",
            "title": null,
            "calendar_event_id": null,
            "started_at": "2026-04-26T00:00:01Z",
            "ended_at": null,
            "duration_secs": null,
            "participants": [],
            "transcript_status": "pending",
            "summary_status": "pending",
        },
    });
    let cross_talk_completed = json!({
        "event_id": "evt_018f9c00-0000-7000-8000-0000000000b2",
        "api_version": "2026-04-25",
        "created_at": "2026-04-26T00:00:01.7Z",
        "meeting_id": OTHER_MEETING_ID,
        "event_type": "meeting.completed",
        "data": {
            "meeting": {
                "id": OTHER_MEETING_ID,
                "status": "done",
                "platform": "zoom",
                "title": null,
                "calendar_event_id": null,
                "started_at": "2026-04-26T00:00:01Z",
                "ended_at": "2026-04-26T00:00:01.5Z",
                "duration_secs": 1,
                "participants": [],
                "transcript_status": "pending",
                "summary_status": "pending",
            },
            "outcome": "aborted",
            "failure_reason": "cross-talk fixture; this should be filtered",
        },
    });
    let ended = json!({
        "event_id": "evt_018f9c00-0000-7000-8000-0000000000a3",
        "api_version": "2026-04-25",
        "created_at": "2026-04-26T00:00:02Z",
        "meeting_id": MEETING_ID,
        "event_type": "meeting.ended",
        "data": meeting_envelope("ended"),
    });
    let completed = json!({
        "event_id": "evt_018f9c00-0000-7000-8000-0000000000a4",
        "api_version": "2026-04-25",
        "created_at": "2026-04-26T00:00:03Z",
        "meeting_id": MEETING_ID,
        "event_type": "meeting.completed",
        "data": {
            "meeting": meeting_envelope("done"),
            "outcome": "success",
            "failure_reason": null,
        },
    });
    let mut s = String::new();
    s.push_str(":heartbeat\n\n");
    s.push_str(&frame("evt_a1", "meeting.detected", &detected));
    s.push_str(&frame("evt_a2", "meeting.started", &started));
    s.push_str(&frame("evt_b1", "meeting.started", &cross_talk_started));
    // Terminal cross-talk envelope: a broken filter would early-
    // return here with the wrong meeting_id and outcome.
    s.push_str(&frame("evt_b2", "meeting.completed", &cross_talk_completed));
    s.push_str(&frame("evt_a3", "meeting.ended", &ended));
    s.push_str(&frame("evt_a4", "meeting.completed", &completed));
    s
}

async fn mount_happy_path(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/meetings"))
        .and(header("authorization", &*format!("Bearer {TEST_BEARER}")))
        .respond_with(ResponseTemplate::new(202).set_body_json(meeting_envelope("recording")))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/events"))
        .and(header("accept", "text/event-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn delegated_session_returns_completed_outcome_on_success() {
    let server = MockServer::start().await;
    mount_happy_path(&server).await;
    // The fixture's SSE body terminates with `meeting.completed` for
    // OUR meeting (and a deliberately-misleading `meeting.completed`
    // for a *different* meeting earlier in the stream — see
    // `cross_talk_completed` in `sse_body`). The stop arm should
    // never fire; pass a future that never resolves so the test
    // exercises the daemon-driven termination path.
    let outcome = drive_delegated_session(
        &client_for(&server),
        DelegateConfig {
            start: StartCaptureArgs {
                platform: Platform::Zoom,
                hint: Some("Standup".into()),
                calendar_event_id: None,
            },
            duration_cap: None,
        },
        pending::<StopReason>(),
    )
    .await
    .expect("delegated session");
    // If the meeting_id filter is broken, the driver returns the
    // cross-talk meeting's outcome (Aborted) rather than ours
    // (Success). Both assertions together pin the filter.
    assert_eq!(outcome.meeting_id.to_string(), MEETING_ID);
    assert!(matches!(
        outcome.completed.outcome,
        heron_session::MeetingOutcome::Success
    ));
}

#[tokio::test]
async fn delegated_session_sends_end_when_stop_signal_fires() {
    let server = MockServer::start().await;
    // Same SSE script as the happy path, but here we *explicitly*
    // assert `POST /v1/meetings/{id}/end` is hit exactly once,
    // because the stop future resolves immediately. The fixture
    // still ends in `meeting.completed`, so the driver loop
    // terminates cleanly.
    Mock::given(method("POST"))
        .and(path("/v1/meetings"))
        .respond_with(ResponseTemplate::new(202).set_body_json(meeting_envelope("recording")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/meetings/{MEETING_ID}/end")))
        .and(header("authorization", &*format!("Bearer {TEST_BEARER}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/events"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let outcome = drive_delegated_session(
        &client_for(&server),
        DelegateConfig {
            start: StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            },
            duration_cap: None,
        },
        // Resolves immediately on first poll → triggers the
        // `end_meeting` POST that the mock above asserts on.
        async { StopReason::UserSignal },
    )
    .await
    .expect("delegated session");
    assert_eq!(outcome.meeting_id.to_string(), MEETING_ID);
}

#[tokio::test]
async fn delegated_session_surfaces_typed_error_when_stream_closes_early() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/meetings"))
        .respond_with(ResponseTemplate::new(202).set_body_json(meeting_envelope("recording")))
        .mount(&server)
        .await;
    // SSE body cuts off after `meeting.started`; no `meeting.ended`,
    // no `meeting.completed`. The driver should surface
    // `DelegateError::StreamClosed` with `last_seen` carrying the
    // last filtered envelope kind for triage.
    let truncated = format!(
        "id: evt_a1\nevent: meeting.started\ndata: {}\n\n",
        json!({
            "event_id": "evt_018f9c00-0000-7000-8000-0000000000a1",
            "api_version": "2026-04-25",
            "created_at": "2026-04-26T00:00:00Z",
            "meeting_id": MEETING_ID,
            "event_type": "meeting.started",
            "data": meeting_envelope("recording"),
        })
    );
    Mock::given(method("GET"))
        .and(path("/v1/events"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(truncated),
        )
        .mount(&server)
        .await;

    let err = drive_delegated_session(
        &client_for(&server),
        DelegateConfig {
            start: StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            },
            duration_cap: None,
        },
        pending::<StopReason>(),
    )
    .await
    .expect_err("should fail");
    match err {
        DelegateError::StreamClosed { last_seen } => {
            assert_eq!(last_seen, Some("meeting.started"));
        }
        other => panic!("expected StreamClosed, got {other:?}"),
    }
}

#[tokio::test]
async fn delegated_session_propagates_start_capture_failure() {
    let server = MockServer::start().await;
    // The events endpoint is mounted (driver opens it before POST)
    // but never expected to be drained, since `start_capture` errors
    // first.
    Mock::given(method("GET"))
        .and(path("/v1/events"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(""),
        )
        .mount(&server)
        .await;
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

    let err = drive_delegated_session(
        &client_for(&server),
        DelegateConfig {
            start: StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            },
            duration_cap: None,
        },
        pending::<StopReason>(),
    )
    .await
    .expect_err("should fail");
    match err {
        DelegateError::Daemon(heron_cli::daemon::DaemonError::Api { status, code, .. }) => {
            assert_eq!(status, 401);
            assert_eq!(code, "HERON_E_UNAUTHORIZED");
        }
        other => panic!("expected Daemon(Api{{401}}), got {other:?}"),
    }
}
