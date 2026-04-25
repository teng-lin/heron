//! Re-summarize + `.md.bak` rollback backend per §15 PR-ε (phase 67).
//!
//! Wraps [`heron_cli::summarize::re_summarize_in_vault`] for the Tauri
//! Re-summarize button and adds two companion commands —
//! [`check_backup`] and [`restore_backup`] — that surface the
//! `<note>.md.bak` rotation [`heron_vault::VaultWriter::re_summarize`]
//! creates so the Review UI can offer a one-click rollback.
//!
//! ## Why a module separate from `notes.rs`?
//!
//! `notes.rs` is the read/write/list surface for plain markdown; it
//! stays free of LLM dependencies so its tests run without the
//! `heron-cli` + `heron-llm` build cost. The summarizer-driven flow
//! lives here so the `notes` shape remains lightweight.
//!
//! ## Path policy
//!
//! Every command routes through [`crate::notes::resolve_note_path`] /
//! [`crate::notes::resolve_vault_path`] so the same basename-only,
//! canonicalize-then-containment-check guard `notes::read_note` and
//! friends use protects this surface too. `.md.bak` lives next to the
//! note inside the vault — the writer never escapes it.
//!
//! ## Errors
//!
//! All four entry points return `Result<_, String>` so the React side
//! can render the failure as a Sonner toast verbatim, matching the
//! `notes::read_note` / `settings::*` convention.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::fs;

use heron_cli::summarize::re_summarize_in_vault;
use heron_llm::{Preference, select_summarizer};

use crate::notes::{resolve_note_path, resolve_vault_path, validate_session_id};

/// Resolve the `<vault>/<session_id>.md.bak` path the renderer is
/// allowed to touch. Validation goes through the same checks
/// [`crate::notes::resolve_note_path`] runs (basename allowlist,
/// canonicalize the vault, ensure containment) — only the file
/// extension differs.
async fn resolve_bak_path(vault: &Path, session_id: &str) -> Result<PathBuf, String> {
    validate_session_id(session_id)?;
    let canonical_vault = resolve_vault_path(vault).await?;
    let candidate = canonical_vault.join(format!("{session_id}.md.bak"));
    // The parent must equal the canonical vault. `notes::resolve_note_path`
    // does the same parent-equals-vault check on the write path so a
    // brand-new file (no canonicalize target) passes; we re-use the
    // logic here so a missing `.md.bak` (the common case) still resolves.
    if candidate.parent() != Some(&canonical_vault) {
        return Err(format!(
            "{} is not inside vault {}",
            candidate.display(),
            canonical_vault.display()
        ));
    }
    Ok(candidate)
}

/// Metadata about a `<note>.md.bak` file the Review UI can show next
/// to the editor as a "Backup from <timestamp>" pill.
///
/// `created_at` is an ISO-8601 / RFC-3339 string in the system local
/// timezone offset — the renderer formats it for display via
/// `Intl.DateTimeFormat`. We surface the modification time rather than
/// inode-creation time because macOS / Linux disagree on what "created"
/// means for files renamed-into-place by `atomic_write`, and `mtime`
/// is what every POSIX tool reports.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupInfo {
    pub created_at: String,
}

/// Re-summarize `<vault>/<session_id>.md` in place, returning the new
/// body the Review editor should render.
///
/// Wires [`heron_llm::select_summarizer(Preference::Auto)`] →
/// [`heron_cli::summarize::re_summarize_in_vault`]. The vault writer
/// rotates the prior body into `<id>.md.bak` *before* overwriting the
/// note, which is what makes the Restore button a true rollback (the
/// `.md.bak` has the user's pre-resummarize content even if the LLM
/// output later turns out worse).
///
/// Returns the rendered note (frontmatter + body) so the renderer can
/// re-mount the editor against the post-merge text. The frontend
/// strips the `---` frontmatter fences when displaying — we hand back
/// the full rendered output so the round-trip with `heron_read_note`
/// (which the frontend already uses) stays consistent.
pub async fn resummarize(vault: &Path, session_id: &str) -> Result<String, String> {
    let note_path = resolve_note_path(vault, session_id, true).await?;
    let canonical_vault = resolve_vault_path(vault).await?;

    // Build a real summarizer per `Preference::Auto`. The selector
    // chooses anthropic if `ANTHROPIC_API_KEY` is set, falling back
    // to `claude` / `codex` CLIs if they're on PATH. Errors surface
    // verbatim so the Review UI can render an actionable toast
    // ("set ANTHROPIC_API_KEY", "install claude-code", etc).
    let (summarizer, _backend, _reason) =
        select_summarizer(Preference::Auto).map_err(|e| format!("LLM backend: {e}"))?;

    // `re_summarize_in_vault` does the work:
    // 1. Reads the note's frontmatter to find the transcript path.
    // 2. Calls `Orchestrator::re_summarize_note` (§10.5 ID preservation).
    // 3. Calls `VaultWriter::re_summarize` (§10.3 merge + .md.bak rotation).
    re_summarize_in_vault(summarizer.as_ref(), &canonical_vault, &note_path)
        .await
        .map_err(|e| format!("re-summarize: {e}"))?;

    // Read the note back so the frontend doesn't need a follow-up
    // `heron_read_note` call.
    fs::read_to_string(&note_path)
        .await
        .map_err(|e| format!("read {}: {}", note_path.display(), e))
}

/// Return the `.md.bak`'s modification time as an ISO-8601 string,
/// or `None` when no backup exists.
///
/// "No backup" is `Ok(None)` rather than an error: the renderer mounts
/// the Review page on every navigation and a missing `.md.bak` is the
/// common (steady-state) case. Surfacing it as `Err` would force every
/// caller to pattern-match on a substring of the error message.
pub async fn check_backup(vault: &Path, session_id: &str) -> Result<Option<BackupInfo>, String> {
    let bak = resolve_bak_path(vault, session_id).await?;
    match fs::metadata(&bak).await {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .map_err(|e| format!("mtime {}: {}", bak.display(), e))?;
            let dt: chrono::DateTime<chrono::Local> = mtime.into();
            Ok(Some(BackupInfo {
                created_at: dt.to_rfc3339(),
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("stat {}: {}", bak.display(), e)),
    }
}

/// Restore `<vault>/<session_id>.md` from `<vault>/<session_id>.md.bak`,
/// then delete the `.bak` so the Review UI's pill goes away.
///
/// Returns the restored body so the editor can re-mount immediately
/// (mirrors [`resummarize`]'s "return-the-new-body" contract). Atomic
/// over-write goes through [`crate::notes::write_note_atomic`] which
/// uses the same temp-file + fsync + rename recipe everything else in
/// the desktop crate writes with.
///
/// On a successful overwrite we best-effort delete the `.bak`. If the
/// delete fails the next render of the page will still see the old
/// `.bak` and offer a Restore again — idempotent rollback.
pub async fn restore_backup(vault: &Path, session_id: &str) -> Result<String, String> {
    let bak = resolve_bak_path(vault, session_id).await?;
    // Single read serves as both the existence check and the data
    // fetch; we map ENOENT to a clear "does not exist" message so a
    // double-click race surfaces a useful toast rather than the raw
    // OS error string. Skipping the upfront `try_exists` also closes
    // a small TOCTOU window where the file could vanish between the
    // probe and the read.
    let body = fs::read_to_string(&bak).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{} does not exist", bak.display())
        } else {
            format!("read {}: {}", bak.display(), e)
        }
    })?;
    crate::notes::write_note_atomic(vault, session_id, &body).await?;

    // Best-effort cleanup. If the unlink races with another writer the
    // worst case is the user sees the Restore pill on the next page
    // load and clicks Restore again — same body, no data loss.
    let _ = fs::remove_file(&bak).await;
    Ok(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::fs;

    /// Helper: seed a vault with a note. The body alone (no
    /// frontmatter) is enough for `check_backup` / `restore_backup`
    /// since they don't parse the markdown.
    async fn seed_note(vault: &Path, session_id: &str, body: &str) {
        fs::write(vault.join(format!("{session_id}.md")), body)
            .await
            .expect("seed note");
    }

    async fn seed_bak(vault: &Path, session_id: &str, body: &str) {
        fs::write(vault.join(format!("{session_id}.md.bak")), body)
            .await
            .expect("seed bak");
    }

    /// `check_backup` returns `None` when no `.md.bak` exists. The
    /// steady-state / common-case path on every page load — must not
    /// surface as an error.
    #[tokio::test]
    async fn check_backup_returns_none_when_no_backup() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "body").await;

        let result = check_backup(vault, "note").await.expect("check");
        assert!(result.is_none(), "expected None, got {result:?}");
    }

    /// `check_backup` returns the mtime as an RFC-3339 string when a
    /// `.md.bak` is present.
    #[tokio::test]
    async fn check_backup_returns_some_with_timestamp() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "current").await;
        seed_bak(vault, "note", "previous").await;

        let result = check_backup(vault, "note").await.expect("check");
        let info = result.expect("expected Some(BackupInfo)");
        // Don't pin the exact value (depends on host clock); assert
        // the shape is RFC-3339-ish (contains `T` separator + a `:`
        // in the time portion). chrono's `to_rfc3339` always emits
        // these, so the assertion is stable.
        assert!(info.created_at.contains('T'), "got: {}", info.created_at);
    }

    /// Path-traversal attempts are rejected before any filesystem
    /// operation. Anchors the same shared validator that `notes.rs`
    /// uses on its read/write surface.
    #[tokio::test]
    async fn check_backup_rejects_traversal() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        for evil in ["..", ".", "../etc/passwd", "foo/bar"] {
            let err = check_backup(vault, evil)
                .await
                .expect_err(&format!("must reject {evil}"));
            assert!(!err.is_empty());
        }
    }

    /// `restore_backup` overwrites the note with the `.md.bak` body
    /// AND deletes the `.bak` — the common rollback flow.
    #[tokio::test]
    async fn restore_backup_overwrites_note_and_deletes_bak() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "current").await;
        seed_bak(vault, "note", "previous").await;

        let restored = restore_backup(vault, "note").await.expect("restore");
        assert_eq!(restored, "previous");

        // Note now contains the backup body.
        let on_disk = fs::read_to_string(vault.join("note.md"))
            .await
            .expect("read note");
        assert_eq!(on_disk, "previous");

        // .bak is gone.
        let bak = vault.join("note.md.bak");
        assert!(!bak.exists(), "expected .md.bak to be deleted");

        // Subsequent check_backup returns None.
        let again = check_backup(vault, "note").await.expect("check again");
        assert!(again.is_none());
    }

    /// `restore_backup` errors clearly when there's no `.bak` to
    /// restore from. The Review UI should never call this without a
    /// pill being visible, but a stale double-click while the first
    /// restore is in flight could race; the error message is the
    /// safety net.
    #[tokio::test]
    async fn restore_backup_errors_when_no_bak() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "body").await;

        let err = restore_backup(vault, "note").await.expect_err("must error");
        assert!(err.contains("does not exist"), "got: {err}");
    }

    /// On unix, the restored note keeps the same 0600 permissions
    /// `notes::write_note_atomic` enforces. Anchors that the rollback
    /// path doesn't accidentally widen the posture vs. a normal save.
    #[cfg(unix)]
    #[tokio::test]
    async fn restore_backup_preserves_user_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "current").await;
        seed_bak(vault, "note", "previous").await;
        restore_backup(vault, "note").await.expect("restore");

        let mode = std::fs::metadata(vault.join("note.md"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// `resolve_bak_path` rejects the same traversal payloads that
    /// `notes::resolve_note_path` rejects, end-to-end. Without this
    /// guard a renderer bug supplying `..` could read or write a
    /// `.md.bak` outside the vault — the rollback surface must keep
    /// parity with the read/write surface.
    #[tokio::test]
    async fn resolve_bak_path_rejects_traversal() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        for evil in ["..", ".", "foo/bar", "a\\b", ""] {
            let err = resolve_bak_path(vault, evil)
                .await
                .expect_err(&format!("must reject {evil}"));
            assert!(!err.is_empty());
        }
    }
}
