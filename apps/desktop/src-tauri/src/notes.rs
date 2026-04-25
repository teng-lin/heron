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
//! any string, but only `<vault>/<basename>.md` (no traversal, no
//! symlink-out, basename matches `[A-Za-z0-9._-]+`) reaches the
//! filesystem. Without this, a route bug or compromised webview
//! would have arbitrary local-file capability.
//!
//! Errors surface as `String` to match the existing `lib.rs` pattern
//! (`AssetError::to_string`, `SettingsError::to_string`) — the React
//! tree shows them in a Sonner toast and doesn't need a typed error.

use std::path::{Path, PathBuf};

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
fn validate_session_id(id: &str) -> Result<(), String> {
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
async fn resolve_vault_path(vault: &Path) -> Result<PathBuf, String> {
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

/// Resolve `<vault>/<session_id>.md` and confirm the result is inside
/// the canonical vault — no symlink escapes, no `..` shenanigans.
///
/// Returns the path the renderer is allowed to read/write. The
/// canonicalize step requires the file to exist on read; for write we
/// canonicalize the *parent* and re-attach the basename so a brand-new
/// note still passes.
async fn resolve_note_path(
    vault: &Path,
    session_id: &str,
    must_exist: bool,
) -> Result<PathBuf, String> {
    validate_session_id(session_id)?;
    let canonical_vault = resolve_vault_path(vault).await?;
    let candidate = canonical_vault.join(format!("{session_id}.md"));

    if must_exist {
        let canonical = fs::canonicalize(&candidate)
            .await
            .map_err(|e| format!("canonicalize {}: {}", candidate.display(), e))?;
        if !canonical.starts_with(&canonical_vault) {
            return Err(format!(
                "resolved path {} escapes vault {}",
                canonical.display(),
                canonical_vault.display()
            ));
        }
        Ok(canonical)
    } else {
        // For write we re-canonicalize the parent (which must exist —
        // it's `canonical_vault` itself) and verify containment of the
        // *parent* so a new file the validator already vetted is
        // allowed through.
        if candidate.parent() != Some(&canonical_vault) {
            return Err(format!(
                "{} is not inside vault {}",
                candidate.display(),
                canonical_vault.display()
            ));
        }
        Ok(candidate)
    }
}

/// Read `<vault>/<session_id>.md`.
///
/// Errors include the canonicalized path so a Sonner toast displays a
/// useful message without the React side stitching strings together.
pub async fn read_note(vault: &Path, session_id: &str) -> Result<String, String> {
    let path = resolve_note_path(vault, session_id, true).await?;
    fs::read_to_string(&path)
        .await
        .map_err(|e| format!("read {}: {}", path.display(), e))
}

/// Atomically write `contents` to `<vault>/<session_id>.md`.
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
    let mut out = Vec::new();
    let mut rd = fs::read_dir(&canonical)
        .await
        .map_err(|e| format!("readdir {}: {}", canonical.display(), e))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| format!("readdir {}: {}", canonical.display(), e))?
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

        let mut rd = fs::read_dir(tmp.path()).await.expect("readdir");
        let mut names = Vec::new();
        while let Some(e) = rd.next_entry().await.expect("entry") {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        // Only the final note should remain — no `.note.md.<uuid>.tmp`.
        assert_eq!(names, vec!["note.md".to_string()]);
    }

    #[tokio::test]
    async fn list_sessions_filters_non_md_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = tmp.path();
        fs::write(dir.join("a.md"), "a").await.expect("seed a");
        fs::write(dir.join("b.md"), "b").await.expect("seed b");
        fs::write(dir.join("c.txt"), "c").await.expect("seed c");
        fs::write(dir.join("d.md.bak"), "d").await.expect("seed d");
        fs::write(dir.join(".DS_Store"), "").await.expect("seed ds");

        let names = list_sessions(dir).await.expect("list");
        // Newest-first sort: "b" > "a" lexicographically.
        assert_eq!(names, vec!["b".to_string(), "a".to_string()]);
    }

    #[tokio::test]
    async fn list_sessions_skips_subdirectories() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = tmp.path();
        fs::create_dir(dir.join("subdir.md"))
            .await
            .expect("mkdir subdir");
        fs::write(dir.join("real.md"), "x")
            .await
            .expect("seed real");
        let names = list_sessions(dir).await.expect("list");
        assert_eq!(names, vec!["real".to_string()]);
    }

    #[tokio::test]
    async fn list_sessions_missing_dir_errors() {
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
        let dir = tmp.path();
        fs::write(dir.join(".hidden.md"), "x").await.expect("seed");
        fs::write(dir.join("visible.md"), "y").await.expect("seed");
        let names = list_sessions(dir).await.expect("list");
        assert_eq!(names, vec!["visible".to_string()]);
    }

    /// Filenames the renderer can't possibly request via the URL
    /// (spaces, slashes, non-ASCII) are filtered so the listing
    /// stays in sync with what `validate_session_id` accepts.
    #[tokio::test]
    async fn list_sessions_filters_unsafe_filenames() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = tmp.path();
        fs::write(dir.join("with space.md"), "x")
            .await
            .expect("seed");
        fs::write(dir.join("ok-name.md"), "y").await.expect("seed");
        let names = list_sessions(dir).await.expect("list");
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
        let mode = std::fs::metadata(tmp.path().join("note.md"))
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
        std::fs::create_dir(&vault).expect("mkdir vault");
        symlink(&outside, vault.join("escape.md")).expect("symlink");

        let err = read_note(&vault, "escape")
            .await
            .expect_err("must reject symlink escape");
        assert!(
            err.contains("escapes vault") || err.contains("outside"),
            "got: {err}"
        );
    }
}
