//! Wiremock-driven integration tests for [`super::RecallDriver`].
//!
//! No live `RECALL_API_KEY` required — every test stands up its own
//! `MockServer` and points the driver at it. The polling interval is
//! shrunk to 50ms so end-to-end flows finish in well under a second.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::client::{Client, ClientConfig};
use super::*;
use crate::{
    BotCreateArgs, BotError, BotState, DisclosureProfile, EjectReason, MeetingBotDriver, PersonaId,
    PreMeetingContext,
};

/// Short interval so each test runs sub-second.
const TEST_POLL: Duration = Duration::from_millis(50);

fn driver_for(server: &MockServer) -> RecallDriver {
    let cfg = ClientConfig {
        api_key: "test-token".into(),
        base_url: server.uri(),
        timeout: Duration::from_secs(5),
    };
    let client = Client::new(cfg).expect("build client");
    RecallDriver::from_client(client, TEST_POLL, "heron-test".into())
}

fn args(meeting_url: &str) -> BotCreateArgs {
    BotCreateArgs {
        meeting_url: meeting_url.into(),
        persona_id: PersonaId::now_v7(),
        disclosure: DisclosureProfile {
            text_template: "Hi, I'm an AI assistant joining for {meeting_title}.".into(),
            objection_patterns: vec!["please leave".into()],
            objection_timeout_secs: 10,
            re_announce_on_join: false,
        },
        context: PreMeetingContext::default(),
        metadata: json!({ "test_case": meeting_url }),
        idempotency_key: Uuid::now_v7(),
    }
}

fn create_response(vendor_id: &str, status_changes: Value) -> ResponseTemplate {
    ResponseTemplate::new(201).set_body_json(json!({
        "id": vendor_id,
        "status_changes": status_changes,
    }))
}

/// Convenience: build a `status_changes` array from `(code, sub_code)`
/// pairs. Caller doesn't pass timestamps — wiremock body is opaque
/// to the driver.
fn changes(items: &[(&str, Option<&str>)]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|(code, sub)| {
                let mut o = serde_json::Map::new();
                o.insert("code".into(), Value::String((*code).to_string()));
                if let Some(s) = sub {
                    o.insert("sub_code".into(), Value::String((*s).to_string()));
                }
                o.insert(
                    "created_at".into(),
                    Value::String("2026-04-26T00:00:00Z".into()),
                );
                Value::Object(o)
            })
            .collect(),
    )
}

/// Wait until `predicate(state)` is `true` or the deadline elapses.
/// Polls with a tight loop because the driver's `poll_interval` is
/// already 50ms.
async fn wait_for_state<F>(driver: &RecallDriver, id: BotId, mut predicate: F) -> BotState
where
    F: FnMut(&BotState) -> bool,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(s) = driver.current_state(id)
            && predicate(&s)
        {
            return s;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timeout waiting for state predicate; current={:?}",
                driver.current_state(id)
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drain the receiver until the predicate matches or `max` events
/// pass. Returns every event observed so the test can assert on the
/// order of transitions.
async fn collect_until<F>(
    rx: &mut tokio::sync::broadcast::Receiver<crate::BotStateEvent>,
    max: usize,
    mut stop: F,
) -> Vec<BotState>
where
    F: FnMut(&BotState) -> bool,
{
    let mut out = Vec::new();
    for _ in 0..max {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(ev)) => {
                let s = ev.state.clone();
                out.push(ev.state);
                if stop(&s) {
                    return out;
                }
            }
            Ok(Err(e)) => {
                panic!("broadcast recv failed: {e}");
            }
            Err(_) => panic!(
                "timeout waiting for next event after {} events: {out:?}",
                out.len()
            ),
        }
    }
    out
}

// ── happy path ────────────────────────────────────────────────────────

#[tokio::test]
async fn bot_create_drives_through_joining_to_in_meeting() {
    let server = MockServer::start().await;

    // POST /bot/ → 201 with no status_changes yet (typical Recall
    // response — codes appear later via polling).
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_happy", changes(&[])))
        .mount(&server)
        .await;

    // GET /bot/{id}/ → progresses through joining_call →
    // in_call_recording. Wiremock takes the FIRST matching mock by
    // default, so register the most-progressed response last and
    // mount the earlier ones with `up_to_n_times` quotas.
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_happy/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_happy",
            "status_changes": changes(&[("joining_call", None)]),
        })))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_happy/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_happy",
            "status_changes": changes(&[
                ("joining_call", None),
                ("in_call_recording", None),
            ]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/123"))
        .await
        .expect("bot_create ok");
    let mut rx = driver.subscribe_state(bot_id);

    // Should observe Init→LoadingPersona→TtsWarming→Joining→
    // Disclosing→InMeeting (the last two come from the projected
    // JoinAccepted + synthetic DisclosureAcked).
    let observed = collect_until(&mut rx, 8, |s| matches!(s, BotState::InMeeting)).await;
    assert!(
        observed.iter().any(|s| matches!(s, BotState::Joining)),
        "missing Joining transition: {observed:?}",
    );
    assert!(
        observed.iter().any(|s| matches!(s, BotState::Disclosing)),
        "missing Disclosing transition: {observed:?}",
    );
    assert_eq!(observed.last(), Some(&BotState::InMeeting));

    let _ = driver.bot_leave(bot_id).await;
}

// ── invariant validation ─────────────────────────────────────────────

#[tokio::test]
async fn bot_create_rejects_empty_disclosure_with_no_disclosure_profile() {
    let server = MockServer::start().await;
    let driver = driver_for(&server);
    let mut a = args("https://zoom.us/j/empty");
    a.disclosure.text_template = "   \n\t  ".into();

    let err = driver.bot_create(a).await.expect_err("must reject");
    assert!(matches!(err, BotError::NoDisclosureProfile), "got: {err:?}",);
}

#[tokio::test]
async fn bot_create_rejects_nil_persona_id() {
    let server = MockServer::start().await;
    let driver = driver_for(&server);
    let mut a = args("https://zoom.us/j/no-persona");
    a.persona_id = PersonaId::nil();

    let err = driver.bot_create(a).await.expect_err("must reject");
    match err {
        BotError::Vendor(msg) => {
            assert!(msg.contains("persona_id"), "got: {msg}");
        }
        other => panic!("expected Vendor, got {other:?}"),
    }
}

#[tokio::test]
async fn second_bot_create_returns_already_active_while_first_is_alive() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_first", changes(&[])))
        .mount(&server)
        .await;
    // Polling endpoint stays empty so the first bot never reaches
    // terminal during the test.
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_first/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_first",
            "status_changes": changes(&[("joining_call", None)]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let first = driver
        .bot_create(args("https://zoom.us/j/A"))
        .await
        .expect("first ok");
    let err = driver
        .bot_create(args("https://zoom.us/j/B"))
        .await
        .expect_err("singleton must reject");
    match err {
        BotError::BotAlreadyActive { existing } => assert_eq!(existing, first),
        other => panic!("expected BotAlreadyActive, got {other:?}"),
    }
    let _ = driver.bot_leave(first).await;
}

#[tokio::test]
async fn second_bot_create_after_first_completes_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_terminal", changes(&[])))
        .mount(&server)
        .await;
    // Polling returns a `done` immediately so the first bot reaches
    // Completed within one poll cycle.
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_terminal/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_terminal",
            "status_changes": changes(&[("done", None)]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let first = driver
        .bot_create(args("https://zoom.us/j/done"))
        .await
        .expect("first ok");
    wait_for_state(&driver, first, |s| matches!(s, BotState::Completed)).await;

    // Re-target POST to a different vendor id for the second bot.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_second", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_second/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_second",
            "status_changes": changes(&[]),
        })))
        .mount(&server)
        .await;

    // Singleton means "active" not "ever-existed" — second create
    // after first reaches Completed should succeed.
    let second = driver
        .bot_create(args("https://zoom.us/j/again"))
        .await
        .expect("second ok after terminal");
    let _ = driver.bot_leave(second).await;
}

// ── leave / terminate ────────────────────────────────────────────────

#[tokio::test]
async fn bot_leave_is_idempotent_across_two_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_leave", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_leave/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_leave",
            "status_changes": changes(&[("joining_call", None)]),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/vbot_leave/leave_call/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/leave"))
        .await
        .expect("create ok");

    driver.bot_leave(bot_id).await.expect("first leave ok");
    // Second leave: idempotent — driver short-circuits when entry
    // is already terminal.
    driver.bot_leave(bot_id).await.expect("second leave ok");
}

#[tokio::test]
async fn bot_terminate_rejects_when_in_meeting_with_not_in_meeting() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_term", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_term/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_term",
            "status_changes": changes(&[
                ("joining_call", None),
                ("in_call_recording", None),
            ]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/term"))
        .await
        .expect("create ok");
    wait_for_state(&driver, bot_id, |s| matches!(s, BotState::InMeeting)).await;

    let err = driver
        .bot_terminate(bot_id)
        .await
        .expect_err("terminate from InMeeting must reject");
    match err {
        BotError::NotInMeeting { current_state } => {
            assert!(matches!(current_state, BotState::InMeeting));
        }
        other => panic!("expected NotInMeeting, got {other:?}"),
    }

    let _ = driver.bot_leave(bot_id).await;
}

// ── error mapping ────────────────────────────────────────────────────

#[tokio::test]
async fn rate_limited_response_maps_to_rate_limited_with_header_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "17")
                .set_body_json(json!({ "detail": "slow down" })),
        )
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let err = driver
        .bot_create(args("https://zoom.us/j/429"))
        .await
        .expect_err("must reject");
    match err {
        BotError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 17),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn capacity_exhausted_response_maps_to_capacity_exhausted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(
            ResponseTemplate::new(507)
                .insert_header("Retry-After", "120")
                .set_body_json(json!({ "detail": "warm pool empty" })),
        )
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let err = driver
        .bot_create(args("https://zoom.us/j/507"))
        .await
        .expect_err("must reject");
    match err {
        BotError::CapacityExhausted { retry_after_secs } => assert_eq!(retry_after_secs, 120),
        other => panic!("expected CapacityExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn network_failure_maps_to_network_error() {
    // Bind a TCP listener to discover an OS-assigned port, then
    // drop it. Subsequent connect attempts hit `ECONNREFUSED` (or
    // immediate RST) on a single OS but `MockServer::drop` is
    // unreliable across platforms — it teases the same port back
    // out for re-use, which on macOS sometimes accepts and 404s.
    // Standard-library `TcpListener` drop is deterministic.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    let dead_port_url = format!("http://127.0.0.1:{port}");

    let cfg = ClientConfig {
        api_key: "test-token".into(),
        base_url: dead_port_url,
        timeout: Duration::from_secs(2),
    };
    let client = Client::new(cfg).expect("client");
    let driver = RecallDriver::from_client(client, TEST_POLL, "heron-test".into());

    let err = driver
        .bot_create(args("https://zoom.us/j/dead"))
        .await
        .expect_err("dead port must error");
    assert!(
        matches!(err, BotError::Network(_)),
        "expected Network, got {err:?}",
    );
}

// ── idempotency forwarding ───────────────────────────────────────────

#[tokio::test]
async fn idempotency_key_is_forwarded_as_header() {
    let server = MockServer::start().await;
    let key = Uuid::now_v7();
    let key_lower = key.as_hyphenated().to_string();

    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .and(header("Idempotency-Key", key_lower.as_str()))
        .respond_with(create_response("vbot_idem", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_idem/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_idem",
            "status_changes": changes(&[]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let mut a = args("https://zoom.us/j/idem");
    a.idempotency_key = key;
    let bot_id = driver
        .bot_create(a)
        .await
        .expect("must succeed if header matches");
    let _ = driver.bot_leave(bot_id).await;
}

// ── eject reason projection end-to-end ───────────────────────────────

#[tokio::test]
async fn fatal_with_kicked_sub_code_lands_in_ejected_host_removed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_kick", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_kick/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_kick",
            "status_changes": changes(&[
                ("joining_call", None),
                ("fatal", Some("bot_kicked_from_call")),
            ]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/kick"))
        .await
        .expect("create ok");
    let final_state =
        wait_for_state(&driver, bot_id, |s| matches!(s, BotState::Ejected { .. })).await;
    assert!(
        matches!(
            final_state,
            BotState::Ejected {
                reason: EjectReason::HostRemoved,
            },
        ),
        "got: {final_state:?}",
    );
}

// ── subscribe semantics ──────────────────────────────────────────────

#[tokio::test]
async fn subscribe_for_unknown_bot_yields_closed_receiver() {
    let server = MockServer::start().await;
    let driver = driver_for(&server);
    let unknown = BotId::now_v7();
    let mut rx = driver.subscribe_state(unknown);
    // A closed channel returns `Err(Closed)` immediately on recv.
    let r = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(
        matches!(r, Ok(Err(_))),
        "expected closed receiver, got {r:?}"
    );
}

// ── small helpers tested ─────────────────────────────────────────────

#[tokio::test]
async fn capabilities_advertise_zoom_meet_and_teams() {
    let server = MockServer::start().await;
    let driver = driver_for(&server);
    let cap = driver.capabilities();
    let plats: Vec<_> = cap.platforms.to_vec();
    assert!(plats.contains(&Platform::Zoom));
    assert!(plats.contains(&Platform::GoogleMeet));
    assert!(plats.contains(&Platform::MicrosoftTeams));
    assert!(cap.live_partial_transcripts);
    assert!(cap.granular_eject_reasons);
    assert!(!cap.raw_pcm_access);
}

#[tokio::test]
async fn metadata_is_echoed_on_state_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_meta", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_meta/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_meta",
            "status_changes": changes(&[]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let mut a = args("https://zoom.us/j/meta");
    a.metadata = json!({ "trace_id": "abc-123" });
    let bot_id = driver.bot_create(a).await.expect("create ok");
    let mut rx = driver.subscribe_state(bot_id);
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("event arrived")
        .expect("event ok");
    assert_eq!(ev.metadata, json!({ "trace_id": "abc-123" }));
    let _ = driver.bot_leave(bot_id).await;
}

// ── request body validation ──────────────────────────────────────────

#[tokio::test]
async fn create_bot_body_contains_recording_config_and_audio_payload() {
    let server = MockServer::start().await;
    // Custom matcher that asserts on the deserialized body — we want
    // to confirm the placeholder MP3 b64 is present and the
    // recording_config has the meeting_captions provider per
    // spike-findings recommendation.
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .and(BodyMatcher)
        .respond_with(create_response("vbot_body", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_body/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_body",
            "status_changes": changes(&[]),
        })))
        .mount(&server)
        .await;
    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/body"))
        .await
        .expect("create ok");
    let _ = driver.bot_leave(bot_id).await;
}

// ── race: leave immediately after create ─────────────────────────────

#[tokio::test]
async fn leave_immediately_after_create_does_not_publish_after_terminal() {
    // Codex-flagged race: between `bot_create` returning and the
    // polling task firing the synthetic `Init → … → Joining`
    // ladder, a fast `bot_leave` should make the polling task
    // exit cleanly without emitting pre-join transitions AFTER a
    // terminal `Completed`. The polling task sleeps one
    // `poll_interval` first; the cancellation oneshot + the
    // terminal-state recheck inside the task close the window.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_race", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/vbot_race/leave_call/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/bot/vbot_race/"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_race/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_race",
            "status_changes": changes(&[("joining_call", None)]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/race"))
        .await
        .expect("create ok");
    let mut rx = driver.subscribe_state(bot_id);
    // Leave BEFORE the poll task wakes from its initial sleep.
    driver.bot_leave(bot_id).await.expect("leave ok");

    // We should observe `Completed` (the leave-completion event)
    // and then the channel closes once all senders are dropped.
    // No `LoadingPersona` / `Joining` may follow the terminal.
    let mut last_state: Option<BotState> = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        last_state = Some(ev.state.clone());
        if matches!(ev.state, BotState::Completed) {
            break;
        }
    }
    // Drain a bit longer to confirm no further events arrive.
    let extra = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        extra.is_err() || matches!(extra, Ok(Err(_))),
        "no further events expected after terminal; got {extra:?}",
    );
    assert!(
        matches!(last_state, Some(BotState::Completed)),
        "expected to land in Completed; got {last_state:?}",
    );
    assert_eq!(
        driver.current_state(bot_id),
        Some(BotState::Completed),
        "driver state must be terminal",
    );
}

// ── host-ended distinction ───────────────────────────────────────────

#[tokio::test]
async fn host_ended_call_ended_lands_in_host_ended_state() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_host", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_host/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_host",
            "status_changes": changes(&[
                ("joining_call", None),
                ("in_call_recording", None),
                // Host ended — no `bot_received_leave_call` sub_code.
                ("call_ended", None),
                ("done", None),
            ]),
        })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/host"))
        .await
        .expect("create ok");
    let final_state = wait_for_state(&driver, bot_id, |s| matches!(s, BotState::HostEnded)).await;
    assert_eq!(final_state, BotState::HostEnded);
}

// ── 404 idempotency ──────────────────────────────────────────────────

#[tokio::test]
async fn leave_handles_vendor_404_as_idempotent_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_404", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_404/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_404",
            "status_changes": changes(&[
                ("joining_call", None),
                ("in_call_recording", None),
            ]),
        })))
        .mount(&server)
        .await;
    // Recall-side leave_call returns 404 — the bot already exited
    // on the vendor's side. Driver must treat this as success.
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/vbot_404/leave_call/"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "detail": "gone" })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/notfound"))
        .await
        .expect("create ok");
    wait_for_state(&driver, bot_id, |s| matches!(s, BotState::InMeeting)).await;
    driver
        .bot_leave(bot_id)
        .await
        .expect("404 from vendor must be idempotent");
}

#[tokio::test]
async fn terminate_handles_vendor_404_as_idempotent_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_404t", changes(&[])))
        .mount(&server)
        .await;
    // No GET responses set up — the create returns and the polling
    // task hits a 404 while the test races to terminate. (The
    // create response itself contains no status_changes; the task
    // sleeps before polling.)
    Mock::given(method("DELETE"))
        .and(path("/api/v1/bot/vbot_404t/"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "detail": "gone" })))
        .mount(&server)
        .await;

    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/term404"))
        .await
        .expect("create ok");
    driver
        .bot_terminate(bot_id)
        .await
        .expect("404 from vendor must be idempotent");
}

#[tokio::test]
async fn already_terminal_terminate_returns_idempotent_ok() {
    // Codex suggestion: if the bot is already terminal, terminate
    // should be Ok(()) (matches bot_leave's behavior). Drive the
    // bot to Completed first, then call terminate.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/"))
        .respond_with(create_response("vbot_done", changes(&[])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/bot/vbot_done/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "vbot_done",
            "status_changes": changes(&[("done", None)]),
        })))
        .mount(&server)
        .await;
    let driver = driver_for(&server);
    let bot_id = driver
        .bot_create(args("https://zoom.us/j/already-done"))
        .await
        .expect("create ok");
    wait_for_state(&driver, bot_id, |s| matches!(s, BotState::Completed)).await;
    driver
        .bot_terminate(bot_id)
        .await
        .expect("terminate on terminal bot must be idempotent");
}

struct BodyMatcher;
impl wiremock::Match for BodyMatcher {
    fn matches(&self, req: &Request) -> bool {
        let Ok(parsed) = serde_json::from_slice::<Value>(&req.body) else {
            return false;
        };
        let has_meeting_captions = parsed
            .pointer("/recording_config/transcript/provider/meeting_captions")
            .is_some();
        let has_audio = parsed
            .pointer("/automatic_audio_output/in_call_recording/data/b64_data")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let has_meeting_url = parsed.get("meeting_url").is_some();
        let has_bot_name = parsed.get("bot_name").is_some();
        has_meeting_captions && has_audio && has_meeting_url && has_bot_name
    }
}
