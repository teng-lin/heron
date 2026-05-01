//! Pre-meeting context staging.
//!
//! `attach_context` (the user-supplied path) and `prepare_context`
//! (the synthesizer that runs from the rail's "primed" workflow) both
//! land in the orchestrator's [`crate::state::PendingContexts`]
//! bounded staging map; `start_capture` consumes the entry whose
//! `calendar_event_id` matches `StartCaptureArgs::calendar_event_id`.
//!
//! Each function here corresponds 1:1 to a `SessionOrchestrator`
//! trait method on `LocalSessionOrchestrator`; the trait impl block
//! in `lib.rs` becomes a thin one-line delegation.

use heron_session::{
    PreMeetingContext, PreMeetingContextRequest, PrepareContextRequest, SessionError,
};

use crate::LocalSessionOrchestrator;
use crate::validation::{normalize_calendar_event_id, validate_context_size};

pub(crate) async fn attach_context(
    orch: &LocalSessionOrchestrator,
    req: PreMeetingContextRequest,
) -> Result<(), SessionError> {
    let calendar_event_id = normalize_calendar_event_id(&req.calendar_event_id)?;
    let bytes = validate_context_size(&req.context)?;
    let overwrote = orch
        .pending_contexts
        .insert(calendar_event_id.clone(), req.context);
    tracing::info!(
        calendar_event_id = %calendar_event_id,
        overwrote,
        bytes,
        "pre-meeting context attached",
    );
    Ok(())
}

pub(crate) async fn prepare_context(
    orch: &LocalSessionOrchestrator,
    req: PrepareContextRequest,
) -> Result<(), SessionError> {
    let calendar_event_id = normalize_calendar_event_id(&req.calendar_event_id)?;
    // Today's synthesizer is intentionally minimal: lift the
    // calendar event's attendees into `attendees_known` and leave
    // the rest at default. Related-notes lookup needs vault
    // search by attendee/title — that lands with the Ask-bar RAG
    // infrastructure (Tier 6b in the UX redesign doc); until then
    // the priming is enough to flip the rail's `primed` flag and
    // give `start_capture` a non-empty staged entry to consume.
    //
    // Known limitation — synth-id drift: when the upstream
    // calendar reader synthesizes ids from `(start, end, title)`
    // (today's behavior, see `list_upcoming_calendar`), editing
    // the event's title or time changes the id. The previously-
    // staged context becomes orphaned in `pending_contexts` and a
    // fresh `prepare_context` runs against the new id. The orphan
    // ages out via the FIFO cap. Worth pruning explicitly once
    // EventKit exposes a stable id.
    let context = PreMeetingContext {
        attendees_known: req.attendees,
        ..PreMeetingContext::default()
    };
    // Re-use the same size guard as `attach_context` even though
    // today's synthesized context is tiny — keeps the on-disk
    // contract uniform and means a future synthesizer that grows
    // the body fails loudly here rather than silently busting the
    // cap.
    let bytes = validate_context_size(&context)?;
    // `insert_if_absent` is a single-mutex-acquisition check +
    // insert: a concurrent `attach_context` for the same id
    // racing this prepare cannot land between the existence
    // probe and the insert (which would silently clobber the
    // user's manual context). Prepare losers leave the prior
    // entry untouched.
    let inserted = orch
        .pending_contexts
        .insert_if_absent(calendar_event_id.clone(), context);
    if inserted {
        tracing::info!(
            calendar_event_id = %calendar_event_id,
            bytes,
            "pre-meeting context auto-prepared",
        );
    } else {
        tracing::debug!(
            calendar_event_id = %calendar_event_id,
            "prepare_context: entry already staged, leaving as-is",
        );
    }
    Ok(())
}
