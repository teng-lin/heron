//! Characterization tests for the capture lifecycle methods on
//! [`heron_orchestrator::LocalSessionOrchestrator`] (`start_capture`,
//! `end_meeting`, `pause_capture`, `resume_capture`).
//!
//! These tests exist to pin the **current** observable behavior of the
//! orchestrator's public surface BEFORE the #222 plan's PR B reshape
//! splits `start_capture` (286 LOC), `end_meeting` (134 LOC), and
//! friends into a `capture.rs` concern module. Each test drives the
//! `SessionOrchestrator` trait via its public methods and asserts on
//! returned values + bus envelopes — never on internal module paths or
//! field layout, so the same assertions hold after PR B.
//!
//! Per the #222 plan §"PR A — characterization tests" §
//! `tests/capture_lifecycle.rs`. These tests are intentionally
//! independently valuable as documentation of orchestrator behavior
//! even if PR B never lands.
//!
//! Style mirrors `tests/vault_reads.rs`: `tempfile` for the vault root
//! when one is needed, bus subscriber pattern from `heron-event` for
//! envelope assertions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use heron_event::Envelope;
use heron_orchestrator::{Builder, LocalSessionOrchestrator};
use heron_session::{
    EventPayload, MeetingStatus, Platform, SessionError, SessionEventBus, SessionOrchestrator,
    StartCaptureArgs,
};

/// Wait until `rx` has buffered at least `n` envelopes, draining them
/// in the process. The bus broadcasts synchronously so under normal
/// load this returns within a microsecond; the deadline only hedges
/// against scheduler jitter under load.
async fn drain_at_least(
    rx: &mut tokio::sync::broadcast::Receiver<Envelope<EventPayload>>,
    n: usize,
) -> Vec<Envelope<EventPayload>> {
    let mut events: Vec<Envelope<EventPayload>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while events.len() < n {
        if Instant::now() > deadline {
            let kinds: Vec<&str> = events.iter().map(|e| e.payload.event_type()).collect();
            panic!(
                "expected at least {n} envelopes; got {} ({kinds:?})",
                events.len(),
            );
        }
        if let Ok(env) = rx.try_recv() {
            events.push(env);
        } else {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    events
}

/// Subscribe to the orchestrator's bus *before* any envelope is
/// published — broadcast subscribers only see messages emitted after
/// `subscribe()` returns, so getting the order right is part of the
/// contract every capture-lifecycle test depends on.
fn subscribe(
    orch: &LocalSessionOrchestrator,
) -> tokio::sync::broadcast::Receiver<Envelope<EventPayload>> {
    let bus: SessionEventBus = orch.event_bus();
    bus.subscribe()
}

// ── start_capture ─────────────────────────────────────────────────────

/// Pins behavior of #222 plan §PR A `start_capture_lifecycle.rs` for
/// PR B's `capture.rs` extraction. The substrate-only happy path
/// (no vault, no live-session factory) must still walk
/// `MeetingDetected → MeetingArmed → MeetingStarted` and return a
/// `Meeting { status: Recording, .. }` whose `id` matches the envelope
/// frames. The exact event-type literals are wire contract per the
/// OpenAPI doc — do NOT relax them on refactor.
#[tokio::test]
async fn start_capture_happy_path_publishes_armed_started_envelopes() {
    let orch = LocalSessionOrchestrator::new();
    let mut rx = subscribe(&orch);

    let meeting = orch
        .start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: Some("Standup".into()),
            calendar_event_id: None,
        })
        .await
        .expect("start_capture should succeed in substrate-only mode");

    assert!(matches!(meeting.status, MeetingStatus::Recording));
    assert_eq!(meeting.title.as_deref(), Some("Standup"));
    assert_eq!(meeting.platform, Platform::Zoom);
    assert!(meeting.calendar_event_id.is_none());

    let events = drain_at_least(&mut rx, 3).await;
    let kinds: Vec<&str> = events.iter().map(|e| e.payload.event_type()).collect();
    assert_eq!(
        kinds,
        ["meeting.detected", "meeting.armed", "meeting.started"],
        "FSM walk must emit exactly the detected→armed→started trio",
    );

    // Envelope frame and payload meeting id must match — the SSE
    // resume contract relies on `Envelope.meeting_id` being the same
    // string the payload's `Meeting.id` serializes to.
    let id_str = meeting.id.to_string();
    for env in &events {
        assert_eq!(
            env.meeting_id.as_deref(),
            Some(id_str.as_str()),
            "envelope.meeting_id must match payload meeting id",
        );
    }
}

/// Pins behavior of #222 plan §PR A for PR B's `capture.rs` /
/// `read_side.rs` split. Without a configured vault, the read-side
/// methods (`read_transcript`, `read_summary`, `audio_path`) collapse
/// to `NotYetImplemented` even after a successful capture — there's
/// no on-disk artifact to read back. The capture FSM still drives
/// fully through the substrate-only path; only the read-back is gated
/// by `vault_root.is_some()`.
#[tokio::test]
async fn start_capture_with_no_vault_returns_not_yet_implemented_on_reads() {
    let orch = LocalSessionOrchestrator::new();
    let started = orch
        .start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: None,
            calendar_event_id: None,
        })
        .await
        .expect("substrate-only start_capture must still succeed");

    // Without a vault, every read endpoint short-circuits to
    // NotYetImplemented — even for a meeting the orchestrator just
    // accepted via start_capture. This is the "no on-disk source to
    // scan" branch the #222 plan calls out as the safety-net for PR
    // B's read_side.rs extraction.
    let err = orch
        .read_transcript(&started.id)
        .await
        .expect_err("read_transcript without vault must error");
    assert!(
        matches!(err, SessionError::NotYetImplemented),
        "expected NotYetImplemented, got {err:?}",
    );

    let err = orch
        .read_summary(&started.id)
        .await
        .expect_err("read_summary without vault must error");
    assert!(
        matches!(err, SessionError::NotYetImplemented),
        "expected NotYetImplemented, got {err:?}",
    );

    let err = orch
        .audio_path(&started.id)
        .await
        .expect_err("audio_path without vault must error");
    assert!(
        matches!(err, SessionError::NotYetImplemented),
        "expected NotYetImplemented, got {err:?}",
    );
}

// ── end_meeting ───────────────────────────────────────────────────────

/// Pins behavior of #222 plan §PR A for PR B's `capture.rs` extraction
/// of `end_meeting` (134 LOC). The substrate-only path emits
/// `MeetingEnded` then `MeetingCompleted` synchronously (no background
/// pipeline waiter on the `CaptureRuntime::Synthetic` branch), with
/// the completed payload carrying `MeetingOutcome::Success`,
/// `status = Done`, populated `ended_at` and `duration_secs`. The
/// `Pipeline` runtime defers `MeetingCompleted` to a finalizer task —
/// exercising that is the daemon-side integration tests' job; this
/// test pins the synchronous synth branch which is the safety net the
/// #222 plan calls out.
#[tokio::test]
async fn end_meeting_publishes_meeting_ended_then_meeting_completed() {
    let orch = LocalSessionOrchestrator::new();
    let mut rx = subscribe(&orch);

    let meeting = orch
        .start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: None,
            calendar_event_id: None,
        })
        .await
        .expect("start_capture");

    // Drain the start_capture envelopes so the assertion below
    // scopes strictly to end_meeting's emissions.
    let _ = drain_at_least(&mut rx, 3).await;

    orch.end_meeting(&meeting.id).await.expect("end_meeting");

    let events = drain_at_least(&mut rx, 2).await;
    assert_eq!(
        events.len(),
        2,
        "synthetic-runtime end_meeting must emit exactly 2 events: ended + completed",
    );
    assert!(matches!(events[0].payload, EventPayload::MeetingEnded(_)));
    match &events[1].payload {
        EventPayload::MeetingCompleted(data) => {
            assert!(
                matches!(data.outcome, heron_session::MeetingOutcome::Success),
                "synth runtime must report Success outcome",
            );
            assert!(matches!(data.meeting.status, MeetingStatus::Done));
            assert!(
                data.meeting.ended_at.is_some(),
                "completed meeting must carry ended_at",
            );
            assert!(
                data.meeting.duration_secs.is_some(),
                "completed meeting must carry duration_secs",
            );
        }
        other => panic!(
            "expected MeetingCompleted second, got {}",
            other.event_type()
        ),
    }

    // After end_meeting the meeting moves into the finalized index so
    // the `Location: /v1/meetings/{id}` header herond stamps on the
    // 202-Accepted POST response stays readable. Pin that here too —
    // PR B's capture.rs extraction must preserve the active→finalized
    // hand-off.
    let fetched = orch
        .get_meeting(&meeting.id)
        .await
        .expect("finalized meeting still readable after end");
    assert_eq!(fetched.id, meeting.id);
    assert!(matches!(fetched.status, MeetingStatus::Done));
}

// ── pause / resume ────────────────────────────────────────────────────

/// Pins behavior of #222 plan §PR A for PR B's `capture.rs` extraction
/// of `pause_capture` / `resume_capture` (Tier 3 #16). The FSM must
/// round-trip `Recording → Paused → Recording` cleanly, with each
/// transition observable via `get_meeting` (the wire-shape consumers
/// rely on); after a full cycle the meeting must remain endable so
/// `end_meeting` can finalize it. The replay cache snapshot serves as
/// the secondary observable — five envelopes from the
/// detected/armed/started/ended/completed sequence land regardless of
/// whether the meeting was paused mid-flight.
#[tokio::test]
async fn pause_capture_then_resume_capture_round_trips_fsm() {
    let orch = LocalSessionOrchestrator::new();
    let meeting = orch
        .start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: None,
            calendar_event_id: None,
        })
        .await
        .expect("start_capture");

    // Initial state: Recording.
    let snap = orch.get_meeting(&meeting.id).await.expect("get_meeting");
    assert!(matches!(snap.status, MeetingStatus::Recording));

    // Pause: Recording → Paused.
    orch.pause_capture(&meeting.id).await.expect("pause");
    let snap = orch.get_meeting(&meeting.id).await.expect("get_meeting");
    assert!(matches!(snap.status, MeetingStatus::Paused));

    // Resume: Paused → Recording.
    orch.resume_capture(&meeting.id).await.expect("resume");
    let snap = orch.get_meeting(&meeting.id).await.expect("get_meeting");
    assert!(matches!(snap.status, MeetingStatus::Recording));

    // After a pause/resume cycle the meeting must still finalize
    // through end_meeting. Without this contract Tier 3 #16's
    // Paused → Transcribing edge would silently regress.
    orch.end_meeting(&meeting.id).await.expect("end_meeting");

    // Replay cache pins the bus history regardless of pause/resume:
    // detected, armed, started, ended, completed = 5 entries.
    let deadline = Instant::now() + Duration::from_secs(2);
    while orch.cache_len() < 5 {
        if Instant::now() > deadline {
            panic!(
                "recorder never reached 5 entries (cur={}); pause/resume must not drop bus events",
                orch.cache_len(),
            );
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(orch.cache_len(), 5);
}

// ── concurrency torture ───────────────────────────────────────────────

/// Pins behavior of #222 plan §PR A for PR B's state-ownership
/// reshape (commit 6 in particular). `auto_record_tick` and
/// `end_meeting` both touch `active_meetings`, and the lock-ordering
/// contract documented on `LocalSessionOrchestrator.pending_contexts`
/// is what keeps them from deadlocking. PR B's `AutoRecordState` /
/// `ContextState` bundles must preserve that ordering — this test
/// would surface a deadlock as a hung join handle.
///
/// The test is a smoke/torture check: spawn an `auto_record_tick`
/// (with a denying calendar so it short-circuits before contending
/// for `active_meetings`) alongside an `end_meeting` and assert both
/// complete within a generous deadline.
#[tokio::test]
async fn concurrent_end_meeting_and_auto_record_tick_no_deadlock() {
    use heron_vault::{CalendarError, CalendarReader};

    /// Calendar reader that always denies. `auto_record_tick` will
    /// short-circuit on the calendar read result (the inner method
    /// translates `CalendarError::Denied` to `PermissionMissing`,
    /// which `auto_record_tick` swallows with a debug log and 0
    /// fires). That's enough for this smoke check — the goal is to
    /// exercise the lock-acquisition path concurrently with
    /// `end_meeting`, not to drive a real auto-record fire.
    struct DenyingCalendar;
    impl CalendarReader for DenyingCalendar {
        fn read_window(
            &self,
            _: chrono::DateTime<chrono::Utc>,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<Option<Vec<heron_vault::CalendarEvent>>, CalendarError> {
            Err(CalendarError::Denied)
        }
    }

    let orch = Arc::new(
        Builder::default()
            .calendar(Arc::new(DenyingCalendar))
            .build(),
    );

    let started = orch
        .start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: None,
            calendar_event_id: None,
        })
        .await
        .expect("start_capture");

    // Race a concurrent tick against end_meeting. Whichever order
    // they resolve in, neither must deadlock or panic. The tick
    // returns 0 fires because the calendar denied; end_meeting
    // returns Ok.
    let orch_tick = Arc::clone(&orch);
    let tick = tokio::spawn(async move { orch_tick.auto_record_tick(chrono::Utc::now()).await });
    let orch_end = Arc::clone(&orch);
    let end_id = started.id;
    let end = tokio::spawn(async move { orch_end.end_meeting(&end_id).await });

    // Generous deadline — under normal load both complete in ms.
    // A deadlock would surface as `timeout` returning Err.
    let fired = tokio::time::timeout(Duration::from_secs(5), tick)
        .await
        .expect("auto_record_tick must not deadlock with end_meeting")
        .expect("tick join");
    assert_eq!(
        fired, 0,
        "denying calendar must yield zero fires regardless of contention",
    );
    let end_result = tokio::time::timeout(Duration::from_secs(5), end)
        .await
        .expect("end_meeting must not deadlock with auto_record_tick")
        .expect("end join");
    end_result.expect("end_meeting result");
}
