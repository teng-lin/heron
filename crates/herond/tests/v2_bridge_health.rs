//! Cross-crate bridge-health propagation: `heron-bridge::HealthVerdict`
//! → `heron-session::EventPayload::DoctorWarning` envelope on the
//! orchestrator's bus → `herond` `/v1/events` SSE projection.
//!
//! Per `docs/codebase-gaps.md` item #10, seam #4: bridge health
//! degradation propagates to daemon/desktop status.
//!
//! Today's wiring constraint: the orchestrator does not yet own a
//! `NaiveBridge` (codebase-gaps items #1, #3 — bot+bridge+realtime
//! composition). The contract these tests *can* pin without that
//! wiring is the public boundary itself: a `HealthVerdict::Critical`
//! / `Degraded` from `heron-bridge::verdict` maps deterministically
//! onto the `EventPayload::DoctorWarning` shape the
//! `heron-event-http` SSE projection forwards. When the future
//! orchestrator-side bridge monitor lands it can use the same
//! conversion the test below pins; without that pin, a silent
//! drift between the bridge's verdict variants and the session
//! payload's `DoctorComponent` enum would only surface as a runtime
//! mismatch in the production wiring PR.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use heron_bridge::{BridgeHealth, CriticalReason, DegradationReason, HealthVerdict, verdict};
use heron_event::Envelope;
use heron_orchestrator::LocalSessionOrchestrator;
use heron_session::{
    DoctorComponent, DoctorWarningData, EventPayload, SessionEventBus, SessionOrchestrator,
};
use herond::{AppState, AuthConfig, build_app};
use tower::ServiceExt;

const TEST_BEARER: &str = "test-bearer-abcdef";

/// Single conversion the orchestrator's future bridge monitor will
/// invoke on every `HealthVerdict` change. Co-located with the test
/// rather than promoted to a public helper because (a) only one
/// production caller will use it once the wiring lands and that
/// caller's exact shape is still being designed, (b) embedding the
/// mapping in the test keeps the assertion's expected output
/// adjacent to the input — a future change to either side fails
/// here visibly instead of silently passing.
fn doctor_warning_for(v: HealthVerdict) -> Option<DoctorWarningData> {
    match v {
        HealthVerdict::Healthy => None,
        HealthVerdict::Degraded { reason } => Some(DoctorWarningData {
            component: DoctorComponent::Capture,
            message: match reason {
                DegradationReason::Jitter { observed_ms } => {
                    format!("bridge jitter degraded ({observed_ms:.0} ms)")
                }
                DegradationReason::PacketLoss { observed_drops } => {
                    format!("bridge packet loss degraded ({observed_drops}/s)")
                }
                // `DegradationReason` is `#[non_exhaustive]`; a future
                // variant must surface explicitly as the production
                // bridge-monitor wiring lands. Until then, keep the
                // mapping honest with a generic message rather than
                // failing closed.
                _ => "bridge degraded".into(),
            },
        }),
        HealthVerdict::Critical { reason } => Some(DoctorWarningData {
            component: DoctorComponent::Capture,
            message: match reason {
                CriticalReason::AecTrackingLost => "bridge AEC tracking lost".into(),
                CriticalReason::JitterCritical { observed_ms } => {
                    format!("bridge jitter critical ({observed_ms:.0} ms)")
                }
                CriticalReason::PacketLossCritical { observed_drops } => {
                    format!("bridge packet loss critical ({observed_drops}/s)")
                }
                // Same `#[non_exhaustive]` rationale as DegradationReason.
                _ => "bridge critical".into(),
            },
        }),
        // `HealthVerdict` is also `#[non_exhaustive]`. A future
        // `Recovering` (or similar) variant should be mapped
        // intentionally; default to "no warning" so the contract
        // skews toward "don't spam the audit log."
        _ => None,
    }
}

fn live_state() -> (AppState, Arc<LocalSessionOrchestrator>) {
    let orch = Arc::new(LocalSessionOrchestrator::new());
    let state = AppState {
        orchestrator: orch.clone(),
        auth: Arc::new(AuthConfig {
            bearer: TEST_BEARER.to_owned(),
        }),
    };
    (state, orch)
}

#[tokio::test]
async fn aec_loss_maps_to_doctor_warning_payload() {
    // The map is deterministic and the message names the failure mode
    // so audit-log readers can grep for it.
    let v = verdict(&BridgeHealth {
        aec_tracking: false,
        jitter_ms: 0.0,
        recent_drops: 0,
    });
    let warning = doctor_warning_for(v).expect("non-Healthy verdict must map");
    assert_eq!(warning.component, DoctorComponent::Capture);
    assert!(
        warning.message.contains("AEC"),
        "message must name the AEC failure: {}",
        warning.message,
    );
}

#[tokio::test]
async fn healthy_verdict_does_not_emit_doctor_warning() {
    let v = verdict(&BridgeHealth {
        aec_tracking: true,
        jitter_ms: 10.0,
        recent_drops: 0,
    });
    assert!(
        doctor_warning_for(v).is_none(),
        "Healthy verdict must not produce a warning",
    );
}

#[tokio::test]
async fn doctor_warning_round_trips_through_bus_to_sse_subscriber() {
    // End-to-end: simulate the bridge monitor publishing a
    // `DoctorWarning` to the live orchestrator's bus, and assert the
    // daemon's `/v1/events` SSE stream forwards it with the expected
    // wire framing. The replay-cache recorder spawned by
    // `LocalSessionOrchestrator` runs in the same runtime so this
    // exercises the same path a production bridge monitor would take.
    let (state, orch) = live_state();
    let bus: SessionEventBus = orch.event_bus();
    let app = build_app(state);

    let critical = verdict(&BridgeHealth {
        aec_tracking: true,
        jitter_ms: 250.0,
        recent_drops: 0,
    });
    let warning = doctor_warning_for(critical).expect("Critical maps to a warning");

    let req = Request::get("/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_BEARER}"))
        .body(Body::empty())
        .unwrap();
    let response_fut = tokio::spawn(async move { app.oneshot(req).await.unwrap() });

    // Wait for the SSE handler's subscriber to register. The
    // orchestrator's recorder task is itself a subscriber (count
    // baseline = 1), so we need >= 2 here — the api.rs StubOrchestrator
    // tests can poll for `> 0` because Stub has no recorder. Without
    // this, a publish-before-subscribe drops the envelope on a
    // broadcast channel that the SSE handler hadn't joined yet.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while bus.subscriber_count() < 2 {
        if std::time::Instant::now() >= deadline {
            panic!(
                "SSE subscriber never registered (count={})",
                bus.subscriber_count(),
            );
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let envelope = Envelope::new(EventPayload::DoctorWarning(warning.clone()));
    let event_id = envelope.event_id;
    let delivered = bus.publish(envelope);
    assert!(delivered >= 1, "publish saw no live subscribers");

    drop(bus);
    orch.shutdown().await.expect("orchestrator shutdown");
    drop(orch);

    let response = response_fut.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let body = String::from_utf8(body_bytes.to_vec()).expect("utf-8 body");

    assert!(
        body.contains(&format!("id: {event_id}")),
        "SSE id frame missing: {body}",
    );
    assert!(
        body.contains("event: doctor.warning"),
        "SSE event-type frame missing: {body}",
    );
    assert!(
        body.contains(&warning.message),
        "DoctorWarning message did not survive the SSE projection: {body}",
    );
}

#[tokio::test]
#[ignore = "TODO: requires production bridge monitor wired into LocalSessionOrchestrator (codebase-gaps.md items #1, #3) — the orchestrator does not yet own a NaiveBridge or any monitor task that derives DoctorWarning envelopes from verdict() transitions"]
async fn bridge_health_degradation_drives_doctor_warning_through_orchestrator() {
    // When the orchestrator owns a bridge and a monitor task, this
    // test should:
    //   1. Build a LocalSessionOrchestrator wired to a NaiveBridge
    //      whose health() initially reports Healthy.
    //   2. Subscribe to the daemon's /v1/events SSE.
    //   3. Drive the bridge into a Critical state (e.g. via a
    //      test-only AEC tap drop or by feeding a frame with
    //      jitter > JITTER_CRITICAL_MS).
    //   4. Assert a `doctor.warning` envelope reaches the SSE
    //      subscriber within a bounded poll budget — and that
    //      transitioning back to Healthy emits no recovery warning
    //      (the production contract emits warnings only on
    //      degradation, not on recovery).
}
