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
    let read_dir = std::fs::read_dir(&dir).map_err(|e| SessionError::VaultLocked {
        detail: format!("read_dir({}): {e}", dir.display()),
    })?;
    // Surface per-entry IO errors instead of silently dropping them.
    // Permission-denied or inode-corruption on a single entry now
    // fails the whole listing rather than presenting a partial view
    // that looks like "the meeting just isn't there." (Issue #215
    // finding 5 — was `filter_map(Result::ok) + unwrap_or(false)`.)
    //
    // Filter on `file_name()` (no allocation) before paying for
    // `entry.path()` so non-`.md` siblings — `.tmp` writer scratch
    // files, hidden `.DS_Store`, etc. — don't allocate. (PR #228
    // review, gemini.)
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| SessionError::VaultLocked {
            detail: format!("read_dir entry({}): {e}", dir.display()),
        })?;
        let file_name = entry.file_name();
        if Path::new(&file_name).extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let file_type = entry.file_type().map_err(|e| SessionError::VaultLocked {
            detail: format!("file_type({}): {e}", entry.path().display()),
        })?;
        if file_type.is_file() {
            entries.push(entry.path());
        }
    }
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

    // Issue #215 finding 1 — symlink directory escape. `starts_with`
    // on canonical paths is a necessary but not sufficient check on
    // its own: a single symlink along `safe_relative` whose target
    // is outside `root_canonical` would canonicalize fine and pass
    // the prefix check on some edge-case configurations (and even
    // on platforms where canonicalize is robust, leaving this
    // implicit invites future drift). Walk every joined component
    // that already exists and reject any that is a symlink. The
    // vault root itself is canonicalized above, so a user who
    // intentionally symlinks their whole vault is unaffected — only
    // intra-vault symlinks (which Obsidian itself does not create)
    // are rejected.
    reject_symlinked_components(&root_canonical, &joined, field)?;

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

/// Walk `joined`'s components below `root_canonical` and refuse any
/// existing component that is itself a symlink. We check
/// `symlink_metadata` (does NOT follow links) so a symlink whose
/// target is inside the vault is still rejected — by construction,
/// the safe answer is "no symlinks below the vault root," because
/// there's no way to tell at validation time whether a symlink will
/// be redirected later (TOCTOU on the resolve→open path).
fn reject_symlinked_components(
    root_canonical: &Path,
    joined: &Path,
    field: &'static str,
) -> Result<(), SessionError> {
    let Ok(rel) = joined.strip_prefix(root_canonical) else {
        // The joined path is outside the canonical root; the
        // downstream `starts_with` check will reject it. Nothing to
        // walk.
        return Ok(());
    };
    let mut current = root_canonical.to_path_buf();
    for component in rel.components() {
        current.push(component);
        match current.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(SessionError::Validation {
                    detail: format!("{field} path traverses a symlink"),
                });
            }
            Ok(_) => {}
            // ENOENT past this point is fine — a not-yet-existing
            // leaf (e.g. a transcript pointer for a meeting whose
            // file hasn't been written) is the documented "Failed"
            // status branch upstream.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return Err(SessionError::VaultLocked {
                    detail: format!("symlink_metadata {field}: {e}"),
                });
            }
        }
    }
    Ok(())
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
    // Issue #215 finding 3 — buffer is reused across iterations so
    // we don't re-allocate per line. `read_until` appends, so each
    // iteration starts with `buf.clear()` to drop the previous
    // line's bytes (the underlying capacity is retained).
    let mut buf = Vec::with_capacity(256);
    loop {
        buf.clear();
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
            // Issue #215 finding 2 — drain via `fill_buf` /
            // `consume` so the rest of the over-cap line never
            // touches an allocation. The pre-fix path called
            // `read_until` after `buf.clear()`, which would happily
            // grow `buf` to hold every byte up to the next newline
            // (or EOF) — a malformed transcript without newlines
            // could OOM the daemon.
            //
            // Skip the drain when `read_until` already consumed the
            // terminating newline (the pathological case of a line
            // exactly `MAX_TRANSCRIPT_LINE_BYTES + 1` bytes long
            // including its `\n`). Without this guard the drain
            // would `consume` the start of the *next* valid line.
            // (PR #228 review, gemini.)
            if buf.last() != Some(&b'\n') {
                loop {
                    let (done, used) = {
                        let available =
                            reader.fill_buf().map_err(|e| SessionError::VaultLocked {
                                detail: format!("drain transcript line {lineno}: {e}"),
                            })?;
                        if available.is_empty() {
                            (true, 0)
                        } else if let Some(i) = available.iter().position(|&b| b == b'\n') {
                            (true, i + 1)
                        } else {
                            (false, available.len())
                        }
                    };
                    reader.consume(used);
                    if done {
                        break;
                    }
                }
            }
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
        // Issue #215 finding 6 — don't echo the host filesystem path
        // into the wire `NotFound` error. `audio_path` already
        // follows this pattern (see `lib.rs`); now `read_summary` /
        // `read_transcript` do too. Auth on the daemon is loopback
        // only, but exfiling the user's vault layout via error
        // strings is a leak we can close cheaply.
        VaultError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => SessionError::NotFound {
            what: "vault file".to_owned(),
        },
        other => SessionError::VaultLocked {
            detail: other.to_string(),
        },
    }
}

fn parse_iso_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Issue #215 hardening tests.
    //!
    //! Each `#[test]` here pins one of the six findings the
    //! `gemini-code-assist` / `coderabbitai` review on PR #214
    //! flagged so a future refactor can't silently regress them.
    //! Symlink tests are gated `#[cfg(unix)]` because Windows
    //! requires a privileged token to create symlinks; the rest run
    //! cross-platform.
    use super::*;

    /// Finding 1 — `resolve_vault_path` must reject a frontmatter
    /// path that walks through a symlink whose target escapes the
    /// vault. This is the gemini-flagged "starts_with-on-canonical
    /// is necessary but not sufficient" case.
    #[cfg(unix)]
    #[test]
    fn resolve_vault_path_rejects_symlink_escape() {
        let outside = tempfile::tempdir().expect("outside tempdir");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "stolen").unwrap();

        let vault = tempfile::tempdir().expect("vault tempdir");
        std::fs::create_dir_all(vault.path().join("transcripts")).unwrap();
        // The leaf is a symlink whose target lives outside the vault.
        // A naive `canonicalize().starts_with(root)` check is the
        // *primary* defense, but on edge-case filesystem
        // configurations canonicalization can fail to follow the
        // link (we've also seen drift where canonicalize behavior
        // differs across platforms). The hardened path rejects the
        // symlink at the metadata layer first.
        std::os::unix::fs::symlink(&secret, vault.path().join("transcripts/evil.jsonl")).unwrap();

        let err = resolve_vault_path(
            vault.path(),
            Path::new("transcripts/evil.jsonl"),
            "transcript",
        )
        .expect_err("must reject symlinked path");
        assert!(
            matches!(err, SessionError::Validation { .. }),
            "expected Validation error, got {err:?}",
        );
    }

    /// Finding 1 — also reject an *intermediate* directory symlink.
    /// A user note pointing at `transcripts/foo.jsonl` where
    /// `transcripts/` itself is a symlink to `/tmp/...` must not
    /// open the file.
    #[cfg(unix)]
    #[test]
    fn resolve_vault_path_rejects_intermediate_dir_symlink() {
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::fs::create_dir_all(outside.path().join("transcripts")).unwrap();
        std::fs::write(outside.path().join("transcripts/foo.jsonl"), "[]").unwrap();

        let vault = tempfile::tempdir().expect("vault tempdir");
        std::os::unix::fs::symlink(
            outside.path().join("transcripts"),
            vault.path().join("transcripts"),
        )
        .unwrap();

        let err = resolve_vault_path(
            vault.path(),
            Path::new("transcripts/foo.jsonl"),
            "transcript",
        )
        .expect_err("must reject intermediate symlink");
        assert!(
            matches!(err, SessionError::Validation { .. }),
            "expected Validation error, got {err:?}",
        );
    }

    /// Finding 1 — a vault root that is itself a symlink (a common
    /// Obsidian setup: vault is a symlink into iCloud Drive) must
    /// keep working. Only intra-vault symlinks are rejected.
    #[cfg(unix)]
    #[test]
    fn resolve_vault_path_allows_symlinked_vault_root() {
        let real = tempfile::tempdir().expect("real tempdir");
        std::fs::create_dir_all(real.path().join("transcripts")).unwrap();
        std::fs::write(real.path().join("transcripts/ok.jsonl"), "[]").unwrap();

        let alias_parent = tempfile::tempdir().expect("alias parent");
        let alias_root = alias_parent.path().join("vault");
        std::os::unix::fs::symlink(real.path(), &alias_root).unwrap();

        let resolved =
            resolve_vault_path(&alias_root, Path::new("transcripts/ok.jsonl"), "transcript")
                .expect("symlinked vault root must still resolve");
        assert!(resolved.ends_with("transcripts/ok.jsonl"));
    }

    /// Finding 2 — a transcript file with no newlines longer than
    /// `MAX_TRANSCRIPT_LINE_BYTES` must NOT pull the whole malformed
    /// payload into a single buffer (OOM). The pre-fix drain path
    /// did `buf.clear(); reader.read_until(b'\n', &mut buf);` which
    /// would happily grow `buf` until EOF.
    ///
    /// The test writes a file that's twice the cap with NO newlines
    /// at all, then asserts that:
    ///   - the call returns successfully (no OOM, no error),
    ///   - the returned `segments` is empty (the over-cap line was
    ///     warn-skipped per the documented contract).
    ///
    /// We can't directly assert on peak allocation in stable Rust,
    /// but the behavior contract — "doesn't grow `buf` past the
    /// cap" — is what the production code's `fill_buf`/`consume`
    /// drain enforces. If the fix regresses to `read_until` we'd
    /// expect this test to either OOM or silently allocate the full
    /// 2 MiB; both signal a problem.
    #[test]
    fn read_transcript_segments_caps_oversized_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("malformed.jsonl");
        // 2 MiB of `x` with no newlines, then a final newline. The
        // cap is 1 MiB, so this is the over-cap branch.
        let bulk_size = MAX_TRANSCRIPT_LINE_BYTES * 2;
        let mut payload = vec![b'x'; bulk_size];
        payload.push(b'\n');
        // Append a real, valid turn after the over-cap line to
        // confirm the reader recovers and keeps streaming. The
        // `channel` / `speaker_source` strings match the snake-case
        // serde rename on `heron_types::Turn`.
        payload.extend_from_slice(
            br#"{"t0":0.0,"t1":1.0,"text":"hi","channel":"mic","speaker":"Ada","speaker_source":"self","confidence":1.0}"#,
        );
        payload.push(b'\n');
        std::fs::write(&path, payload).unwrap();

        let segs = read_transcript_segments(&path).expect("must not OOM");
        assert_eq!(
            segs.len(),
            1,
            "over-cap line skipped, valid turn after still parsed",
        );
        assert_eq!(segs[0].speaker.display_name, "Ada");
    }

    /// Finding 2 — corner case caught by gemini on PR #228. When an
    /// over-cap line is *exactly* `MAX_TRANSCRIPT_LINE_BYTES + 1`
    /// bytes long INCLUDING its terminating `\n`, `read_until` has
    /// already consumed the newline. A naive drain that always runs
    /// would `consume` the start of the next valid turn — silently
    /// eating it.
    ///
    /// The fix gates the drain on `buf.last() != Some(&b'\n')`. If
    /// regressed, this test would see 0 segments instead of 1.
    #[test]
    fn read_transcript_segments_oversize_line_with_trailing_newline_does_not_eat_next() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge.jsonl");
        // Build a line whose total length (content + `\n`) is
        // exactly MAX + 1 — i.e. content is MAX bytes, plus `\n`.
        let mut payload = vec![b'x'; MAX_TRANSCRIPT_LINE_BYTES];
        payload.push(b'\n');
        // Followed by a real, parseable turn that the reader MUST
        // surface (regression check for the drain-eats-next bug).
        payload.extend_from_slice(
            br#"{"t0":0.0,"t1":1.0,"text":"hi","channel":"mic","speaker":"Ada","speaker_source":"self","confidence":1.0}"#,
        );
        payload.push(b'\n');
        std::fs::write(&path, payload).unwrap();

        let segs = read_transcript_segments(&path).expect("must not error");
        assert_eq!(
            segs.len(),
            1,
            "drain must not consume the next line when over-cap line ended in `\\n`",
        );
        assert_eq!(segs[0].speaker.display_name, "Ada");
    }

    /// Finding 5 — `note_paths_newest_first` must surface a real
    /// IO failure rather than swallowing it. The legacy
    /// `filter_map(Result::ok) + unwrap_or(false)` path made
    /// permission-denied look like "no notes here," which is a
    /// silent partial-success that's hard to debug.
    ///
    /// We trigger the failure by chmod-ing the meetings/ dir to
    /// non-readable on Unix. Skipped on Windows (different ACL
    /// model) and skipped when running as root (chmod doesn't
    /// restrict root).
    #[cfg(unix)]
    #[test]
    fn note_paths_surfaces_read_dir_errors() {
        use std::os::unix::fs::PermissionsExt;

        // RAII guard so a panic between chmod-down and chmod-up
        // still restores perms (otherwise the tempdir cleanup trips
        // EACCES and the failure cascades into a misleading Drop
        // panic). PR #228 review, gemini.
        struct PermGuard(PathBuf);
        impl Drop for PermGuard {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }

        let vault = tempfile::tempdir().expect("vault tempdir");
        let meetings = vault.path().join("meetings");
        std::fs::create_dir_all(&meetings).unwrap();
        // Drop all perms so `read_dir` returns Err on a non-root
        // user. We confirm the chmod actually denied the calling
        // process below — if it didn't (e.g. running as root, or a
        // filesystem that ignores POSIX modes), skip the assertion
        // rather than fail spuriously.
        std::fs::set_permissions(&meetings, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _guard = PermGuard(meetings.clone());
        let chmod_works = std::fs::read_dir(&meetings).is_err();

        let result = note_paths_newest_first(vault.path());

        if !chmod_works {
            // Running as root or on a permissive FS — the partial
            // -success regression we're guarding against can't be
            // reproduced here. Don't assert.
            return;
        }
        let err = result.expect_err("must surface read_dir failure");
        assert!(
            matches!(err, SessionError::VaultLocked { .. }),
            "expected VaultLocked, got {err:?}",
        );
    }

    /// Finding 6 — `vault_to_session_err` mapping a NotFound IO
    /// error must NOT include the host filesystem path in the
    /// surfaced wire error. Path leaking via error strings is the
    /// asymmetry CodeRabbit flagged against `audio_path`'s
    /// already-correct phrasing.
    #[test]
    fn vault_to_session_err_does_not_leak_path() {
        let host_path = "/Users/teng-lin/secret/vault/meetings/note.md";
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, host_path);
        let mapped = vault_to_session_err(VaultError::Io(io_err));
        let SessionError::NotFound { what } = mapped else {
            panic!("expected NotFound, got {mapped:?}");
        };
        assert!(
            !what.contains(host_path),
            "wire error must not echo host path; got {what:?}",
        );
        assert!(
            !what.contains("/Users/"),
            "wire error must not echo any user dir; got {what:?}",
        );
    }
}
