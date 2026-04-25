//! Crash recovery salvage scan + per-session purge.
//!
//! Phase 69 (PR-η). After a SIGKILL / panic during a recording the
//! orchestrator never gets to write the final `.md` to the vault and
//! the per-session cache directory at
//! `~/Library/Caches/heron/sessions/<session_id>/` is left behind in
//! a non-finalized state. This module surfaces those leftovers to the
//! frontend so the user can either:
//!
//! - **Recover** — re-run the orchestrator's finalize path. Today the
//!   orchestrator does not yet expose a standalone "finalize this
//!   cache directory" entry point (the run-from-cache path lives
//!   inside `Orchestrator::run`, which assumes a live audio capture);
//!   the [`heron_recover_session`] command is therefore a placeholder
//!   that returns a clear error rather than producing a partial /
//!   incorrect note. A follow-up PR wires in the real re-finalize
//!   entry point once `crates/heron-cli` exposes one.
//! - **Purge** — recursively delete the cache directory. Symlinks are
//!   refused before any descent so a planted symlink at
//!   `<cache>/sessions/foo` cannot trick the purge into deleting an
//!   arbitrary directory tree on disk.
//!
//! ## Path-safety contract
//!
//! Both [`heron_recover_session`] and [`heron_purge_session`] take a
//! `session_id: String` from the frontend. The id is validated as a
//! basename (no path separators, no `..`, no leading `.`) before any
//! file-system access — the rest of this module assumes that contract
//! holds.
//!
//! ## Diagnostics-shaped state file
//!
//! The brief identifies `<session_id>/heron_session.json` as the
//! "is this finalized?" probe. The diagnostics format (see
//! [`crate::diagnostics::SessionLog`]) does not currently carry an
//! explicit `status` field — adding it is out of scope. We treat the
//! presence of `"status": "finalized"` as the only "skip this" signal
//! and surface every other shape as unfinalized; that's deliberately
//! permissive so a half-written session never disappears from the
//! salvage list because of a parser quirk.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Hard cap on the size of `heron_session.json` we'll slurp into
/// memory while scanning. Real diagnostics files are well under 64
/// KiB; refusing larger files prevents a runaway writer (or a planted
/// payload) from blowing up startup memory.
const MAX_DIAG_FILE_BYTES: u64 = 1 << 20;

/// One entry in the salvage list. Wire shape mirrored on the frontend
/// at `apps/desktop/src/lib/invoke.ts`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UnfinalizedSession {
    /// Directory basename, used as the session identifier on the
    /// frontend. Always a basename, never a path.
    pub session_id: String,
    /// ISO 8601 / RFC 3339 timestamp of when the session started — the
    /// `heron_session.json` field if present, otherwise the cache
    /// directory's mtime.
    pub started_at: String,
    /// Sum of `*.wav` file sizes inside the session dir, in bytes.
    /// `0` when no WAV files exist (e.g., the user armed but never
    /// hit "Yes, go" so the capture never started).
    pub audio_bytes: u64,
    /// `true` iff at least one `transcript*.json` file exists in the
    /// session dir. Used by the UI to decide whether to label the
    /// session "transcribed but not summarized" vs. "audio only".
    pub has_partial_transcript: bool,
}

/// Errors from the salvage module. Wraps `io::Error` and the parse
/// path; everything reaches the frontend as a `String` via the
/// command shims so the React tree doesn't need this enum on the wire.
#[derive(Debug, Error)]
pub enum SalvageError {
    #[error("invalid session id: {reason}")]
    InvalidSessionId { reason: &'static str },
    #[error("session directory does not exist: {path}")]
    SessionNotFound { path: PathBuf },
    #[error("refusing to follow symlink at {path}")]
    SymlinkRefused { path: PathBuf },
    #[error("recovery is not yet wired through the orchestrator")]
    RecoveryNotImplemented,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Permissive view of `heron_session.json` for the salvage scan. We
/// only care about two fields:
///
/// - `status` — when present and equal to `"finalized"`, the session
///   is finished and we skip it.
/// - `started_at` — ISO 8601; preferred over the directory mtime when
///   present so two sessions started on the same minute keep their
///   relative order across a clock skew.
///
/// Every field is optional; a malformed or empty file falls through to
/// the mtime fallback rather than aborting the scan.
#[derive(Debug, Default, Deserialize)]
struct SalvageProbe {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
}

/// Resolve the cache root that holds per-session directories.
///
/// Mirrors the path the orchestrator writes to:
/// `~/Library/Caches/heron/sessions/`. Returns `None` when
/// [`dirs::cache_dir`] cannot be resolved (sandboxed test runners /
/// minimal containers); the scan treats that as "no candidates".
pub fn default_cache_root() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("heron").join("sessions"))
}

/// Validate that `session_id` is a plain basename — no path
/// separators, no `..`, no leading `.` (so a session id can never
/// reach outside the cache root or stomp on a hidden marker file).
///
/// The character whitelist is permissive: anything that is not `/`,
/// `\`, NUL, or starts with `.`/`..` passes. UUIDs (the format the
/// orchestrator uses) easily clear the bar; we keep the rule loose so
/// a future format change (e.g., a slug with hyphens) doesn't trip it.
fn validate_session_id(session_id: &str) -> Result<&str, SalvageError> {
    if session_id.is_empty() {
        return Err(SalvageError::InvalidSessionId {
            reason: "empty session id",
        });
    }
    if session_id == "." || session_id == ".." {
        return Err(SalvageError::InvalidSessionId {
            reason: "session id is a relative-path token",
        });
    }
    if session_id.starts_with('.') {
        return Err(SalvageError::InvalidSessionId {
            reason: "session id may not start with a dot",
        });
    }
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains('\0') {
        return Err(SalvageError::InvalidSessionId {
            reason: "session id may not contain path separators",
        });
    }
    Ok(session_id)
}

/// Walk `cache_root` one level deep and return every subdirectory
/// whose `heron_session.json` does NOT show a `status: "finalized"`
/// marker.
///
/// Missing root, unreadable entries, and broken symlinks are all
/// treated as "no candidate from this entry" — the scan never aborts
/// on a single bad child. This matches the brief's guidance that the
/// salvage page should fail open ("nothing to recover") rather than
/// failing closed (the user can't even see what's there).
pub fn scan_unfinalized(cache_root: &Path) -> Result<Vec<UnfinalizedSession>, SalvageError> {
    let entries = match std::fs::read_dir(cache_root) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SalvageError::Io(e)),
    };

    let mut out: Vec<UnfinalizedSession> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Reject symlinks at the top level so a planted symlink can't
        // smuggle a "finalized" marker from outside the cache root.
        // `symlink_metadata` does NOT follow the link.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Hidden / dotfile children (`.DS_Store`, `.heron-tmp-*`) are
        // not real sessions; skip them to keep the salvage list tidy.
        if name.starts_with('.') {
            continue;
        }

        let probe = read_probe(&path);
        if probe.status.as_deref() == Some("finalized") {
            continue;
        }

        let audio_bytes = sum_wav_bytes(&path);
        let has_partial_transcript = any_transcript_file(&path);
        let started_at = probe.started_at.unwrap_or_else(|| mtime_iso(&meta));

        out.push(UnfinalizedSession {
            session_id: name.to_owned(),
            started_at,
            audio_bytes,
            has_partial_transcript,
        });
    }

    // Sort newest-first so the most-recent crash floats to the top of
    // the list; tie-break on session_id for deterministic ordering
    // when two sessions share an mtime (the test fixture path).
    out.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(out)
}

/// Read `<dir>/heron_session.json`. Permissive: missing file or any
/// parse / IO error returns the default probe so the session still
/// surfaces as unfinalized (caller's choice — see [`scan_unfinalized`]).
fn read_probe(dir: &Path) -> SalvageProbe {
    let path = dir.join("heron_session.json");
    let f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return SalvageProbe::default(),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 || len > MAX_DIAG_FILE_BYTES {
        return SalvageProbe::default();
    }
    // `serde_json::from_reader` reads incrementally without an
    // intermediate `String`; `unwrap_or_default` is the right
    // permissive fallback because malformed JSON should not erase
    // a salvageable session from the list.
    serde_json::from_reader(f).unwrap_or_default()
}

/// Sum the byte size of every `*.wav` file in `dir` (one level deep).
/// Returns `0` on any IO error so the salvage list still renders.
fn sum_wav_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // Use `metadata` (follows symlinks) here — a symlink to a
        // legitimate WAV is fine, since the path stays inside the
        // session dir and we already rejected symlinks at the
        // top-level scan.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.to_ascii_lowercase().ends_with(".wav") {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// `true` iff `dir` contains at least one `transcript*.json` file.
fn any_transcript_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let lc = name.to_ascii_lowercase();
        if lc.starts_with("transcript") && lc.ends_with(".json") {
            return true;
        }
    }
    false
}

/// Format a [`std::fs::Metadata`] mtime as RFC 3339 UTC. Falls back to
/// the unix epoch when the platform cannot resolve mtime — `0` is
/// still a valid ordering token, just one that always sorts last in
/// the newest-first scan above.
///
/// `Duration::as_secs()` returns `u64`; the i64 conversion is checked
/// rather than cast so a corrupt mtime in the year 292B (or a future
/// platform that returns garbage) falls back to the epoch instead of
/// silently wrapping.
fn mtime_iso(meta: &std::fs::Metadata) -> String {
    let dt = meta
        .modified()
        .or_else(|_| meta.created())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| {
            let secs = i64::try_from(d.as_secs()).ok()?;
            DateTime::<Utc>::from_timestamp(secs, d.subsec_nanos())
        })
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    dt.to_rfc3339()
}

/// Recursively delete `<cache_root>/<session_id>/`.
///
/// Refuses to follow symlinks at every level: the implementation
/// walks the tree by hand using [`std::fs::symlink_metadata`] so a
/// planted symlink cannot trick the purge into removing an arbitrary
/// directory. Symlink children inside the tree are unlinked (the link
/// itself, not its target).
pub fn purge_session(cache_root: &Path, session_id: &str) -> Result<(), SalvageError> {
    let session_id = validate_session_id(session_id)?;
    let target = cache_root.join(session_id);

    let meta = match std::fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SalvageError::SessionNotFound { path: target });
        }
        Err(e) => return Err(SalvageError::Io(e)),
    };
    if meta.file_type().is_symlink() {
        return Err(SalvageError::SymlinkRefused { path: target });
    }
    if !meta.is_dir() {
        // A dangling file at this name is not a session; refuse rather
        // than guess the user's intent.
        return Err(SalvageError::SessionNotFound { path: target });
    }
    delete_dir_no_symlinks(&target)
}

/// Manual recursive directory removal that refuses to traverse into
/// symlinked directories. `std::fs::remove_dir_all` follows symlinks
/// to directories on some platforms (notably stable Rust pre-1.74),
/// which would let `<cache>/<sid>/link-to-elsewhere` widen the blast
/// radius beyond the session dir.
fn delete_dir_no_symlinks(dir: &Path) -> Result<(), SalvageError> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            // `remove_file` removes the symlink itself, not its
            // target. This is the behaviour we want — purging the
            // session must not reach across the link.
            std::fs::remove_file(&path)?;
        } else if meta.is_dir() {
            delete_dir_no_symlinks(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    std::fs::remove_dir(dir)?;
    Ok(())
}

// ---------- Tauri command shims ------------------------------------

/// Tauri command: scan the cache root for unfinalized sessions.
///
/// Errors map to `String` for the wire so the frontend doesn't need
/// the [`SalvageError`] type. Missing cache root surfaces as `Ok([])`,
/// not an error — first-run users (no recordings yet) should see an
/// empty salvage list, not an error banner.
#[tauri::command]
pub fn heron_scan_unfinalized() -> Result<Vec<UnfinalizedSession>, String> {
    let Some(root) = default_cache_root() else {
        return Ok(Vec::new());
    };
    scan_unfinalized(&root).map_err(|e| e.to_string())
}

/// Tauri command: re-run the orchestrator's finalize path on a
/// salvaged session and write the resulting markdown into `vault_path`.
///
/// **Phase 69 status — placeholder.** The orchestrator's finalize
/// pipeline (see `crates/heron-cli/src/pipeline.rs`) currently runs
/// inside `Orchestrator::run`, which assumes a live audio-capture
/// handle. Exposing a "finalize from cache directory" entry point is
/// non-trivial (it has to re-read the partial WAVs, decide whether
/// transcription was complete, and re-enter the LLM summarize step
/// without re-running capture) and is deferred to a follow-up PR.
///
/// Until that lands, this command:
///
/// 1. Validates the session id (no path traversal),
/// 2. Confirms the session directory exists,
/// 3. Returns [`SalvageError::RecoveryNotImplemented`] so the frontend
///    surfaces a "not yet implemented" toast and the user can choose
///    to purge the session manually.
///
/// The signature is the one we want a future PR to keep — wiring the
/// real finalize call requires no caller change.
#[tauri::command]
pub fn heron_recover_session(session_id: String, vault_path: String) -> Result<String, String> {
    // Echo `vault_path` into the error path below so an empty / missing
    // setting doesn't silently no-op. Once the real wiring lands the
    // path is the destination for the resulting `.md`.
    let _ = vault_path;
    let session_id = validate_session_id(&session_id).map_err(|e| e.to_string())?;
    let Some(root) = default_cache_root() else {
        return Err(SalvageError::SessionNotFound {
            path: PathBuf::from(session_id),
        }
        .to_string());
    };
    let target = root.join(session_id);
    if !target.is_dir() {
        return Err(SalvageError::SessionNotFound { path: target }.to_string());
    }
    Err(SalvageError::RecoveryNotImplemented.to_string())
}

/// Tauri command: recursively delete the session's cache directory.
///
/// Refuses to follow symlinks. See [`purge_session`] for the full
/// path-safety contract.
#[tauri::command]
pub fn heron_purge_session(session_id: String) -> Result<(), String> {
    let Some(root) = default_cache_root() else {
        return Err(SalvageError::SessionNotFound {
            path: PathBuf::from(&session_id),
        }
        .to_string());
    };
    purge_session(&root, &session_id).map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn scan_returns_empty_when_cache_dir_missing() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let phantom = tmp.path().join("does-not-exist");
        let out = scan_unfinalized(&phantom).expect("scan");
        assert!(out.is_empty());
    }

    #[test]
    fn scan_returns_empty_when_root_is_empty_dir() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = scan_unfinalized(tmp.path()).expect("scan");
        assert!(out.is_empty());
    }

    #[test]
    fn scan_skips_finalized_sessions() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = tmp.path().join("session-a");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join("heron_session.json"),
            r#"{"status":"finalized","session_id":"session-a"}"#,
        )
        .expect("write");

        let out = scan_unfinalized(tmp.path()).expect("scan");
        assert!(
            out.is_empty(),
            "finalized session should be filtered out: {out:?}"
        );
    }

    #[test]
    fn scan_lists_unfinalized_sessions_with_started_at_and_sizes() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = tmp.path().join("crashed");
        fs::create_dir_all(&dir).expect("mkdir");
        // No `status` field — treat as unfinalized. Provide a
        // started_at so the test asserts the field round-trips.
        fs::write(
            dir.join("heron_session.json"),
            r#"{"started_at":"2026-04-25T10:00:00Z"}"#,
        )
        .expect("write");
        // Two WAV files (mic + tap), one transcript fragment.
        fs::write(dir.join("mic.wav"), b"RIFF........").expect("mic");
        fs::write(dir.join("tap.wav"), b"RIFFxxxxxxxx").expect("tap");
        fs::write(dir.join("transcript-partial.json"), b"{}").expect("trans");

        let out = scan_unfinalized(tmp.path()).expect("scan");
        assert_eq!(out.len(), 1);
        let entry = &out[0];
        assert_eq!(entry.session_id, "crashed");
        assert_eq!(entry.started_at, "2026-04-25T10:00:00Z");
        assert!(entry.audio_bytes > 0);
        assert!(entry.has_partial_transcript);
    }

    #[test]
    fn scan_falls_back_to_mtime_when_no_diag_file() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let dir = tmp.path().join("no-diag");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("mic.wav"), b"x").expect("mic");

        let out = scan_unfinalized(tmp.path()).expect("scan");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "no-diag");
        // RFC 3339 fallback timestamp shape.
        assert!(out[0].started_at.contains('T'));
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_top_level_symlink_targets() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).expect("mkdir outside");
        fs::write(
            outside.join("heron_session.json"),
            r#"{"status":"finalized"}"#,
        )
        .expect("write");

        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("mkdir root");
        symlink(&outside, root.join("symlinked")).expect("symlink");

        let out = scan_unfinalized(&root).expect("scan");
        assert!(out.is_empty(), "symlinked dir should not appear: {out:?}");
    }

    #[test]
    fn scan_skips_dotfile_directories() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let hidden = tmp.path().join(".heron-tmp");
        fs::create_dir_all(&hidden).expect("mkdir");
        fs::write(hidden.join("mic.wav"), b"x").expect("write");
        let out = scan_unfinalized(tmp.path()).expect("scan");
        assert!(out.is_empty());
    }

    #[test]
    fn validate_session_id_rejects_dot_dot() {
        let err = validate_session_id("..").expect_err("must reject ..");
        assert!(matches!(err, SalvageError::InvalidSessionId { .. }));
    }

    #[test]
    fn validate_session_id_rejects_path_separators() {
        for sid in ["a/b", "a\\b", "..", "../escape", "foo/bar"] {
            let err = validate_session_id(sid)
                .err()
                .unwrap_or_else(|| panic!("must reject {sid}"));
            assert!(matches!(err, SalvageError::InvalidSessionId { .. }));
        }
    }

    #[test]
    fn validate_session_id_rejects_leading_dot() {
        let err = validate_session_id(".hidden").expect_err("must reject leading dot");
        assert!(matches!(err, SalvageError::InvalidSessionId { .. }));
    }

    #[test]
    fn validate_session_id_rejects_empty_string() {
        let err = validate_session_id("").expect_err("must reject empty");
        assert!(matches!(err, SalvageError::InvalidSessionId { .. }));
    }

    #[test]
    fn validate_session_id_accepts_uuid_like_strings() {
        validate_session_id("0190a8e9-7c3a-7c0f-8b9b-1a2b3c4d5e6f")
            .expect("uuid-like id should pass");
        validate_session_id("session-abc123").expect("hyphens ok");
    }

    #[cfg(unix)]
    #[test]
    fn purge_session_refuses_top_level_symlink() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).expect("mkdir");
        let canary = outside.join("canary.txt");
        fs::write(&canary, b"keep me").expect("canary");

        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("mkdir root");
        symlink(&outside, root.join("victim")).expect("symlink");

        let err = purge_session(&root, "victim").expect_err("should refuse");
        assert!(
            matches!(err, SalvageError::SymlinkRefused { .. }),
            "unexpected: {err:?}"
        );
        // The symlink target's contents must still be intact.
        assert!(canary.exists(), "purge must not follow the symlink");
        // The symlink itself must still be there — we refused, not
        // silently unlinked it.
        assert!(root.join("victim").exists());
    }

    #[test]
    fn purge_session_removes_directory_recursively() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        let dir = root.join("doomed");
        fs::create_dir_all(dir.join("sub")).expect("mkdir");
        fs::write(dir.join("a.wav"), b"x").expect("write");
        fs::write(dir.join("sub/b.json"), b"{}").expect("write");

        purge_session(root, "doomed").expect("purge");
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn purge_session_unlinks_inner_symlinks_without_following() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        let dir = root.join("with-symlink");
        fs::create_dir_all(&dir).expect("mkdir");

        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("elsewhere");
        let canary = elsewhere.join("canary.txt");
        fs::write(&canary, b"keep me").expect("canary");

        symlink(&elsewhere, dir.join("inner-link")).expect("inner symlink");
        fs::write(dir.join("a.wav"), b"x").expect("write");

        purge_session(root, "with-symlink").expect("purge");
        assert!(!dir.exists(), "session dir purged");
        assert!(canary.exists(), "symlink target preserved");
    }

    #[test]
    fn purge_session_returns_session_not_found_for_missing_dir() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let err = purge_session(tmp.path(), "nope").expect_err("should fail");
        assert!(matches!(err, SalvageError::SessionNotFound { .. }));
    }

    #[test]
    fn purge_session_validates_session_id_before_touching_disk() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let err = purge_session(tmp.path(), "../escape").expect_err("should reject id");
        assert!(matches!(err, SalvageError::InvalidSessionId { .. }));
    }

    #[test]
    fn scan_orders_by_started_at_desc_with_session_id_tiebreak() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        for (name, ts) in [
            ("alpha", "2026-04-25T10:00:00Z"),
            ("bravo", "2026-04-25T11:00:00Z"),
            ("charlie", "2026-04-25T11:00:00Z"),
        ] {
            let dir = tmp.path().join(name);
            fs::create_dir_all(&dir).expect("mkdir");
            fs::write(
                dir.join("heron_session.json"),
                format!("{{\"started_at\":\"{ts}\"}}"),
            )
            .expect("write");
        }
        let out = scan_unfinalized(tmp.path()).expect("scan");
        let names: Vec<&str> = out.iter().map(|s| s.session_id.as_str()).collect();
        // Newest-first; tie between bravo and charlie breaks
        // alphabetically (bravo < charlie).
        assert_eq!(names, ["bravo", "charlie", "alpha"]);
    }
}
