//! Characterization tests for the pre-meeting context flows on
//! [`heron_orchestrator::LocalSessionOrchestrator`] (`attach_context`,
//! `prepare_context`, plus the `pending_context` diagnostic accessor).
//!
//! These tests pin the **current** observable behavior of the public
//! surface BEFORE the #222 plan's PR B (commit 2) bundles the
//! pending/applied context maps into a `pub(crate) struct
//! ContextState` held under one `Arc<Mutex<_>>`. The plan calls these
//! out as the safety net for the state-bundling commit — assertions
//! drive `attach_context` / `prepare_context` and read back via the
//! public `pending_context` accessor, so the contract holds whether
//! state lives in the current per-field layout or in the bundled
//! `ContextState` PR B introduces.
//!
//! Per the #222 plan §"PR A — characterization tests" §
//! `tests/context_flows.rs`. The validation surface (`attach_context`
//! oversize / empty-id rejection) is exercised in the in-crate tests
//! already; this file complements them with the round-trip + size-
//! validator integration paths.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use heron_orchestrator::LocalSessionOrchestrator;
use heron_session::{
    PreMeetingContext, PreMeetingContextRequest, SessionError, SessionOrchestrator,
};

// ── attach_context round-trip ─────────────────────────────────────────

/// Pins behavior of #222 plan §PR A `context_flows.rs` for PR B's
/// `ContextState` bundling (commit 2). After `attach_context` stages a
/// `PreMeetingContext` for a calendar event id, the diagnostic
/// `pending_context` accessor must return the same body — independent
/// of vault configuration. The registry is in-memory, so a vault-less
/// orchestrator exercises the same path the desktop hits when no
/// vault is configured.
#[tokio::test]
async fn attach_context_then_pending_context_returns_attached() {
    let orch = LocalSessionOrchestrator::new();

    let context = PreMeetingContext {
        agenda: Some("Q3 planning".to_owned()),
        attendees_known: vec![heron_session::AttendeeContext {
            name: "Ada".to_owned(),
            email: Some("ada@example.com".to_owned()),
            last_seen_in: None,
            relationship: Some("CEO".to_owned()),
            notes: None,
        }],
        related_notes: vec!["meetings/2026-04-12.md".to_owned()],
        prior_decisions: Vec::new(),
        user_briefing: Some("Focus on launch readiness.".to_owned()),
    };

    orch.attach_context(PreMeetingContextRequest {
        calendar_event_id: "evt_planning".into(),
        context: context.clone(),
    })
    .await
    .expect("attach_context");

    let staged = orch
        .pending_context("evt_planning")
        .expect("staged context retrievable through diagnostic accessor");
    assert_eq!(staged.agenda.as_deref(), Some("Q3 planning"));
    assert_eq!(staged.attendees_known.len(), 1);
    assert_eq!(staged.attendees_known[0].name, "Ada");
    assert_eq!(
        staged.attendees_known[0].email.as_deref(),
        Some("ada@example.com"),
    );
    assert_eq!(
        staged.attendees_known[0].relationship.as_deref(),
        Some("CEO"),
    );
    assert_eq!(
        staged.related_notes,
        vec!["meetings/2026-04-12.md".to_owned()]
    );
    assert_eq!(
        staged.user_briefing.as_deref(),
        Some("Focus on launch readiness."),
    );

    // Unrelated id is unstaged — pin the negative path so the accessor
    // can't be implemented as "always return the latest" (which would
    // pass the positive assertion above for the wrong reason).
    assert!(
        orch.pending_context("evt_other").is_none(),
        "unrelated id must not return a staged context",
    );
}

// ── prepare_context size validation ───────────────────────────────────

/// Pins behavior of #222 plan §PR A for PR B's `context.rs`
/// extraction (commit 4). `prepare_context` synthesizes a default
/// `PreMeetingContext` with the provided attendees lifted into
/// `attendees_known`, then runs it through the same
/// `validate_context_size` guard `attach_context` uses. The guard
/// caps the JSON-serialized payload at `MAX_PRE_MEETING_CONTEXT_BYTES`
/// — a request whose attendee list serializes past the cap must be
/// rejected with `SessionError::Validation` and leave the registry
/// untouched. Without this contract, a future `prepare_context`
/// synthesizer that grows the body could silently break the on-disk
/// size guarantee.
///
/// The cap is 256 KiB. To exceed it we craft a single attendee with
/// a large `notes` field — one entry past the cap is enough; the
/// rejection path doesn't depend on entry count, only on serialized
/// size.
#[tokio::test]
async fn prepare_context_with_oversize_request_returns_validation_error() {
    let orch = LocalSessionOrchestrator::new();

    // 256 KiB is `MAX_PRE_MEETING_CONTEXT_BYTES` (private constant —
    // we can't import it from outside the crate, so use a value
    // comfortably above the cap to stay robust against either side
    // adjusting the bound). 512 KiB of notes is ~2× cap and is well
    // past anything a real synthesizer would produce, so this test
    // exercises the rejection edge without coupling to the constant.
    let huge_notes = "x".repeat(512 * 1024);
    let attendees = vec![heron_session::AttendeeContext {
        name: "Mallory".to_owned(),
        email: Some("mallory@example.com".to_owned()),
        last_seen_in: None,
        relationship: None,
        // The size validator runs against the full
        // `PreMeetingContext` JSON; oversized notes inside an
        // attendee push past the cap the same way oversized agenda
        // / briefing would.
        notes: Some(huge_notes),
    }];

    let err = orch
        .prepare_context(heron_session::PrepareContextRequest {
            calendar_event_id: "evt_oversize".into(),
            attendees,
        })
        .await
        .expect_err("oversize prepare must be rejected");
    assert!(
        matches!(err, SessionError::Validation { .. }),
        "expected SessionError::Validation, got {err:?}",
    );

    // Rejection must NOT mutate the registry — the public surface
    // contract is that a Validation error leaves nothing behind.
    // Without this, a partial synthesizer write past the cap would
    // leave a half-formed context in the staging map.
    assert!(
        orch.pending_context("evt_oversize").is_none(),
        "validation failure must not stage anything",
    );
}
