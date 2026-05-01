//! Read-side projection over the in-memory active-capture index and
//! the on-disk vault.
//!
//! Each function here corresponds 1:1 to a `SessionOrchestrator`
//! trait method on `LocalSessionOrchestrator`; the trait impl block
//! in `lib.rs` becomes a thin one-line delegation. Read-side state
//! (the active-meeting map, the finalized-id index, the configured
//! vault root) stays as fields on `LocalSessionOrchestrator` because
//! it is also written by capture — bundling those fields into a
//! read-side struct would force capture to dereference through it
//! for no clarity gain.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use heron_session::{
    AttendeeContext, CalendarEvent, ListMeetingsPage, ListMeetingsQuery, Meeting, MeetingId,
    SessionError, Summary, Transcript, TranscriptLifecycle,
};
use heron_vault::{epoch_seconds_to_utc, read_note};

use crate::collect_active_for_query;
use crate::vault_read::{
    action_items_from_frontmatter, find_note_path_by_id, list_meetings_impl, meeting_from_note,
    read_transcript_segments, resolve_vault_path, started_at_from_frontmatter,
    vault_to_session_err,
};
use crate::{LocalSessionOrchestrator, lock_or_recover};

/// Resolve the on-disk note path for a meeting id, preferring the
/// daemon's in-memory index of finalized meetings (so a `Location`
/// returned by `start_capture` keeps resolving after the background
/// pipeline writes the note) and falling back to a path-derived
/// lookup against the vault.
pub(crate) fn note_path_for_read(
    orch: &LocalSessionOrchestrator,
    vault_root: &Path,
    id: &MeetingId,
) -> Result<PathBuf, SessionError> {
    if let Some(path) = lock_or_recover(&orch.finalized_meetings)
        .get(id)
        .and_then(|m| m.note_path.clone())
    {
        return Ok(path);
    }
    find_note_path_by_id(vault_root, id)
}

pub(crate) async fn list_meetings(
    orch: &LocalSessionOrchestrator,
    q: ListMeetingsQuery,
) -> Result<ListMeetingsPage, SessionError> {
    // Active captures are the live state; finalized vault notes
    // are the disk snapshot. The same `Meeting` is never in both
    // (no vault writer yet, and once one lands the entry is
    // removed from `active_meetings` on `end_meeting` before the
    // note is finalized). Surface active captures only on the
    // first page (cursor=None) — the cursor format is a vault-
    // relative path, so paginating through them would require a
    // synthetic cursor scheme. Active captures are bounded by
    // the singleton-per-platform invariant, so they always fit on
    // page one anyway.
    let active_items = if q.cursor.is_none() {
        collect_active_for_query(&orch.active_meetings, &q)
    } else {
        Vec::new()
    };

    let Some(root) = orch.vault_root.as_deref() else {
        // Without a vault, the only meetings to surface are
        // active ones. If there are none, preserve the substrate-
        // only `NotYetImplemented` behavior so vault-less tests
        // keep their existing surface.
        return if active_items.is_empty() {
            Err(SessionError::NotYetImplemented)
        } else {
            Ok(ListMeetingsPage {
                items: active_items,
                next_cursor: None,
            })
        };
    };

    let mut page = list_meetings_impl(root, q.clone())?;
    // Newest first: active captures predate any cursor-paginated
    // disk results, so prepend then re-apply the limit. The
    // `next_cursor` from the disk scan still points into the disk
    // set — that's fine because active items aren't paginated.
    let limit = q.limit.unwrap_or(50).min(200) as usize;
    let mut combined = active_items;
    combined.extend(page.items);
    if combined.len() > limit {
        combined.truncate(limit);
    }
    page.items = combined;
    Ok(page)
}

pub(crate) async fn get_meeting(
    orch: &LocalSessionOrchestrator,
    id: &MeetingId,
) -> Result<Meeting, SessionError> {
    // Active capture wins — it's the live state, and it's the
    // only thing that exists for a meeting between
    // `start_capture` and the (future) vault note write. Without
    // this short-circuit the `Location: /v1/meetings/{id}` header
    // herond stamps on `POST /meetings` (per the OpenAPI
    // 202-Accepted shape) would dangle into a 404.
    if let Some(active) = lock_or_recover(&orch.active_meetings).get(id) {
        return Ok(active.meeting.clone());
    }
    if let Some(finalized) = lock_or_recover(&orch.finalized_meetings).get(id) {
        return Ok(finalized.meeting.clone());
    }
    let Some(root) = orch.vault_root.as_deref() else {
        return Err(SessionError::NotYetImplemented);
    };
    let path = find_note_path_by_id(root, id)?;
    meeting_from_note(root, &path)
}

pub(crate) async fn read_transcript(
    orch: &LocalSessionOrchestrator,
    id: &MeetingId,
) -> Result<Transcript, SessionError> {
    let Some(root) = orch.vault_root.as_deref() else {
        return Err(SessionError::NotYetImplemented);
    };
    let path = note_path_for_read(orch, root, id)?;
    let (frontmatter, _) = read_note(&path).map_err(vault_to_session_err)?;
    let transcript_path = resolve_vault_path(root, &frontmatter.transcript, "transcript")?;
    let segments = read_transcript_segments(&transcript_path)?;
    Ok(Transcript {
        meeting_id: *id,
        status: TranscriptLifecycle::Complete,
        language: None,
        segments,
    })
}

pub(crate) async fn read_summary(
    orch: &LocalSessionOrchestrator,
    id: &MeetingId,
) -> Result<Option<Summary>, SessionError> {
    let Some(root) = orch.vault_root.as_deref() else {
        return Err(SessionError::NotYetImplemented);
    };
    let path = note_path_for_read(orch, root, id)?;
    let (frontmatter, body) = read_note(&path).map_err(vault_to_session_err)?;
    let action_items = action_items_from_frontmatter(&frontmatter.action_items);
    Ok(Some(Summary {
        meeting_id: *id,
        generated_at: started_at_from_frontmatter(&frontmatter),
        text: body,
        action_items,
        llm_provider: None,
        llm_model: None,
    }))
}

pub(crate) async fn audio_path(
    orch: &LocalSessionOrchestrator,
    id: &MeetingId,
) -> Result<PathBuf, SessionError> {
    let Some(root) = orch.vault_root.as_deref() else {
        return Err(SessionError::NotYetImplemented);
    };
    let path = note_path_for_read(orch, root, id)?;
    let (frontmatter, _) = read_note(&path).map_err(vault_to_session_err)?;
    let recording = resolve_vault_path(root, &frontmatter.recording, "recording")?;
    if !recording.exists() {
        // Don't echo the resolved host path into the wire error
        // — keeps a vault-layout exfil channel closed even on
        // an authenticated request. The meeting id is sufficient
        // for the consumer to act on.
        return Err(SessionError::NotFound {
            what: format!("audio for meeting {id}"),
        });
    }
    Ok(recording)
}

pub(crate) async fn list_upcoming_calendar(
    orch: &LocalSessionOrchestrator,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    limit: Option<u32>,
) -> Result<Vec<CalendarEvent>, SessionError> {
    let now = Utc::now();
    let from = from.unwrap_or(now);
    let to = to.unwrap_or_else(|| from + chrono::Duration::days(7));
    let raw = orch
        .calendar
        .read_window(from, to)
        .map_err(|e| match e {
            heron_vault::CalendarError::Denied => SessionError::PermissionMissing {
                permission: "calendar",
            },
            other => SessionError::VaultLocked {
                detail: format!("calendar read failed: {other}"),
            },
        })?
        .unwrap_or_default();
    let cap = limit.unwrap_or(20).min(100) as usize;
    let events = raw
        .into_iter()
        .take(cap)
        .map(|ev| {
            // EventKit doesn't yet expose a stable per-event id
            // through the Swift bridge; until it does, synthesize
            // a deterministic id from `(start, end, title)` so a
            // future `attach_context` impl can correlate. Long
            // titles are SHA-collision-resistant — `format!` of
            // the raw f64 bits + full title string is enough at
            // this scope; collision-free across realistic vaults.
            let id = format!(
                "synth_{}_{}_{}",
                ev.start.to_bits(),
                ev.end.to_bits(),
                ev.title
            );
            let primed = orch.pending_contexts.contains_key(&id);
            let auto_record = orch.auto_record_registry.contains(&id);
            CalendarEvent {
                id,
                title: ev.title,
                start: epoch_seconds_to_utc(ev.start),
                end: epoch_seconds_to_utc(ev.end),
                attendees: ev
                    .attendees
                    .into_iter()
                    .map(|a| AttendeeContext {
                        name: a.name,
                        email: Some(a.email).filter(|s| !s.is_empty()),
                        last_seen_in: None,
                        relationship: None,
                        notes: None,
                    })
                    .collect(),
                meeting_url: None,
                related_meetings: Vec::new(),
                primed,
                auto_record,
            }
        })
        .collect();
    Ok(events)
}
