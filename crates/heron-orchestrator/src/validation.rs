//! Cross-endpoint validators for inputs that flow into more than one
//! orchestrator method.
//!
//! `attach_context` and `start_capture` both stamp a
//! `calendar_event_id`; `attach_context` and `prepare_context` both
//! enforce the same on-disk size contract on a `PreMeetingContext`
//! body. Centralising the rules here means a caller padding either
//! side of the id with whitespace can't silently miss the context they
//! themselves attached, and a future persistence layer (or HTTP echo)
//! observes the same byte boundary across paths.

use heron_session::{PreMeetingContext, SessionError};

/// Cap on the calendar event identifier `attach_context` accepts.
/// EventKit ids are short opaque strings and the synthetic ids
/// `list_upcoming_calendar` mints are bounded by `(start, end, title)`
/// — 4 KiB is well past the largest realistic input.
pub(crate) const MAX_CALENDAR_EVENT_ID_BYTES: usize = 4 * 1024;

/// Cap on the JSON-serialized `PreMeetingContext` payload
/// `attach_context` accepts. Spec-shape contexts (agenda, attendees,
/// related notes, briefing) are kilobytes; 256 KiB tolerates a long
/// briefing without letting one caller wedge daemon memory by
/// uploading a megabyte-scale payload per calendar event id.
pub(crate) const MAX_PRE_MEETING_CONTEXT_BYTES: usize = 256 * 1024;

/// Trim and length-validate a `calendar_event_id`. Used by both
/// `attach_context` (where it gates persistence) and `start_capture`
/// (where it gates correlation against the staged map and what gets
/// stamped on `Meeting.calendar_event_id`). Centralising here keeps
/// the trim/cap rules symmetric — without this, a caller padding
/// either side of the id with whitespace would silently miss the
/// context they themselves attached.
pub(crate) fn normalize_calendar_event_id(raw: &str) -> Result<String, SessionError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SessionError::Validation {
            detail: "calendar_event_id must not be empty".to_owned(),
        });
    }
    if trimmed.len() > MAX_CALENDAR_EVENT_ID_BYTES {
        return Err(SessionError::Validation {
            detail: format!("calendar_event_id exceeds {MAX_CALENDAR_EVENT_ID_BYTES} bytes"),
        });
    }
    Ok(trimmed.to_owned())
}

/// Serialize-then-size-check the context body. Shared by
/// `attach_context` and `prepare_context` so the on-disk size
/// contract is enforced uniformly: a future persistence layer (or
/// HTTP echo) observes the same byte boundary, and a non-serializable
/// payload bails before mutating any state. Returns the serialized
/// length so the caller can stamp it on the trace event.
pub(crate) fn validate_context_size(context: &PreMeetingContext) -> Result<usize, SessionError> {
    let serialized = serde_json::to_vec(context).map_err(|e| SessionError::Validation {
        detail: format!("context serialization failed: {e}"),
    })?;
    if serialized.len() > MAX_PRE_MEETING_CONTEXT_BYTES {
        return Err(SessionError::Validation {
            detail: format!("context payload exceeds {MAX_PRE_MEETING_CONTEXT_BYTES} bytes"),
        });
    }
    Ok(serialized.len())
}
