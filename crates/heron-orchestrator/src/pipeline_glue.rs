//! Glue between `LocalSessionOrchestrator` and the `heron-pipeline`
//! v1 capture pipeline.
//!
//! `start_capture` spawns the v1
//! [`heron_pipeline::session::Orchestrator`] on a dedicated blocking
//! thread and returns once the FSM has transitioned to `Recording`; a
//! background waiter calls [`complete_pipeline_meeting`] when the
//! pipeline finalises so the orchestrator's bookkeeping
//! (`finalized_meetings`, the bus `MeetingCompleted` event) lands
//! without the HTTP request being held open.
//!
//! The error-mapping helpers here ([`pipeline_to_session_error`],
//! [`transition_to_session_error`]) translate the v1 pipeline's typed
//! errors into the HTTP `SessionError` shape so daemon callers see a
//! single error surface across paths, and the FSM-internal
//! "shouldn't happen" cases produce a `Validation` payload that
//! captures the diagnostic.
//!
//! [`insert_finalized_meeting`] / [`push_pruned_finalizer`] enforce
//! the bounded-memory discipline on the per-orchestrator finalized
//! index and finalizer JoinHandle list — without the prune, a
//! long-running daemon would accumulate one handle per ended meeting
//! until `shutdown()` was called.

use std::collections::HashMap;
use std::sync::Mutex;

use heron_event::Envelope;
use heron_metrics::redacted;
use heron_pipeline::session::{
    SessionError as CliSessionError, SessionOutcome as CliSessionOutcome,
};
use heron_session::{
    EventPayload, Meeting, MeetingCompletedData, MeetingId, MeetingOutcome, MeetingStatus,
    SessionError, SessionEventBus, SummaryLifecycle, TranscriptLifecycle,
};
use heron_types::{RecordingFsm, SummaryOutcome};
use tokio::task::JoinHandle;

use crate::lock_or_recover;
use crate::metrics_names::{CAPTURE_ENDED_TOTAL, SALVAGE_RECOVERY_TOTAL};
use crate::state::{FINALIZED_MEETING_INDEX_CAP, FinalizedMeeting};

pub(crate) fn pipeline_to_session_error(err: CliSessionError) -> SessionError {
    match err {
        CliSessionError::Audio(e) => SessionError::Validation {
            detail: format!("audio capture failed: {e}"),
        },
        CliSessionError::Stt(e) => SessionError::Validation {
            detail: format!("STT failed: {e}"),
        },
        CliSessionError::Llm(e) => SessionError::LlmProviderFailed {
            provider: "auto".to_owned(),
            detail: e.to_string(),
        },
        CliSessionError::Vault(e) => SessionError::VaultLocked {
            detail: e.to_string(),
        },
        CliSessionError::Transition(e) => transition_to_session_error(e),
        other => SessionError::Validation {
            detail: format!("capture pipeline failed: {other}"),
        },
    }
}

/// Map a [`heron_types::TransitionError`] to the closest
/// [`SessionError`] for the HTTP projection. A transition error from
/// the orchestrator's own FSM walks is "shouldn't happen" — it would
/// mean the FSM disagrees with the orchestrator's own bookkeeping —
/// so map to `Validation` and surface the FSM's diagnostic so a real
/// occurrence can be investigated.
pub(crate) fn transition_to_session_error(err: heron_types::TransitionError) -> SessionError {
    SessionError::Validation {
        detail: format!("FSM rejected internal transition: {err}"),
    }
}

pub(crate) fn complete_pipeline_meeting(
    bus: &SessionEventBus,
    finalized_meetings: &Mutex<HashMap<MeetingId, FinalizedMeeting>>,
    id: MeetingId,
    mut fsm: RecordingFsm,
    mut meeting: Meeting,
    result: Result<CliSessionOutcome, SessionError>,
) {
    let (note_path, failure_reason) = match result {
        Ok(outcome) => {
            let note_path = outcome.note_path;
            let summary = if note_path.is_some() {
                SummaryOutcome::Done
            } else {
                SummaryOutcome::Failed
            };
            if let Err(err) = fsm
                .on_transcribe_done()
                .and_then(|_| fsm.on_summary(summary))
            {
                let reason = format!("FSM rejected pipeline completion: {err}");
                (None, Some(reason))
            } else {
                (note_path, None)
            }
        }
        Err(err) => {
            let reason = err.to_string();
            let _ = fsm.on_transcribe_done();
            let _ = fsm.on_summary(SummaryOutcome::Failed);
            (None, Some(reason))
        }
    };
    let success = note_path.is_some();
    meeting.status = if success {
        MeetingStatus::Done
    } else {
        MeetingStatus::Failed
    };
    meeting.transcript_status = if success {
        TranscriptLifecycle::Complete
    } else {
        TranscriptLifecycle::Failed
    };
    meeting.summary_status = if success {
        SummaryLifecycle::Ready
    } else {
        SummaryLifecycle::Failed
    };
    insert_finalized_meeting(
        finalized_meetings,
        id,
        FinalizedMeeting {
            meeting: meeting.clone(),
            note_path,
        },
    );
    // Capture-lifecycle counter: the pipeline-side disposition.
    // `success` and `error` are the steady-state pair; `user_stop`
    // already fired from `end_meeting` when the request handler
    // returned. Both are valid reasons for the same lifecycle — the
    // dashboard summing these makes the timeline obvious. Labels are
    // pinned `redacted!` literals; `into_inner()` is the immediate
    // expression per the foundation's observability rule.
    let outcome_label = if success {
        redacted!("success")
    } else {
        redacted!("error")
    };
    metrics::counter!(
        CAPTURE_ENDED_TOTAL,
        "reason" => outcome_label.into_inner(),
    )
    .increment(1);
    // Salvage recovery counter. The v1 capture pipeline is the only
    // path that writes a `state.json` cache today, and its purge-or-
    // retain decision in `heron-pipeline` already encodes the
    // disposition we care about: a successful finalize purged the
    // cache (no salvage left behind = `recovered`), a failed
    // finalize retained the WAVs for the user to manually salvage on
    // next launch (= `abandoned`). The third arm `failed` is reserved
    // for the future hard-error recovery path (an attempt to recover
    // a previous session's cache that errored out); without that
    // flow today, only `recovered` and `abandoned` fire from this
    // call site. The `outcome` label dimension matches the spec in
    // #224 / `docs/observability.md` so a future hard-error
    // instrumentation slots in without renaming.
    let salvage_label = if success {
        redacted!("recovered")
    } else {
        redacted!("abandoned")
    };
    metrics::counter!(
        SALVAGE_RECOVERY_TOTAL,
        "outcome" => salvage_label.into_inner(),
    )
    .increment(1);
    publish_meeting_event(
        bus,
        EventPayload::MeetingCompleted(MeetingCompletedData {
            meeting,
            outcome: if success {
                MeetingOutcome::Success
            } else {
                MeetingOutcome::Failed
            },
            failure_reason,
        }),
        id,
    );
}

/// Drop already-completed handles from the finalizers list and
/// push `handle`. Without this prune, a long-running daemon
/// would accumulate one `JoinHandle` per ended meeting until
/// `shutdown()` was called. Tasks that have not yet finished are
/// retained: `shutdown()` still needs to drain them so terminal
/// events make it into the replay cache before the recorder
/// stops.
pub(crate) fn push_pruned_finalizer(
    finalizers: &Mutex<Vec<JoinHandle<()>>>,
    handle: JoinHandle<()>,
) {
    let mut guard = lock_or_recover(finalizers);
    guard.retain(|h| !h.is_finished());
    guard.push(handle);
}

pub(crate) fn insert_finalized_meeting(
    finalized_meetings: &Mutex<HashMap<MeetingId, FinalizedMeeting>>,
    id: MeetingId,
    finalized: FinalizedMeeting,
) {
    let mut index = lock_or_recover(finalized_meetings);
    if !index.contains_key(&id)
        && index.len() >= FINALIZED_MEETING_INDEX_CAP
        && let Some(oldest_id) = index
            .iter()
            .min_by_key(|(_, item)| item.meeting.started_at)
            .map(|(id, _)| *id)
    {
        index.remove(&oldest_id);
    }
    index.insert(id, finalized);
}

/// Wrap an [`EventPayload`] in an [`Envelope`] scoped to `meeting_id`
/// and publish it on the bus. Helper so every transition site picks
/// up the same `with_meeting` framing without each one re-stringifying
/// the id (the consistency contract on `Envelope::meeting_id` requires
/// it match the meeting carried in the payload).
pub(crate) fn publish_meeting_event(
    bus: &SessionEventBus,
    payload: EventPayload,
    meeting_id: MeetingId,
) {
    bus.publish(Envelope::new(payload).with_meeting(meeting_id.to_string()));
}
