//! Cross-crate integration: `DefaultSpeechController` (heron-policy) ↔
//! `MockRealtimeBackend` (heron-realtime).
//!
//! The crate's own controller tests use a private `TestRealtimeBackend`
//! to assert internal call sequences. These tests use the *public*
//! `MockRealtimeBackend` from `heron-realtime` so the seam between the
//! two crates is exercised through their published surfaces — a future
//! refactor that breaks the contract (e.g. trait method renamed,
//! capability matrix shifted) surfaces here rather than only inside
//! whichever crate owns the inline mock.
//!
//! See `docs/codebase-gaps.md` item #10 ("v2 cross-crate integration
//! coverage") seam #3: policy-denied speech never reaches the backend
//! in an orchestrated session.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use heron_policy::{
    CancelReason, DefaultSpeechController, EscalationMode, PolicyProfile, Priority,
    SpeechController, SpeechError, SpeechEvent,
};
use heron_realtime::{
    MockRealtimeBackend, RealtimeBackend, RealtimeEvent, SessionConfig, TurnDetection,
};
use tokio::sync::broadcast;
use tokio::time::timeout;

fn turn_detection() -> TurnDetection {
    TurnDetection {
        vad_threshold: 0.5,
        prefix_padding_ms: 300,
        silence_duration_ms: 500,
        interrupt_response: true,
        auto_create_response: true,
    }
}

fn session_config() -> SessionConfig {
    SessionConfig {
        system_prompt: "You are a helpful meeting assistant.".to_owned(),
        tools: vec![],
        turn_detection: turn_detection(),
        voice: "alloy".to_owned(),
    }
}

fn open_profile() -> PolicyProfile {
    PolicyProfile {
        allow_topics: vec![],
        deny_topics: vec![],
        mute: false,
        escalation: EscalationMode::None,
    }
}

async fn open_session(backend: &MockRealtimeBackend) -> heron_realtime::SessionId {
    backend
        .session_open(session_config())
        .await
        .expect("session_open")
}

/// Best-effort drain. Returns whatever shows up within `dur`.
async fn drain_speech_events(
    rx: &mut broadcast::Receiver<SpeechEvent>,
    max: usize,
    dur: Duration,
) -> Vec<SpeechEvent> {
    let mut out = Vec::with_capacity(max);
    for _ in 0..max {
        match timeout(dur, rx.recv()).await {
            Ok(Ok(e)) => out.push(e),
            _ => break,
        }
    }
    out
}

#[tokio::test]
async fn policy_mute_blocks_emission_at_public_backend_surface() {
    let backend = Arc::new(MockRealtimeBackend::new());
    let session = open_session(&backend).await;
    let mut backend_events = backend.subscribe_events(session);

    let mut profile = open_profile();
    profile.mute = true;
    let controller = DefaultSpeechController::new(Arc::clone(&backend), session, profile);
    let mut speech_events = controller.subscribe_events();

    let err = controller
        .speak("hi there", Priority::Append, None)
        .await
        .expect_err("muted profile must reject");
    assert!(matches!(err, SpeechError::PolicyDenied { rule } if rule == "muted"));

    let observed = drain_speech_events(&mut speech_events, 1, Duration::from_millis(50)).await;
    assert!(
        matches!(
            observed.as_slice(),
            [SpeechEvent::Cancelled {
                reason: CancelReason::PolicyDenied { rule },
                ..
            }] if rule == "muted"
        ),
        "expected one PolicyDenied(muted) audit event, got {observed:?}",
    );

    // Public-surface assertion: nothing reached the backend. If the
    // controller had erroneously called response_create the mock would
    // have emitted ResponseCreated, which we'd observe here.
    let leaked = timeout(Duration::from_millis(50), backend_events.recv()).await;
    assert!(
        leaked.is_err(),
        "policy-blocked utterance leaked to backend: {leaked:?}",
    );
}

#[tokio::test]
async fn policy_deny_topic_blocks_emission_at_public_backend_surface() {
    let backend = Arc::new(MockRealtimeBackend::new());
    let session = open_session(&backend).await;
    let mut backend_events = backend.subscribe_events(session);

    let mut profile = open_profile();
    profile.deny_topics = vec!["compensation".into()];
    let controller = DefaultSpeechController::new(Arc::clone(&backend), session, profile);
    let mut speech_events = controller.subscribe_events();

    let err = controller
        .speak("their compensation package", Priority::Append, None)
        .await
        .expect_err("deny-topic must reject");
    assert!(matches!(err, SpeechError::PolicyDenied { rule } if rule.contains("compensation")));

    let observed = drain_speech_events(&mut speech_events, 1, Duration::from_millis(50)).await;
    assert!(
        matches!(
            observed.as_slice(),
            [SpeechEvent::Cancelled {
                reason: CancelReason::PolicyDenied { .. },
                ..
            }],
        ),
        "expected one PolicyDenied audit event, got {observed:?}",
    );

    let leaked = timeout(Duration::from_millis(50), backend_events.recv()).await;
    assert!(
        leaked.is_err(),
        "policy-blocked utterance leaked to backend: {leaked:?}",
    );
}

#[tokio::test]
async fn allowed_speech_propagates_through_to_backend_response_create() {
    // Positive control for the deny tests above: a profile that
    // permits the utterance must result in a ResponseCreated event on
    // the backend. Without this, "no event arrives" assertions in the
    // deny path could be vacuously true if the controller had simply
    // stopped working entirely.
    let backend = Arc::new(MockRealtimeBackend::new());
    let session = open_session(&backend).await;
    let mut backend_events = backend.subscribe_events(session);

    let controller = DefaultSpeechController::new(Arc::clone(&backend), session, open_profile());

    let utt = controller
        .speak("hello, team", Priority::Append, None)
        .await
        .expect("open profile permits speak");

    // `MockRealtimeBackend::response_create` emits `ResponseCreated`
    // synchronously, so a freshly-subscribed receiver gets it first.
    let response = match timeout(Duration::from_millis(200), backend_events.recv()).await {
        Ok(Ok(RealtimeEvent::ResponseCreated { response, .. })) => response,
        other => panic!("expected ResponseCreated on public backend surface, got {other:?}"),
    };
    let recorded = backend
        .expect_response_request(session, response)
        .expect("response_create captured on the public surface");
    assert_eq!(recorded.text, "hello, team");
    assert!(recorded.voice_override.is_none());
    // Prove the speech event handle isn't all-zero — exercises
    // utterance_ids capability on the public boundary.
    assert_ne!(
        utt.to_string(),
        heron_policy::UtteranceId::nil().to_string()
    );
}
