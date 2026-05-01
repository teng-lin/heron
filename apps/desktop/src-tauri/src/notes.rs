//! Note-file backend used by the Review UI (PR-γ, phase 65).
//!
//! Three small async functions — read a `.md`, write it atomically, and
//! list the `.md` filenames in a vault. The frontend wraps each as a
//! Tauri command (`heron_read_note`, `heron_write_note_atomic`,
//! `heron_list_sessions`).
//!
//! ## Why a dedicated module instead of reusing `heron-vault`
//!
//! `heron_vault::atomic_write` (which the desktop crate already pulls in
//! for `onboarding.rs`) is sync and operates on `&[u8]`. The Review UI
//! needs an `async` path so editor blur / ⌘S doesn't block on the
//! webview-bridge thread, plus the input is `&str` (UTF-8 markdown).
//! Rather than wrap the sync helper in `spawn_blocking` we mirror the
//! recipe — write to a sibling temp file, fsync, set `0600`, rename —
//! against `tokio::fs` directly. Same atomicity guarantees, no thread
//! pool ceremony.
//!
//! ## Path policy
//!
//! All three commands route through [`resolve_note_path`] /
//! [`resolve_vault_path`], which canonicalize the input and reject
//! paths that escape the configured vault. The renderer can supply
//! any string, but only `<vault>/meetings/<basename>.md` (no
//! traversal, no symlink-out, basename matches `[A-Za-z0-9._-]+`
//! after stripping the wire-form `mtg_` prefix) reaches the
//! filesystem. The `meetings/` subdirectory matches the layout
//! `heron_vault::VaultWriter::finalize_with_pattern` writes into.
//! Without this, a route bug or compromised webview would have
//! arbitrary local-file capability.
//!
//! ## Wire-id resolution (issue #205)
//!
//! [`resolve_note_path`] handles three input shapes:
//!
//! 1. A bare basename produced by [`list_sessions`] / the sidebar
//!    (e.g. `2026-04-26-standup`). Strip-and-join finds the file
//!    directly — basename matches the on-disk filename for every
//!    [`heron_vault::FileNamingPattern`] variant.
//! 2. A wire id `mtg_<uuid>` for a [`heron_vault::FileNamingPattern::Id`]-
//!    written note. The strip discards the prefix and the bare uuid
//!    is the on-disk basename, so step 1's strip-and-join finds the
//!    file.
//! 3. A wire id `mtg_<uuid>` for a `Slug` / `DateSlug`-written note.
//!    The on-disk file is `<slug>.md` / `<YYYY-MM-DD>-<slug>.md` —
//!    *not* the bare-uuid basename. When step 2's strip-and-join
//!    misses, [`find_note_path_by_wire_id`] scans `<vault>/meetings`
//!    and reverse-looks-up the file whose path-derived `MeetingId`
//!    (UUIDv5 over the vault-relative bytes, matching
//!    `heron_orchestrator::vault_read::derive_meeting_id`) equals
//!    the wire id's UUID. The same derivation the daemon uses to
//!    answer per-id reads — so the renderer's read / write hits the
//!    exact note the daemon's `Meeting.id` points at.
//!
//! The scan is `O(notes_in_vault)` per resolution. For a typical user
//! vault (hundreds to low-thousands of notes) and a one-off-per-Review-
//! page-load access pattern, that's well under the IO floor of the
//! `read_to_string` it gates. If a future hot path needs to resolve
//! every meeting in one render, hoist the scan into a per-vault index
//! (the orchestrator builds the same index on its read endpoints).
//!
//! Errors surface as `String` to match the existing `lib.rs` pattern
//! (`AssetError::to_string`, `SettingsError::to_string`) — the React
//! tree shows them in a Sonner toast and doesn't need a typed error.

use std::path::{Path, PathBuf};

use heron_orchestrator::MEETING_ID_NAMESPACE;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use uuid::{NoContext, Timestamp, Uuid};

/// Validate a session id supplied by the renderer.
///
/// We accept ASCII alphanumerics plus `_ - .`, which covers Heron's
/// `YYYY-MM-DD-meeting-name` filenames and any reasonable variant a
/// future synthesizer might emit. We explicitly reject:
/// - empty strings (would resolve to the vault root with `.md`)
/// - `.` and `..` (parent-dir escape)
/// - any `/` or `\` (path-component escape)
/// - leading `.` (would create a hidden file the sidebar filters out)
///
/// This is a basename policy, not a full path check —
/// [`resolve_note_path`] canonicalizes and re-checks against the vault
/// after this filter passes.
pub(crate) fn validate_session_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("session id is empty".to_string());
    }
    if id == "." || id == ".." {
        return Err(format!("session id '{id}' is reserved"));
    }
    if id.starts_with('.') {
        return Err(format!("session id '{id}' starts with '.'"));
    }
    for ch in id.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.';
        if !ok {
            return Err(format!("session id '{id}' contains '{ch}'"));
        }
    }
    Ok(())
}

/// Canonicalize `vault` and ensure it points at a directory.
///
/// Splitting this out of [`resolve_note_path`] lets `list_sessions`
/// reuse the same vetting logic — a renderer asking us to list
/// `/etc` is just as much a footgun as one asking us to write there.
pub(crate) async fn resolve_vault_path(vault: &Path) -> Result<PathBuf, String> {
    if vault.as_os_str().is_empty() {
        return Err("vault path is empty".to_string());
    }
    let canonical = fs::canonicalize(vault)
        .await
        .map_err(|e| format!("canonicalize {}: {}", vault.display(), e))?;
    let meta = fs::metadata(&canonical)
        .await
        .map_err(|e| format!("stat {}: {}", canonical.display(), e))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    Ok(canonical)
}

/// `<vault>/meetings` — the directory `heron_vault::VaultWriter::finalize_with_pattern`
/// writes finalized session notes into. Kept in one place so the
/// renderer's read/write path can never drift from the writer's.
pub(crate) fn meetings_dir(vault: &Path) -> PathBuf {
    vault.join("meetings")
}

/// Canonicalize `meetings` (assumed to be `<canonical_vault>/meetings`)
/// and verify it still lives inside `canonical_vault`. Returns
/// `Ok(None)` if the directory doesn't exist (pre-capture state),
/// `Ok(Some(canonical))` on success, `Err` for IO errors or symlink
/// escapes. Single source of truth for the `meetings/` containment
/// check shared by [`resolve_note_path`] (write), `list_sessions`,
/// and `resummarize::resolve_bak_path`.
pub(crate) async fn canonicalize_meetings_within(
    meetings: &Path,
    canonical_vault: &Path,
) -> Result<Option<PathBuf>, String> {
    match fs::canonicalize(meetings).await {
        Ok(c) => {
            if !c.starts_with(canonical_vault) {
                return Err(format!(
                    "meetings dir {} escapes vault {}",
                    c.display(),
                    canonical_vault.display()
                ));
            }
            Ok(Some(c))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("canonicalize {}: {}", meetings.display(), e)),
    }
}

/// Strip the `mtg_` wire prefix from a meeting id, leaving the bare
/// uuid the writer used as the basename for `FileNamingPattern::Id`.
/// See the module-level "Path policy" docstring for the
/// `Slug` / `DateSlug` caveat.
pub(crate) fn note_basename(session_id: &str) -> &str {
    session_id.strip_prefix("mtg_").unwrap_or(session_id)
}

/// Validate `session_id`, strip its `mtg_` wire prefix, and re-validate
/// the result. The double-check is load-bearing: a clever id like
/// `mtg_..` passes the first call (the literal `..` rule only matches
/// the whole string) but the post-strip basename `..` must not reach
/// the filesystem. Used by both `resolve_note_path` (`.md`) and
/// `resummarize::resolve_bak_path` (`.md.bak`) so the two surfaces stay
/// in lock-step.
pub(crate) fn validated_basename(session_id: &str) -> Result<&str, String> {
    validate_session_id(session_id)?;
    let basename = note_basename(session_id);
    validate_session_id(basename)?;
    Ok(basename)
}

/// Resolve `<vault>/meetings/<basename>.md` (where `<basename>` strips
/// any `mtg_` prefix from `session_id`) and confirm the result is
/// inside the canonical vault — no symlink escapes, no `..` shenanigans.
///
/// Returns the path the renderer is allowed to read/write. The
/// canonicalize step requires the file to exist on read; for write we
/// canonicalize the *parent* and re-attach the basename so a brand-new
/// note still passes.
///
/// **Wire-id fallback (issue #205):** if the strip-derived basename
/// has no `.md` on disk and `session_id` is a `mtg_<uuid>` wire id,
/// scans `meetings/` for a file whose path-derived `MeetingId` matches
/// the wire id. This is the path that resolves `Slug` / `DateSlug`
/// captures the daemon surfaced via `Meeting.id`. Module-level docs
/// have the full input-shape table.
pub(crate) async fn resolve_note_path(
    vault: &Path,
    session_id: &str,
    must_exist: bool,
) -> Result<PathBuf, String> {
    let basename = validated_basename(session_id)?;
    let canonical_vault = resolve_vault_path(vault).await?;
    let meetings = meetings_dir(&canonical_vault);
    let candidate = meetings.join(format!("{basename}.md"));

    if must_exist {
        match fs::canonicalize(&candidate).await {
            Ok(canonical) => {
                if !canonical.starts_with(&canonical_vault) {
                    return Err(format!(
                        "resolved path {} escapes vault {}",
                        canonical.display(),
                        canonical_vault.display()
                    ));
                }
                Ok(canonical)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Strip-and-join missed. For a `mtg_<uuid>` wire id,
                // fall back to a meetings/ scan so `Slug` / `DateSlug`
                // captures resolve. For a bare basename the renderer
                // got from `list_sessions`, the scan can't help (no
                // wire id to match), so we surface the original error.
                let Some(wire_uuid) = parse_meeting_wire_id(session_id) else {
                    return Err(format!("canonicalize {}: {}", candidate.display(), e));
                };
                find_note_path_by_wire_id(&canonical_vault, wire_uuid)
                    .await?
                    .ok_or_else(|| {
                        format!(
                            "no note in {} matches meeting id {}",
                            meetings.display(),
                            session_id
                        )
                    })
            }
            Err(e) => Err(format!("canonicalize {}: {}", candidate.display(), e)),
        }
    } else {
        // For write we ensure the meetings/ subdir exists (the vault
        // writer creates it on first capture; the renderer-only Save
        // path may run before that ever happened) before
        // canonicalizing.
        //
        // The `create_dir_all` → `canonicalize` pair is technically
        // racy (a hostile process could swap `meetings/` for a
        // symlink between the two calls). The vault directory is
        // user-owned by construction, so the threat is theoretical;
        // the `starts_with` check inside
        // `canonicalize_meetings_within` still prevents a vault
        // escape if the swap somehow lands.
        fs::create_dir_all(&meetings)
            .await
            .map_err(|e| format!("mkdir {}: {}", meetings.display(), e))?;
        let canonical_meetings = canonicalize_meetings_within(&meetings, &canonical_vault)
            .await?
            .ok_or_else(|| {
                format!(
                    "meetings dir {} disappeared after create_dir_all",
                    meetings.display()
                )
            })?;
        // Wire-id fallback for write: the renderer's Save path always
        // overwrites a note it just read, so the `Slug` / `DateSlug`
        // file already exists. Without this lookup, a `mtg_<uuid>`
        // save would create a sibling `<uuid>.md` next to the real
        // `<slug>.md` and silently fork user content. Strip-and-join
        // wins when the file *does* live at the bare-basename path
        // (the `Id` pattern, or a fresh capture under any pattern
        // whose first save races the writer's finalize — vanishingly
        // rare since the editor only mounts after `heron_read_note`
        // succeeds).
        let strip_target = canonical_meetings.join(format!("{basename}.md"));
        if fs::metadata(&strip_target).await.is_ok() {
            return Ok(strip_target);
        }
        if let Some(wire_uuid) = parse_meeting_wire_id(session_id)
            && let Some(existing) = find_note_path_by_wire_id(&canonical_vault, wire_uuid).await?
        {
            return Ok(existing);
        }
        Ok(strip_target)
    }
}

/// Parse `mtg_<uuid>` into the inner UUID, or return `None` for any
/// other input shape. Matches the wire form `heron_types::MeetingId`
/// emits via `Display` — but kept as a free function (not
/// `MeetingId::from_str`) so an invalid UUID payload short-circuits to
/// `None` (treat as "not a wire id, no fallback") rather than bubbling
/// a parse error into the renderer's `read` envelope. The strict
/// `from_str` path lives in the `meetings.rs` daemon proxies, which
/// guard the daemon's URL space; here we're picking an in-vault file.
fn parse_meeting_wire_id(session_id: &str) -> Option<Uuid> {
    let rest = session_id.strip_prefix("mtg_")?;
    Uuid::parse_str(rest).ok()
}

/// Scan `<canonical_vault>/meetings/*.md` for the file whose path-
/// derived `MeetingId` (UUIDv5 over the vault-relative bytes — same
/// namespace `heron_orchestrator::vault_read::derive_meeting_id` uses
/// on the daemon side) equals `wire_uuid`. Returns the canonical path
/// of the match, or `None` when no file matches (the empty-vault and
/// stale-wire-id cases).
///
/// Symmetric with the orchestrator's `find_note_path_by_id` so the
/// renderer's read / write lands on the exact note the daemon's
/// `Meeting.id` referred to. We deliberately don't share that function
/// — it's `pub(crate)` in the orchestrator and exposing it would widen
/// the orchestrator's surface for one consumer; the deriving formula is
/// stable per the namespace's `MEETING_ID_NAMESPACE` doc, which is
/// already `pub use`'d.
///
/// **Symlink-aware UUID derivation.** The orchestrator iterates
/// `vault_root.join("meetings")` lexically (without resolving the
/// symlink) and hashes `meetings/<file>.md` for each entry — even
/// when `meetings/` is itself a symlink to another in-vault directory.
/// We have to mirror that exact lexical shape, *not* the canonical
/// shape, or an in-vault symlink (e.g. `meetings/` → `data/`) yields
/// a different UUIDv5 on the renderer side and a valid wire id stops
/// resolving. We still pre-canonicalize to enforce the
/// "no symlink escape outside the vault" invariant, then fall back to
/// the lexical path for iteration.
async fn find_note_path_by_wire_id(
    canonical_vault: &Path,
    wire_uuid: Uuid,
) -> Result<Option<PathBuf>, String> {
    let meetings = meetings_dir(canonical_vault);
    // Containment pre-check: a symlinked `meetings/` pointing OUTSIDE
    // the vault is rejected here — the orchestrator's own
    // `note_paths_newest_first` would happily follow such a link, but
    // the desktop's vault-containment posture is stricter. Inside the
    // vault is fine, so this returns `Some(canonical)` for the in-
    // vault-symlink case and `None` for the pre-capture
    // (`meetings/` doesn't exist yet) case.
    if canonicalize_meetings_within(&meetings, canonical_vault)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    // Iterate via the lexical `meetings/` path. Reading from a
    // symlinked dir transparently follows the symlink, but the entry
    // paths the iterator yields are `<canonical_vault>/meetings/<file>`
    // — exactly the shape the orchestrator's `derive_meeting_id`
    // hashes (`meetings/<file>` after stripping the vault root). If
    // the lexical dir doesn't exist, we returned `None` above.
    let mut rd = fs::read_dir(&meetings)
        .await
        .map_err(|e| format!("readdir {}: {}", meetings.display(), e))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| format!("readdir {}: {}", meetings.display(), e))?
    {
        // Filter on the file_name's extension before allocating a
        // full `PathBuf` via `entry.path()`. Cheaper on big vaults
        // (skips e.g. every `.md.bak` rotation file without paying
        // the join cost). `OsStr::to_str` returns `None` for non-
        // UTF-8 names, which we treat as a non-match.
        let name = entry.file_name();
        let is_md = Path::new(&name)
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext == "md");
        if !is_md {
            continue;
        }
        // Reject the entry if it's itself a symlink — the
        // path-derived `MeetingId` is computed over the vault-
        // relative bytes, so a symlinked `<slug>.md` whose target
        // lives outside the vault would otherwise hash to a wire id
        // an attacker controls. `symlink_metadata` examines the
        // entry without following links; a real file passes.
        // Defense in depth: the post-match canonicalize +
        // `starts_with` below ALSO catches escapes.
        let lmeta = match fs::symlink_metadata(meetings.join(&name)).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !lmeta.file_type().is_file() {
            // Skip directories, symlinks, sockets, etc. The
            // recorder only finalizes regular files.
            continue;
        }
        let path = meetings.join(&name);
        // Derive the meeting id from the vault-relative LEXICAL bytes
        // — `meetings/<file>.md`. Mirrors
        // `heron_orchestrator::vault_read::derive_meeting_id`'s shape
        // exactly, including the in-vault-symlink case the canonical-
        // path version of this scan would silently miss.
        let rel = path.strip_prefix(canonical_vault).unwrap_or(&path);
        let derived = Uuid::new_v5(&MEETING_ID_NAMESPACE, rel.as_os_str().as_encoded_bytes());
        if derived == wire_uuid {
            // Canonicalize + containment check is the same belt
            // `list_sessions` and `resolve_note_path` apply; keep
            // this site in lockstep with the rest of the file's
            // vault-escape posture.
            let canonical = fs::canonicalize(&path)
                .await
                .map_err(|e| format!("canonicalize {}: {}", path.display(), e))?;
            if !canonical.starts_with(canonical_vault) {
                return Err(format!(
                    "resolved path {} escapes vault {}",
                    canonical.display(),
                    canonical_vault.display()
                ));
            }
            return Ok(Some(canonical));
        }
    }
    Ok(None)
}

/// Read `<vault>/meetings/<basename>.md` (where `<basename>` strips
/// any `mtg_` prefix from `session_id`).
///
/// Errors include the canonicalized path so a Sonner toast displays a
/// useful message without the React side stitching strings together.
pub async fn read_note(vault: &Path, session_id: &str) -> Result<String, String> {
    let path = resolve_note_path(vault, session_id, true).await?;
    fs::read_to_string(&path)
        .await
        .map_err(|e| format!("read {}: {}", path.display(), e))
}

/// Atomically write `contents` to `<vault>/meetings/<basename>.md`
/// (where `<basename>` strips any `mtg_` prefix from `session_id`).
///
/// Recipe (mirrors `heron_vault::atomic_write` and `settings::write_settings`):
/// 1. Resolve + validate the path lives inside the vault.
/// 2. Write to a sibling temp file `.<basename>.<uuid>.tmp`.
/// 3. `sync_all` so the bytes are durable on disk before the rename.
/// 4. `chmod 0600` so the note (which can contain sensitive meeting
///    transcripts) is not world-readable, matching the rest of the
///    vault's permission posture.
/// 5. `rename` over the destination — atomic on the same POSIX volume.
///
/// On any failure, best-effort delete the temp file so we don't leak
/// `.tmp` artifacts in the user's vault. The success path leaves no
/// `.tmp` behind because `rename` consumes it.
pub async fn write_note_atomic(
    vault: &Path,
    session_id: &str,
    contents: &str,
) -> Result<(), String> {
    let path = resolve_note_path(vault, session_id, false).await?;

    let temp = note_temp_path(&path);

    // Scope the file handle so it drops (and closes) before the rename.
    let write_result: Result<(), String> = async {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .await
            .map_err(|e| format!("create {}: {}", temp.display(), e))?;
        f.write_all(contents.as_bytes())
            .await
            .map_err(|e| format!("write {}: {}", temp.display(), e))?;
        f.sync_all()
            .await
            .map_err(|e| format!("fsync {}: {}", temp.display(), e))?;
        Ok(())
    }
    .await;

    if let Err(e) = write_result {
        // Best-effort cleanup. If the temp wasn't created, the unlink
        // returns NotFound and we ignore it.
        let _ = fs::remove_file(&temp).await;
        return Err(e);
    }

    if let Err(e) = set_user_only_perms(&temp).await {
        let _ = fs::remove_file(&temp).await;
        return Err(format!("chmod {}: {}", temp.display(), e));
    }

    if let Err(e) = fs::rename(&temp, &path).await {
        let _ = fs::remove_file(&temp).await;
        return Err(format!(
            "rename {} -> {}: {}",
            temp.display(),
            path.display(),
            e
        ));
    }

    Ok(())
}

/// Mirror of `settings::set_user_only_perms` against `tokio::fs`.
/// On non-unix platforms this is a no-op (Heron ships macOS-only;
/// the cfg gate keeps `cargo check --target x86_64-pc-windows-msvc`
/// honest).
#[cfg(unix)]
async fn set_user_only_perms(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perm = std::fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, perm).await
}

#[cfg(not(unix))]
async fn set_user_only_perms(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// List `.md` filenames in `vault_path`, without their extension.
///
/// Returns the basenames (e.g. `2025-04-25-meeting`) so the React side
/// can build `/review/<sessionId>` URLs without duplicating the path
/// arithmetic. Filters non-`.md` files (e.g. `.bak`, hidden files,
/// `.DS_Store`). Hidden `.md` files (leading `.`) are also filtered so
/// the basenames returned here always pass [`validate_session_id`] —
/// the round-trip "list, click, write" flow can't desync.
///
/// Errors include the directory path.
pub async fn list_sessions(vault_path: &Path) -> Result<Vec<String>, String> {
    let canonical = resolve_vault_path(vault_path).await?;
    let meetings = meetings_dir(&canonical);
    let mut out = Vec::new();
    // Canonicalize + containment-check before reading. A symlinked
    // `meetings/` would otherwise let `list_sessions` enumerate
    // filenames outside the vault — a defense-in-depth concern
    // because `resolve_note_path` would still reject the subsequent
    // read, but Heron treats vault containment as a hard boundary.
    // Pre-capture state (no `meetings/` yet) returns an empty list,
    // matching the orchestrator's `note_paths_newest_first`. A
    // *permission* error on the dir still surfaces.
    let canonical_meetings = match canonicalize_meetings_within(&meetings, &canonical).await? {
        Some(c) => c,
        None => return Ok(out),
    };
    let mut rd = fs::read_dir(&canonical_meetings)
        .await
        .map_err(|e| format!("readdir {}: {}", canonical_meetings.display(), e))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| format!("readdir {}: {}", canonical_meetings.display(), e))?
    {
        let p = entry.path();
        // Skip directories — `.md` matters only for files.
        let ft = match entry.file_type().await {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_file() {
            continue;
        }
        // Match on extension, not basename suffix, so `foo.md.bak`
        // (rotated rollback files) doesn't get listed.
        if p.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            // Filter anything that wouldn't pass back through
            // `validate_session_id` — hidden dotfiles, names with
            // spaces or non-ASCII characters. The sidebar only ever
            // navigates to ids that round-trip, so showing them
            // would just produce a "session id contains '...'" toast.
            if validate_session_id(stem).is_err() {
                continue;
            }
            out.push(stem.to_string());
        }
    }
    // Newest-first by lexicographic order on the filename. Heron
    // session filenames start with an ISO date (`YYYY-MM-DD…`), so
    // a single descending sort = newest-first without parsing.
    out.sort_by(|a, b| b.cmp(a));
    Ok(out)
}

/// Build a sibling temp path for the atomic write. UUIDv7 keeps the
/// name unique-per-call so concurrent writers can't collide on the
/// same target path.
fn note_temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let basename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "note".to_string());
    parent.join(format!(
        ".{basename}.{}.tmp",
        Uuid::new_v7(Timestamp::now(NoContext)).simple()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_read_write() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let body = "# hello\n\n> 00:00:01  Alice: hi\n";
        write_note_atomic(tmp.path(), "note", body)
            .await
            .expect("write");
        let read = read_note(tmp.path(), "note").await.expect("read");
        assert_eq!(read, body);
    }

    #[tokio::test]
    async fn read_missing_file_errors_with_path() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let err = read_note(tmp.path(), "does-not-exist")
            .await
            .expect_err("expected error");
        assert!(err.contains("does-not-exist"), "got: {err}");
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_note_atomic(tmp.path(), "note", "v1")
            .await
            .expect("write 1");
        write_note_atomic(tmp.path(), "note", "v2")
            .await
            .expect("write 2");
        let read = read_note(tmp.path(), "note").await.expect("read");
        assert_eq!(read, "v2");
    }

    /// Successful atomic write must leave no `.tmp` files behind.
    /// Asserts the rename consumed the temp instead of leaving it
    /// next to the destination.
    #[tokio::test]
    async fn atomic_write_leaves_no_temp_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_note_atomic(tmp.path(), "note", "body")
            .await
            .expect("write");

        let mut rd = fs::read_dir(meetings_dir(tmp.path()))
            .await
            .expect("readdir");
        let mut names = Vec::new();
        while let Some(e) = rd.next_entry().await.expect("entry") {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        // Only the final note should remain — no `.note.md.<uuid>.tmp`.
        assert_eq!(names, vec!["note.md".to_string()]);
    }

    /// `mtg_<uuid>` is the wire-form id; the on-disk basename is the
    /// bare uuid because that's the shape `heron_vault::VaultWriter`
    /// writes for `FileNamingPattern::Id`. Anchors that strip.
    #[tokio::test]
    async fn strips_mtg_prefix_for_on_disk_basename() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let id = "mtg_019ddcda-831c-72f0-927b-8b894466902c";
        write_note_atomic(tmp.path(), id, "body")
            .await
            .expect("write");
        let on_disk = meetings_dir(tmp.path()).join("019ddcda-831c-72f0-927b-8b894466902c.md");
        assert!(on_disk.exists(), "expected {} to exist", on_disk.display());
        let read = read_note(tmp.path(), id).await.expect("read");
        assert_eq!(read, "body");
    }

    #[tokio::test]
    async fn list_sessions_filters_non_md_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        fs::write(dir.join("a.md"), "a").await.expect("seed a");
        fs::write(dir.join("b.md"), "b").await.expect("seed b");
        fs::write(dir.join("c.txt"), "c").await.expect("seed c");
        fs::write(dir.join("d.md.bak"), "d").await.expect("seed d");
        fs::write(dir.join(".DS_Store"), "").await.expect("seed ds");

        let names = list_sessions(tmp.path()).await.expect("list");
        // Newest-first sort: "b" > "a" lexicographically.
        assert_eq!(names, vec!["b".to_string(), "a".to_string()]);
    }

    #[tokio::test]
    async fn list_sessions_skips_subdirectories() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        fs::create_dir(dir.join("subdir.md"))
            .await
            .expect("mkdir subdir");
        fs::write(dir.join("real.md"), "x")
            .await
            .expect("seed real");
        let names = list_sessions(tmp.path()).await.expect("list");
        assert_eq!(names, vec!["real".to_string()]);
    }

    /// Pre-capture state: the writer hasn't created `meetings/` yet.
    /// `list_sessions` returns an empty list rather than erroring so
    /// the Home page renders cleanly on a fresh install.
    #[tokio::test]
    async fn list_sessions_missing_meetings_dir_returns_empty() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let names = list_sessions(tmp.path()).await.expect("list");
        assert!(names.is_empty(), "got: {names:?}");
    }

    #[tokio::test]
    async fn list_sessions_missing_vault_errors() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let missing = tmp.path().join("nope");
        let err = list_sessions(&missing).await.expect_err("err");
        assert!(err.contains("nope"), "got: {err}");
    }

    /// Hidden `.md` files (e.g. `.draft.md`) are not user-visible
    /// session files; the sidebar would render them as confusing
    /// dotfile entries. Filter them out.
    #[tokio::test]
    async fn list_sessions_filters_hidden_md_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        fs::write(dir.join(".hidden.md"), "x").await.expect("seed");
        fs::write(dir.join("visible.md"), "y").await.expect("seed");
        let names = list_sessions(tmp.path()).await.expect("list");
        assert_eq!(names, vec!["visible".to_string()]);
    }

    /// Filenames the renderer can't possibly request via the URL
    /// (spaces, slashes, non-ASCII) are filtered so the listing
    /// stays in sync with what `validate_session_id` accepts.
    #[tokio::test]
    async fn list_sessions_filters_unsafe_filenames() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        fs::write(dir.join("with space.md"), "x")
            .await
            .expect("seed");
        fs::write(dir.join("ok-name.md"), "y").await.expect("seed");
        let names = list_sessions(tmp.path()).await.expect("list");
        assert_eq!(names, vec!["ok-name".to_string()]);
    }

    /// `.md` content is UTF-8; ensure round-trip preserves multibyte
    /// characters (em-dashes, smart quotes, emoji) since the
    /// transcript line format uses both.
    #[tokio::test]
    async fn round_trip_preserves_unicode() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let body = "# Café — über\n\n> 00:00:01  Élodie: hello 👋\n";
        write_note_atomic(tmp.path(), "note", body)
            .await
            .expect("write");
        let read = read_note(tmp.path(), "note").await.expect("read");
        assert_eq!(read, body);
    }

    /// Empty contents must succeed — clearing a note is a valid
    /// operation. Without this guarantee a user could delete every
    /// character in the editor and hit Save and get an opaque error.
    #[tokio::test]
    async fn empty_contents_round_trip() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_note_atomic(tmp.path(), "note", "")
            .await
            .expect("write");
        let read = read_note(tmp.path(), "note").await.expect("read");
        assert_eq!(read, "");
    }

    /// Anchors the path-traversal guard. A renderer / route bug that
    /// supplies `..` or a slash-laden id must NOT escape the vault.
    #[tokio::test]
    async fn rejects_traversal_session_id() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        for evil in ["..", ".", "../etc/passwd", "foo/bar", "a\\b", ""] {
            let err = write_note_atomic(tmp.path(), evil, "x")
                .await
                .expect_err(&format!("must reject {evil}"));
            assert!(!err.is_empty(), "empty error for {evil}");
        }
    }

    /// Leading `.` would create a hidden file; `list_sessions`
    /// already filters those, so the writer must reject them too —
    /// otherwise a save creates a note the sidebar can never show.
    #[tokio::test]
    async fn rejects_leading_dot_session_id() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let err = write_note_atomic(tmp.path(), ".hidden", "x")
            .await
            .expect_err("must reject leading dot");
        assert!(err.contains(".hidden"), "got: {err}");
    }

    /// On a missing vault the canonicalize fails before we even
    /// look at the session id — the user gets a clear "vault path"
    /// error rather than a confusing "session id" one.
    #[tokio::test]
    async fn rejects_missing_vault() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let missing = tmp.path().join("nope");
        let err = read_note(&missing, "session").await.expect_err("err");
        assert!(err.contains("nope"), "got: {err}");
    }

    /// On unix, the saved note must end up at mode 0600 — the same
    /// posture as `settings.rs` and `heron-vault::atomic_write`.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_sets_user_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_note_atomic(tmp.path(), "note", "body")
            .await
            .expect("write");
        let mode = std::fs::metadata(meetings_dir(tmp.path()).join("note.md"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// Symlink-out attack: even if the renderer placed a symlink
    /// inside the vault that points outside, opening it for read
    /// must canonicalize and reject. (Write goes through the parent
    /// canonicalization, which is `vault` itself, so it can't drop
    /// new files outside even via a symlinked subdirectory — but
    /// reads of pre-existing symlinked-out targets would otherwise
    /// succeed, leaking arbitrary files.)
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape_on_read() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let outside = tmp.path().join("outside.md");
        std::fs::write(&outside, "secret").expect("seed outside");

        let vault = tmp.path().join("vault");
        let meetings = meetings_dir(&vault);
        std::fs::create_dir_all(&meetings).expect("mkdir meetings");
        symlink(&outside, meetings.join("escape.md")).expect("symlink");

        let err = read_note(&vault, "escape")
            .await
            .expect_err("must reject symlink escape");
        assert!(
            err.contains("escapes vault") || err.contains("outside"),
            "got: {err}"
        );
    }

    /// `list_sessions` must reject a `meetings/` symlink that points
    /// outside the vault. Without this guard the renderer could
    /// enumerate filenames in any directory the symlink targets —
    /// `resolve_note_path` would still block the subsequent read, but
    /// vault containment is treated as a hard boundary in
    /// defense-in-depth.
    #[cfg(unix)]
    #[tokio::test]
    async fn list_sessions_rejects_symlinked_meetings_dir() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path().join("vault");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&vault).expect("mkdir vault");
        std::fs::create_dir(&outside).expect("mkdir outside");
        // Plant a `.md` file we'd be able to enumerate if the escape worked.
        std::fs::write(outside.join("leaked.md"), b"x").expect("seed");
        symlink(&outside, vault.join("meetings")).expect("symlink");

        let err = list_sessions(&vault)
            .await
            .expect_err("symlinked meetings/ must be rejected");
        assert!(err.contains("escapes vault"), "got: {err}");
    }

    // -------- issue #205: Slug / DateSlug wire-id resolution --------

    /// Derive the wire-form `MeetingId` string a daemon configured
    /// against `vault` would emit for a finalized note at
    /// `<vault>/meetings/<basename>.md`. Mirrors
    /// `heron_orchestrator::vault_read::derive_meeting_id` — the test
    /// re-implements rather than imports the helper because it's
    /// `pub(crate)` in the orchestrator.
    ///
    /// **Caveat:** because both sides re-implement the same formula,
    /// these tests don't catch a drift where the orchestrator's
    /// derivation changes shape (e.g. switching to a hash over a
    /// different path component). The pinned cross-crate contract is
    /// the `MEETING_ID_NAMESPACE` constant (already `pub use`'d) plus
    /// the formula documented at the namespace declaration. Any
    /// would-be drift should land as a coordinated cross-crate edit
    /// against both `vault_read.rs` and this module — there is no
    /// stable public function to wire an end-to-end parity test
    /// through today.
    fn wire_id_for(vault: &Path, basename: &str) -> String {
        let canonical = std::fs::canonicalize(vault).expect("canonicalize vault");
        let path = canonical.join("meetings").join(format!("{basename}.md"));
        let rel = path.strip_prefix(&canonical).expect("strip prefix");
        let derived = Uuid::new_v5(&MEETING_ID_NAMESPACE, rel.as_os_str().as_encoded_bytes());
        format!("mtg_{}", derived.as_hyphenated())
    }

    /// Seed a `Slug`-pattern note at `<vault>/meetings/<slug>.md` and
    /// confirm `heron_read_note(vault, mtg_<wire-id>)` resolves it. The
    /// strip-and-join lookup misses (no `<wire-uuid>.md`), so the
    /// fallback scan must find the file via path-derived id match.
    #[tokio::test]
    async fn read_note_resolves_slug_pattern_via_wire_id() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        let body = "# Slug pattern note\n";
        fs::write(dir.join("team-standup.md"), body)
            .await
            .expect("seed slug note");

        let wire_id = wire_id_for(tmp.path(), "team-standup");
        let read = read_note(tmp.path(), &wire_id)
            .await
            .expect("read via wire id");
        assert_eq!(read, body);
    }

    /// Seed a `DateSlug`-pattern note at
    /// `<vault>/meetings/<date>-<slug>.md` and confirm wire-id
    /// resolution reaches it. Same fallback path as the Slug test —
    /// the two patterns differ only in the on-disk basename shape.
    #[tokio::test]
    async fn read_note_resolves_date_slug_pattern_via_wire_id() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        let body = "# Date-slug pattern note\n";
        let basename = "2026-04-26-team-standup";
        fs::write(dir.join(format!("{basename}.md")), body)
            .await
            .expect("seed date-slug note");

        let wire_id = wire_id_for(tmp.path(), basename);
        let read = read_note(tmp.path(), &wire_id)
            .await
            .expect("read via wire id");
        assert_eq!(read, body);
    }

    /// `Id`-pattern remains a strip-and-join fast path. A `mtg_<uuid>`
    /// wire id where the file is `<uuid>.md` must NOT trigger a
    /// fallback scan (it would still work, but slowly).
    #[tokio::test]
    async fn read_note_resolves_id_pattern_via_strip_fast_path() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let id = "019ddcda-831c-72f0-927b-8b894466902c";
        let body = "# Id pattern note\n";
        write_note_atomic(tmp.path(), &format!("mtg_{id}"), body)
            .await
            .expect("write");
        let on_disk = meetings_dir(tmp.path()).join(format!("{id}.md"));
        assert!(on_disk.exists(), "Id pattern writes <uuid>.md directly");
        let read = read_note(tmp.path(), &format!("mtg_{id}"))
            .await
            .expect("read");
        assert_eq!(read, body);
    }

    /// Write-through-wire-id: when the on-disk note is `<slug>.md`,
    /// `write_note_atomic(vault, mtg_<wire-id>, body)` must overwrite
    /// it in place — not create a sibling `<wire-uuid>.md` next to
    /// the slug file. Without this, an editor save under Slug /
    /// DateSlug silently forks the user's note.
    #[tokio::test]
    async fn write_note_via_wire_id_overwrites_slug_file_in_place() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        fs::write(dir.join("project-kickoff.md"), "v1\n")
            .await
            .expect("seed");

        let wire_id = wire_id_for(tmp.path(), "project-kickoff");
        write_note_atomic(tmp.path(), &wire_id, "v2\n")
            .await
            .expect("write via wire id");

        // Single file remains: the slug-named one, with v2 contents.
        let mut rd = fs::read_dir(&dir).await.expect("readdir");
        let mut names: Vec<String> = Vec::new();
        while let Some(e) = rd.next_entry().await.expect("entry") {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        assert_eq!(names, vec!["project-kickoff.md".to_string()]);
        let read = fs::read_to_string(dir.join("project-kickoff.md"))
            .await
            .expect("read");
        assert_eq!(read, "v2\n");
    }

    /// A `mtg_<uuid>` wire id with no matching on-disk note errors
    /// with the meeting id in the message — distinct from the
    /// strip-and-join NotFound the `Id` flow surfaces. Without this,
    /// a stale renderer cache (the daemon reaped a meeting but the
    /// React tree hasn't refreshed) would dump a confusing
    /// "canonicalize <vault>/meetings/<wire-uuid>.md" toast.
    #[tokio::test]
    async fn read_note_unknown_wire_id_errors_with_meeting_context() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        // A stale wire id that doesn't match any seeded file.
        let stale = "mtg_00000000-0000-7000-8000-000000000000";
        let err = read_note(tmp.path(), stale)
            .await
            .expect_err("expected error");
        assert!(
            err.contains("matches meeting id") || err.contains(stale),
            "got: {err}"
        );
    }

    /// A bare basename input (the sidebar / `list_sessions` path)
    /// must still hit the strip-and-join fast path for slug-named
    /// files — no scan, no `mtg_` prefix. Pins that the fallback
    /// only triggers for wire-id input shapes.
    #[tokio::test]
    async fn read_note_resolves_bare_basename_for_slug_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = meetings_dir(tmp.path());
        fs::create_dir_all(&dir).await.expect("mkdir meetings");
        let body = "# bare basename\n";
        fs::write(dir.join("ad-hoc-chat.md"), body)
            .await
            .expect("seed");

        // Sidebar passes the bare basename, not the wire id.
        let read = read_note(tmp.path(), "ad-hoc-chat")
            .await
            .expect("read by basename");
        assert_eq!(read, body);
    }

    /// In-vault symlink-meetings: when the user's `meetings/` is a
    /// symlink to another directory inside the vault (e.g. they're
    /// migrating a Dropbox layout), the orchestrator's
    /// `derive_meeting_id` hashes `meetings/<file>.md` lexically —
    /// it never resolves the symlink. The renderer's wire-id scan
    /// must mirror that exactly or a valid daemon-issued wire id
    /// stops resolving. Anchors the lexical-iteration choice in
    /// [`find_note_path_by_wire_id`].
    #[cfg(unix)]
    #[tokio::test]
    async fn read_note_via_wire_id_through_in_vault_meetings_symlink() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().expect("tmp");
        // Stand the vault up as `<tmp>/vault` so `<tmp>/data` is a
        // sibling, not a parent — both are inside the canonical
        // vault root after the symlink resolves.
        let vault = tmp.path().join("vault");
        std::fs::create_dir(&vault).expect("mkdir vault");
        let real_meetings = vault.join("data");
        std::fs::create_dir(&real_meetings).expect("mkdir data");
        symlink(&real_meetings, vault.join("meetings")).expect("symlink meetings -> data");

        let body = "# in-vault symlink target\n";
        fs::write(real_meetings.join("kickoff-notes.md"), body)
            .await
            .expect("seed note");

        let wire_id = wire_id_for(&vault, "kickoff-notes");
        let read = read_note(&vault, &wire_id)
            .await
            .expect("read via wire id through in-vault symlink");
        assert_eq!(read, body);
    }

    /// `parse_meeting_wire_id` accepts only `mtg_<valid-uuid>`. A
    /// non-prefixed input or a malformed UUID returns `None` so the
    /// caller's NotFound error surfaces rather than the parse error
    /// (the renderer doesn't distinguish, and "no such note" is the
    /// correct semantic).
    #[test]
    fn parse_meeting_wire_id_strict() {
        assert!(parse_meeting_wire_id("019ddcda-831c-72f0-927b-8b894466902c").is_none());
        assert!(parse_meeting_wire_id("ad-hoc-chat").is_none());
        assert!(parse_meeting_wire_id("mtg_not-a-uuid").is_none());
        assert!(parse_meeting_wire_id("mtg_").is_none());
        assert!(
            parse_meeting_wire_id("mtg_019ddcda-831c-72f0-927b-8b894466902c").is_some(),
            "valid wire id must parse",
        );
    }
}
