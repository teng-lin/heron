//! Vault-backed read model for the orchestrator.
//!
//! The daemon's read endpoints (`list_meetings`, `get_meeting`,
//! `read_transcript`, `read_summary`, `audio_path`) project
//! `<vault>/meetings/*.md` notes into the wire-shape `Meeting` /
//! `TranscriptSegment`. Without a configured vault root every method
//! returns `NotYetImplemented` (the substrate-only behavior); with one,
//! the helpers in this module do the disk scan and the
//! frontmatter-to-wire mapping.
//!
//! All path resolution flows through [`resolve_vault_path`] /
//! [`normalize_no_traverse`] so a malicious or buggy frontmatter
//! cannot read outside the vault root over loopback-auth. The
//! `MeetingId` is derived deterministically from the vault-relative
//! note path via [`derive_meeting_id`] (UUIDv5 over
//! [`MEETING_ID_NAMESPACE`]) so the daemon can answer per-id reads
//! without an in-memory index.

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use heron_session::{
    IdentifierKind, ListMeetingsPage, ListMeetingsQuery, Meeting, MeetingId, MeetingStatus,
    Participant, Platform, SessionError, SummaryLifecycle, TranscriptLifecycle, TranscriptSegment,
};
use heron_vault::{VaultError, read_note};
use uuid::Uuid;

use crate::MAX_TRANSCRIPT_LINE_BYTES;

/// Namespace UUID seeded into [`uuid::Uuid::new_v5`] when deriving
/// a `MeetingId` from a vault-relative note path. The byte pattern
/// is arbitrary but FIXED — changing it would re-key every meeting
/// in every consumer cache and break `Last-Event-ID` resume
/// expectations. If a future change really needs a different
/// derivation, bump it AND emit a synthetic `daemon.error` so
/// consumers know to invalidate their caches.
pub const MEETING_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x68, 0x65, 0x72, 0x6f, 0x6e, 0x6d, 0x74, 0x67, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21,
]);

pub(crate) fn list_meetings_impl(
    vault_root: &Path,
    q: ListMeetingsQuery,
) -> Result<ListMeetingsPage, SessionError> {
    let paths = note_paths_newest_first(vault_root)?;
    let limit = q.limit.unwrap_or(50).min(200) as usize;
    let after = q.cursor.as_deref();
    let mut started_after = after.is_none();
    let mut items = Vec::with_capacity(limit);
    let mut next_cursor: Option<String> = None;
    let mut last_kept_cursor: Option<String> = None;
    let mut listed = Vec::new();
    for path in paths {
        let rel = path
            .strip_prefix(vault_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.clone());
        let rel_str = rel.to_string_lossy().to_string();
        let meeting = match meeting_from_note(vault_root, &path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping malformed note in list_meetings",
                );
                continue;
            }
        };
        if let Some(since) = q.since
            && meeting.started_at < since
        {
            continue;
        }
        if let Some(status) = q.status
            && meeting.status != status
        {
            continue;
        }
        if let Some(platform) = q.platform
            && meeting.platform != platform
        {
            continue;
        }
        listed.push((rel_str, meeting));
    }
    // Pattern-based filenames (`slug.md`, `YYYY-MM-DD-slug.md`, or
    // `<uuid>.md`) are not a reliable chronology. Sort by parsed
    // frontmatter time first, then by relative path for deterministic
    // pagination when two notes share the same minute.
    listed.sort_by(|(a_rel, a), (b_rel, b)| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b_rel.cmp(a_rel))
    });
    for (rel_str, meeting) in listed {
        let cursor = meeting_list_cursor(&rel_str, &meeting);
        if !started_after {
            if Some(cursor.as_str()) == after || Some(rel_str.as_str()) == after {
                started_after = true;
            }
            continue;
        }
        if items.len() == limit {
            next_cursor = last_kept_cursor.clone();
            break;
        }
        items.push(meeting);
        last_kept_cursor = Some(cursor);
    }
    Ok(ListMeetingsPage { items, next_cursor })
}

pub(crate) fn meeting_list_cursor(rel_path: &str, meeting: &Meeting) -> String {
    format!("{}|{rel_path}", meeting.started_at.timestamp_millis())
}

pub(crate) fn note_paths_newest_first(vault_root: &Path) -> Result<Vec<PathBuf>, SessionError> {
    let dir = vault_root.join("meetings");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| SessionError::VaultLocked {
            detail: format!("read_dir({}): {e}", dir.display()),
        })?
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && e.path().extension().and_then(|s| s.to_str()) == Some("md")
        })
        .map(|e| e.path())
        .collect();
    // Deterministic input order only. `list_meetings_impl` sorts by
    // parsed frontmatter time after reading each note.
    entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(entries)
}

/// Linear scan for the note whose derived `MeetingId` matches `id`.
/// Used by every per-meeting read endpoint. Replaceable with an
/// in-memory index when capture lifecycle ships and the bus starts
/// publishing events (the index is the natural piggyback on the
/// recorder).
pub(crate) fn find_note_path_by_id(
    vault_root: &Path,
    id: &MeetingId,
) -> Result<PathBuf, SessionError> {
    note_paths_newest_first(vault_root)?
        .into_iter()
        .find(|p| derive_meeting_id(vault_root, p) == *id)
        .ok_or_else(|| SessionError::NotFound {
            what: format!("meeting {id}"),
        })
}

/// Resolve a frontmatter path field against the vault root,
/// rejecting absolute paths and `..` traversal. Without this
/// `read_transcript` and `audio_path` are file-read primitives over
/// loopback-auth.
pub(crate) fn resolve_vault_path(
    vault_root: &Path,
    candidate: &Path,
    field: &'static str,
) -> Result<PathBuf, SessionError> {
    if candidate.is_absolute() {
        return Err(SessionError::Validation {
            detail: format!("{field} path must be vault-relative"),
        });
    }
    // Canonicalize the vault root FIRST so the prefix check below
    // compares apples to apples — on macOS, `/var/...` canonicalizes
    // to `/private/var/...` (system symlink). Without this, a non-
    // canonical vault_root + non-canonical candidate would fail the
    // canonical prefix check, mistakenly rejecting a perfectly-
    // relative path.
    let root_canonical = vault_root
        .canonicalize()
        .unwrap_or_else(|_| vault_root.to_path_buf());
    let safe_relative = normalize_no_traverse(candidate)?;
    let joined = root_canonical.join(&safe_relative);
    let resolved = if joined.exists() {
        joined
            .canonicalize()
            .map_err(|e| SessionError::VaultLocked {
                detail: format!("canonicalize {field}: {e}"),
            })?
    } else {
        joined
    };
    if !resolved.starts_with(&root_canonical) {
        return Err(SessionError::Validation {
            detail: format!("{field} path escapes vault"),
        });
    }
    Ok(resolved)
}

fn normalize_no_traverse(path: &Path) -> Result<PathBuf, SessionError> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                return Err(SessionError::Validation {
                    detail: "path contains '..' which is forbidden".to_owned(),
                });
            }
            Component::Normal(_)
            | Component::RootDir
            | Component::Prefix(_)
            | Component::CurDir => {
                out.push(c.as_os_str());
            }
        }
    }
    Ok(out)
}

pub(crate) fn derive_meeting_id(vault_root: &Path, note_path: &Path) -> MeetingId {
    let rel = note_path.strip_prefix(vault_root).unwrap_or(note_path);
    let bytes = rel.as_os_str().as_encoded_bytes();
    MeetingId(Uuid::new_v5(&MEETING_ID_NAMESPACE, bytes))
}

pub(crate) fn meeting_from_note(vault_root: &Path, path: &Path) -> Result<Meeting, SessionError> {
    let (fm, body) = read_note(path).map_err(vault_to_session_err)?;
    let id = derive_meeting_id(vault_root, path);
    let started_at = started_at_from_frontmatter(&fm);
    let ended_at = Some(started_at + chrono::Duration::minutes(fm.duration_min as i64));
    let participants = fm
        .attendees
        .iter()
        .map(|a| Participant {
            display_name: a.name.clone(),
            identifier_kind: IdentifierKind::Fallback,
            is_user: false,
        })
        .collect();
    let transcript_resolved = resolve_vault_path(vault_root, &fm.transcript, "transcript").ok();
    let transcript_status = match transcript_resolved {
        Some(p) if p.exists() => TranscriptLifecycle::Complete,
        _ => TranscriptLifecycle::Failed,
    };
    let summary_status = if body.trim().is_empty() {
        SummaryLifecycle::Pending
    } else {
        SummaryLifecycle::Ready
    };
    let processing = meeting_processing_from_cost(&fm.cost);
    let action_items = action_items_from_frontmatter(&fm.action_items);
    Ok(Meeting {
        id,
        // Notes are only finalized for completed meetings, so the
        // status is always `Done`. A meeting still in `Recording`
        // doesn't have a finalized note on disk for us to surface.
        status: MeetingStatus::Done,
        platform: platform_from_source_app(&fm.source_app),
        title: fm.company.clone(),
        calendar_event_id: None,
        started_at,
        ended_at,
        duration_secs: Some((fm.duration_min as u64) * 60),
        participants,
        transcript_status,
        summary_status,
        // Surface LLM-inferred tags so the frontend can render chips
        // without a second read into the note's frontmatter.
        tags: fm.tags.clone(),
        processing,
        action_items,
    })
}

/// Project `Frontmatter.cost` into the wire `MeetingProcessing`.
///
/// Returns `None` when the cost looks unpopulated — the integration
/// test fixtures and pre-Tier-0-#2 vault notes wrote zero tokens and
/// an empty model string, which the desktop "Processing" panel can't
/// render usefully ("Summarized by ", "Tokens in: 0"). Treating that
/// shape as `None` keeps the panel hidden until a real summarize has
/// run, rather than rendering a misleading "$0.00 by `<empty>`" row.
///
/// All-zero-but-real-model and all-real-but-zero-tokens are
/// vanishingly unlikely (the summarizer always pays for at least the
/// system prompt), but we still surface them as `Some` — the
/// emptiness gate is the conjunction, so a real-but-cheap call is
/// honestly reported.
fn meeting_processing_from_cost(
    cost: &heron_types::Cost,
) -> Option<heron_session::MeetingProcessing> {
    if cost.model.is_empty()
        && cost.tokens_in == 0
        && cost.tokens_out == 0
        && cost.summary_usd == 0.0
    {
        None
    } else {
        Some(heron_session::MeetingProcessing {
            summary_usd: cost.summary_usd,
            tokens_in: cost.tokens_in,
            tokens_out: cost.tokens_out,
            model: cost.model.clone(),
        })
    }
}

/// Project `Frontmatter.action_items` (typed rows with stable
/// [`heron_types::ItemId`]) into the wire `heron_session::ActionItem`.
///
/// Tier 0 #3 of the UX redesign: surface structured rows on the
/// `Meeting` and `Summary` IPC types so the desktop's Review tab can
/// render assignees + due dates without re-parsing the markdown body
/// with a bullet-extracting regex. Read path only — write-back stays
/// markdown-flavoured for now (`docs/ux-redesign-backend-prerequisites.md`).
///
/// Empty `owner` strings on disk become `None` on the wire — the
/// vault writer materializes an empty string when the LLM emitted no
/// owner, but the wire type is "owner is optional," so `""` is the
/// honest projection of "no owner."
pub(crate) fn action_items_from_frontmatter(
    items: &[heron_types::ActionItem],
) -> Vec<heron_session::ActionItem> {
    items
        .iter()
        .map(|a| heron_session::ActionItem {
            id: a.id,
            text: a.text.clone(),
            owner: (!a.owner.is_empty()).then(|| a.owner.clone()),
            due: a.due.as_deref().and_then(parse_iso_date),
        })
        .collect()
}

pub(crate) fn platform_from_source_app(source_app: &str) -> Platform {
    let s = source_app.to_ascii_lowercase();
    if s.contains("zoom") {
        Platform::Zoom
    } else if s.contains("meet.google") || s.contains("googlemeet") || s.contains("google_meet") {
        Platform::GoogleMeet
    } else if s.contains("teams") || s.contains("microsoft") {
        Platform::MicrosoftTeams
    } else if s.contains("webex") {
        Platform::Webex
    } else {
        if !source_app.is_empty() {
            tracing::warn!(
                source_app,
                "unrecognized source_app; defaulting to Platform::Zoom"
            );
        }
        Platform::Zoom
    }
}

pub(crate) fn platform_from_meeting_url(meeting_url: Option<&str>) -> Option<Platform> {
    let url = meeting_url?.to_ascii_lowercase();
    if url.contains("zoom.us") || url.contains("zoomgov.com") {
        Some(Platform::Zoom)
    } else if url.contains("meet.google.com") {
        Some(Platform::GoogleMeet)
    } else if url.contains("teams.microsoft.com") || url.contains("teams.live.com") {
        Some(Platform::MicrosoftTeams)
    } else if url.contains("webex.com") {
        Some(Platform::Webex)
    } else {
        None
    }
}

pub(crate) fn started_at_from_frontmatter(fm: &heron_types::Frontmatter) -> DateTime<Utc> {
    let date: NaiveDate = fm.date;
    let time = NaiveTime::parse_from_str(&fm.start, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(&fm.start, "%H:%M:%S"))
        .unwrap_or_else(|_| NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default());
    let naive = date.and_time(time);
    // Frontmatter has no explicit timezone field. The vault writer
    // records meetings in the user's local clock (the
    // `YYYY-MM-DD-HHMM` filename matches the user's wall clock at
    // capture time), so the API contract is "local time projected
    // to UTC." Earliest mapping wins on the autumn DST overlap;
    // the gap (spring) falls back to naive-as-UTC with a warn so a
    // single missing-hour frontmatter doesn't fail the whole list.
    use chrono::Local;
    use chrono::offset::LocalResult;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(local) => local.with_timezone(&Utc),
        LocalResult::Ambiguous(earliest, _latest) => earliest.with_timezone(&Utc),
        LocalResult::None => {
            tracing::warn!(
                date = %fm.date,
                start = %fm.start,
                "frontmatter datetime in DST gap; treating naive value as UTC",
            );
            Utc.from_utc_datetime(&naive)
        }
    }
}

pub(crate) fn read_transcript_segments(
    path: &Path,
) -> Result<Vec<TranscriptSegment>, SessionError> {
    use std::io::{BufRead, Read};
    if !path.exists() {
        return Err(SessionError::NotFound {
            what: format!("transcript file: {}", path.display()),
        });
    }
    let file = std::fs::File::open(path).map_err(|e| SessionError::VaultLocked {
        detail: format!("open transcript {}: {e}", path.display()),
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut segments = Vec::new();
    let mut lineno = 0usize;
    loop {
        let mut buf = Vec::with_capacity(256);
        // Cap each read at MAX_TRANSCRIPT_LINE_BYTES so a malformed
        // transcript without newlines can't pull the whole file
        // into one allocation. Lines longer than the cap are
        // warn-skipped — corrupt entries don't stall the rest.
        let n = (&mut reader)
            .take(MAX_TRANSCRIPT_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)
            .map_err(|e| SessionError::VaultLocked {
                detail: format!("read transcript line {lineno}: {e}"),
            })?;
        if n == 0 {
            break;
        }
        if n > MAX_TRANSCRIPT_LINE_BYTES {
            tracing::warn!(
                line = lineno,
                bytes = n,
                "transcript line exceeds MAX_TRANSCRIPT_LINE_BYTES; skipping",
            );
            buf.clear();
            let _ = reader.read_until(b'\n', &mut buf);
            lineno += 1;
            continue;
        }
        let line = match std::str::from_utf8(&buf) {
            Ok(s) => s.trim_end_matches('\n').trim_end_matches('\r').to_owned(),
            Err(_) => {
                tracing::warn!(line = lineno, "non-utf8 transcript line; skipping");
                lineno += 1;
                continue;
            }
        };
        if line.trim().is_empty() {
            lineno += 1;
            continue;
        }
        match serde_json::from_str::<heron_types::Turn>(&line) {
            Ok(turn) => {
                let is_user = matches!(turn.speaker_source, heron_types::SpeakerSource::Self_);
                let identifier_kind = match turn.speaker_source {
                    heron_types::SpeakerSource::Self_ => IdentifierKind::Mic,
                    heron_types::SpeakerSource::Ax => IdentifierKind::AxTree,
                    heron_types::SpeakerSource::Channel => IdentifierKind::Fallback,
                    heron_types::SpeakerSource::Cluster => IdentifierKind::Fallback,
                };
                let confidence = match turn.confidence {
                    Some(c) if c >= 0.7 => heron_session::Confidence::High,
                    _ => heron_session::Confidence::Low,
                };
                segments.push(TranscriptSegment {
                    speaker: Participant {
                        display_name: turn.speaker,
                        identifier_kind,
                        is_user,
                    },
                    text: turn.text,
                    start_secs: turn.t0,
                    end_secs: turn.t1,
                    confidence,
                    is_final: true,
                });
            }
            Err(e) => {
                tracing::warn!(line = lineno, error = %e, "skipping malformed turn");
            }
        }
        lineno += 1;
    }
    Ok(segments)
}

pub(crate) fn vault_to_session_err(err: VaultError) -> SessionError {
    match err {
        VaultError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => SessionError::NotFound {
            what: format!("vault file io: {e}"),
        },
        other => SessionError::VaultLocked {
            detail: other.to_string(),
        },
    }
}

fn parse_iso_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}
