//! Crash-recovery state persistence per
//! [`docs/implementation.md`](../../../docs/implementation.md) §14.3.
//!
//! While a session is live, the orchestrator drops a small JSON file
//! at `<cache_dir>/<session_id>/state.json`. On launch, the desktop
//! shell calls [`discover_unfinished`] over the cache root: any state
//! file whose phase is not [`SessionPhase::Done`] surfaces as a
//! salvage candidate.
//!
//! The format is tiny on purpose — counts and paths only, no audio,
//! no transcript text — so a SIGKILL that catches us mid-write loses
//! at most one phase transition's worth of metadata. The transcript
//! and recording-buffer files on disk are the source of truth for
//! salvage; this state file just points at them.
//!
//! ## Atomicity
//!
//! [`write_state`] writes via UUID-suffixed temp file + `fsync` +
//! `rename`. This mirrors `heron_vault::atomic_write` but is
//! intentionally duplicated here so `heron-types` stays free of a
//! dep on `heron-vault` (which carries the EventKit Swift bridge).
//! On non-unix targets (Windows / Linux v2), the file mode is set
//! best-effort via the platform default.
//!
//! ## Wire stability
//!
//! Bump [`STATE_VERSION`] on any rename / type change of an existing
//! field. Adding optional fields is non-breaking. Readers skip records
//! whose `state_version` they don't recognize so a downgrade can't
//! crash on unknown fields — see [`read_state`].

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::SessionId;

/// Stable schema version. Bump on any rename / type change of an
/// existing field. New optional fields do *not* bump.
pub const STATE_VERSION: u32 = 1;

/// Filename inside `<cache_dir>/<session_id>/` holding the record.
pub const STATE_FILE_NAME: &str = "state.json";

/// Hard cap on `state.json` size when reading. Real records are well
/// under 1 KiB; anything bigger is either a runaway buggy writer or
/// an attacker-planted file we shouldn't slurp into memory. Per the
/// "file reads from untrusted sources" rule in `CONTRIBUTING.md`.
pub const MAX_STATE_FILE_BYTES: u64 = 1 << 20;

/// Phase the session was in when [`write_state`] last fired. Recovery
/// reads this to decide what to surface to the user.
///
/// Marked `#[non_exhaustive]` so adding a phase in a future minor is
/// non-breaking for downstream `match`es — a new variant only needs
/// to be terminal-or-not via [`SessionPhase::is_unfinished`], not a
/// schema bump.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionPhase {
    /// User pressed hotkey; consent banner up; audio not yet rolling.
    Armed,
    /// Audio capture is live; ringbuffer accumulating frames.
    Recording,
    /// Capture stopped; STT transcription in progress.
    Transcribing,
    /// Transcript ready; LLM summarize in progress.
    Summarizing,
    /// Markdown note written; safe to drop the cache dir.
    Done,
}

impl SessionPhase {
    /// `true` when [`discover_unfinished`] should surface this phase
    /// as a salvage candidate. `Done` is the only finished state.
    pub fn is_unfinished(self) -> bool {
        !matches!(self, SessionPhase::Done)
    }
}

/// One on-disk record. Kept narrow on purpose (paths + counts only,
/// no audio or transcript text) so a partial write only loses
/// metadata, not user content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionStateRecord {
    pub state_version: u32,
    pub session_id: SessionId,
    pub started_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
    pub source_app: String,
    pub cache_dir: PathBuf,
    pub phase: SessionPhase,
    /// Bytes written to `mic.wav` so far. Lets the salvage UI estimate
    /// recoverable duration without opening the WAV.
    #[serde(default)]
    pub mic_bytes_written: u64,
    /// Bytes written to `tap.wav` so far.
    #[serde(default)]
    pub tap_bytes_written: u64,
    /// Number of finalized turns in `transcript.jsonl`.
    #[serde(default)]
    pub turns_finalized: u32,
}

impl SessionStateRecord {
    /// Build a fresh record at [`SessionPhase::Armed`]. Caller still
    /// has to [`write_state`] it for the salvage flow to see it.
    pub fn new_armed(
        session_id: SessionId,
        cache_dir: PathBuf,
        source_app: String,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            state_version: STATE_VERSION,
            session_id,
            started_at: now,
            last_updated: now,
            source_app,
            cache_dir,
            phase: SessionPhase::Armed,
            mic_bytes_written: 0,
            tap_bytes_written: 0,
            turns_finalized: 0,
        }
    }

    /// Path the record was/will-be persisted to.
    pub fn state_file_path(&self) -> PathBuf {
        self.cache_dir.join(STATE_FILE_NAME)
    }
}

/// Errors from this module's I/O surface. Wraps `io::Error` and the
/// JSON parse error so callers can distinguish "file missing" from
/// "file corrupt" without inspecting `kind()` strings.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("state.json could not be parsed at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("state path has no parent directory: {0}")]
    NoParent(PathBuf),
    #[error(
        "state.json at {path} exceeds the {max}-byte cap (file is {bytes} bytes); \
         refusing to load",
        max = MAX_STATE_FILE_BYTES,
    )]
    TooLarge { path: PathBuf, bytes: u64 },
}

/// Atomically write `record` to `<record.cache_dir>/state.json`. Creates
/// the cache dir if it doesn't exist; uses a UUID-suffixed temp file +
/// `fsync` + `rename` so a SIGKILL mid-write leaves either the previous
/// record or the new one, never a half-written file.
///
/// **Single-writer invariant.** The orchestrator owns its session and
/// must not call `write_state` from two threads on the same record —
/// `rename` is last-writer-wins, so concurrent writes can regress
/// `last_updated` / `phase`. `read_state` callers are unconstrained.
pub fn write_state(record: &SessionStateRecord) -> Result<(), RecoveryError> {
    fs::create_dir_all(&record.cache_dir)?;
    let path = record.state_file_path();
    let bytes = serde_json::to_vec_pretty(record).map_err(|source| parse_err(&path, source))?;
    atomic_write(&path, &bytes)
}

/// Read `<cache_dir>/state.json`. Returns `Ok(None)` if the file
/// doesn't exist (a clean session that never started, or one whose
/// directory was purged). Returns `Ok(None)` *also* when the record's
/// `state_version` is newer than this binary's [`STATE_VERSION`] — a
/// downgrade scenario where we'd rather skip the record than misread
/// it.
///
/// File reads cap at [`MAX_STATE_FILE_BYTES`]; over-cap files surface
/// as [`RecoveryError::TooLarge`] so a runaway writer (or planted
/// payload) can't blow up startup memory.
pub fn read_state(cache_dir: &Path) -> Result<Option<SessionStateRecord>, RecoveryError> {
    let path = cache_dir.join(STATE_FILE_NAME);
    let f = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RecoveryError::Io(e)),
    };

    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > MAX_STATE_FILE_BYTES {
        return Err(RecoveryError::TooLarge {
            path: path.clone(),
            bytes: len,
        });
    }

    let mut buf = String::with_capacity(len.min(MAX_STATE_FILE_BYTES) as usize);
    f.take(MAX_STATE_FILE_BYTES + 1)
        .read_to_string(&mut buf)
        .map_err(RecoveryError::Io)?;
    if buf.len() as u64 > MAX_STATE_FILE_BYTES {
        return Err(RecoveryError::TooLarge {
            path,
            bytes: buf.len() as u64,
        });
    }

    // Peek at state_version first so a future-version record doesn't
    // explode the strict deserializer when fields it doesn't know
    // about appear.
    #[derive(Deserialize)]
    struct VersionPeek {
        state_version: u32,
    }
    let peek: VersionPeek =
        serde_json::from_str(&buf).map_err(|source| parse_err(&path, source))?;
    if peek.state_version > STATE_VERSION {
        return Ok(None);
    }

    let rec: SessionStateRecord =
        serde_json::from_str(&buf).map_err(|source| parse_err(&path, source))?;
    Ok(Some(rec))
}

/// Walk `cache_root` one level deep, reading each subdirectory's
/// `state.json`. Returns the records whose phase is unfinished, sorted
/// by `started_at` ascending (oldest first) so the salvage UI surfaces
/// the most-stale candidates at the top.
///
/// Sub-directories without a state file are silently skipped (a
/// session that never wrote one is not unfinished — there's nothing
/// to recover). Sub-directories whose `state.json` is corrupt, too
/// large, or fails an I/O read are still skipped (recovery can't
/// block startup), but the count is returned via the error log path
/// of the caller's choice — at this layer the caller gets what we
/// could read.
///
/// If two records claim the same `session_id` (a crash-on-launch
/// scenario), the one with the freshest `last_updated` wins.
pub fn discover_unfinished(cache_root: &Path) -> Result<Vec<SessionStateRecord>, RecoveryError> {
    let entries = match fs::read_dir(cache_root) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RecoveryError::Io(e)),
    };

    let mut by_id: HashMap<SessionId, SessionStateRecord> = HashMap::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }
        let rec = match read_state(&entry.path()) {
            Ok(Some(rec)) if rec.phase.is_unfinished() => rec,
            Ok(_) => continue,
            // Corrupt / unreadable / oversized: skip rather than
            // fail the whole walk. Recovery must never block
            // startup on a stale partial directory.
            Err(_) => continue,
        };
        by_id
            .entry(rec.session_id)
            .and_modify(|existing| {
                if rec.last_updated > existing.last_updated {
                    *existing = rec.clone();
                }
            })
            .or_insert(rec);
    }
    let mut out: Vec<SessionStateRecord> = by_id.into_values().collect();
    out.sort_by_key(|r| r.started_at);
    Ok(out)
}

/// Helper: every JSON parse error in this module wants the same
/// `Parse { path, source }` shape. Avoids three near-identical
/// closure call sites.
fn parse_err(path: &Path, source: serde_json::Error) -> RecoveryError {
    RecoveryError::Parse {
        path: path.to_path_buf(),
        source,
    }
}

/// Drop-guard that removes the temp file unless `commit()` is called.
/// Without this, a failure between create + rename leaves
/// `.state-<uuid>.tmp` files that accumulate in every session dir.
struct TempCleanup<'a> {
    path: &'a Path,
    committed: bool,
}

impl TempCleanup<'_> {
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for TempCleanup<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort: nothing useful to do if cleanup itself
            // fails (the temp file was about to be replaced anyway).
            let _ = fs::remove_file(self.path);
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), RecoveryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RecoveryError::NoParent(path.to_path_buf()))?;
    let temp = parent.join(format!(".state-{}.tmp", Uuid::now_v7()));
    let cleanup = TempCleanup {
        path: &temp,
        committed: false,
    };
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        // 0600 on unix; a no-op on other platforms (heron is macOS-
        // only in v1; Windows / Linux follow once they're shipped).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&temp, path)?;
    cleanup.commit();
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn rec_at(cache_dir: PathBuf, phase: SessionPhase, age_secs: i64) -> SessionStateRecord {
        let now = Utc::now() - chrono::Duration::seconds(age_secs);
        SessionStateRecord {
            state_version: STATE_VERSION,
            // Tests that need to share a session_id (dedup test)
            // overwrite this after construction; the helper hands
            // out a distinct uuid by default so unrelated tests
            // don't accidentally collide under the dedup logic.
            session_id: SessionId::now_v7(),
            started_at: now,
            last_updated: now,
            source_app: "us.zoom.xos".into(),
            cache_dir,
            phase,
            mic_bytes_written: 0,
            tap_bytes_written: 0,
            turns_finalized: 0,
        }
    }

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tmpdir();
        let mut rec = rec_at(dir.path().to_path_buf(), SessionPhase::Recording, 0);
        rec.mic_bytes_written = 1_234_567;
        rec.tap_bytes_written = 7_654_321;
        rec.turns_finalized = 42;

        write_state(&rec).expect("write");
        let read = read_state(dir.path()).expect("read").expect("present");
        assert_eq!(read, rec);
    }

    #[test]
    fn read_returns_none_when_file_missing() {
        let dir = tmpdir();
        let read = read_state(dir.path()).expect("read");
        assert!(read.is_none());
    }

    #[test]
    fn read_returns_none_for_future_state_version() {
        // Forward-compat: a newer binary may write state_version: 99.
        // The current binary should skip it gracefully so an older
        // launch after a newer crash doesn't blow up.
        let dir = tmpdir();
        let path = dir.path().join(STATE_FILE_NAME);
        let json = r#"{"state_version":99,"session_id":"00000000-0000-0000-0000-000000000000","started_at":"2026-04-25T00:00:00Z","last_updated":"2026-04-25T00:00:00Z","source_app":"us.zoom.xos","cache_dir":"/tmp","phase":"recording","fields_we_dont_know":"yet"}"#;
        std::fs::write(&path, json).expect("write");
        let read = read_state(dir.path()).expect("read");
        assert!(read.is_none(), "future version should be skipped");
    }

    #[test]
    fn read_propagates_corrupt_json_as_parse_error() {
        let dir = tmpdir();
        let path = dir.path().join(STATE_FILE_NAME);
        std::fs::write(&path, "{ this is not json").expect("write");
        let err = read_state(dir.path()).expect_err("corrupt should error");
        assert!(matches!(err, RecoveryError::Parse { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        let rec = rec_at(dir.path().to_path_buf(), SessionPhase::Armed, 0);
        write_state(&rec).expect("write");
        let mode = std::fs::metadata(rec.state_file_path())
            .expect("metadata")
            .permissions()
            .mode();
        // rwx triples masked: only the owner-rw bits should be set.
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
    }

    #[test]
    fn write_overwrites_atomically_without_partial_state() {
        // Two writes in sequence: the second must replace the first
        // without ever leaving an empty or half-written state.json on
        // disk that a concurrent reader could observe.
        let dir = tmpdir();
        let mut rec = rec_at(dir.path().to_path_buf(), SessionPhase::Armed, 0);
        write_state(&rec).expect("v1");
        let v1 = std::fs::read(rec.state_file_path()).expect("v1 bytes");

        rec.phase = SessionPhase::Recording;
        rec.last_updated = Utc::now();
        write_state(&rec).expect("v2");
        let v2 = std::fs::read(rec.state_file_path()).expect("v2 bytes");

        assert_ne!(v1, v2, "rewrite must change file bytes");
        let read = read_state(dir.path()).expect("read").expect("present");
        assert_eq!(read.phase, SessionPhase::Recording);
    }

    #[test]
    fn discover_unfinished_skips_done_and_sorts_oldest_first() {
        let root = tmpdir();
        let oldest = root.path().join("aaa");
        let newest = root.path().join("zzz");
        let finished = root.path().join("mmm");
        for d in [&oldest, &newest, &finished] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        write_state(&rec_at(oldest.clone(), SessionPhase::Recording, 600)).expect("oldest");
        write_state(&rec_at(newest.clone(), SessionPhase::Transcribing, 60)).expect("newest");
        write_state(&rec_at(finished.clone(), SessionPhase::Done, 300)).expect("finished");

        let found = discover_unfinished(root.path()).expect("walk");
        assert_eq!(found.len(), 2, "Done is filtered out");
        assert_eq!(found[0].cache_dir, oldest);
        assert_eq!(found[1].cache_dir, newest);
    }

    #[test]
    fn discover_unfinished_silently_skips_corrupt_records() {
        let root = tmpdir();
        let good = root.path().join("good");
        let bad = root.path().join("bad");
        for d in [&good, &bad] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        write_state(&rec_at(good.clone(), SessionPhase::Recording, 30)).expect("good");
        std::fs::write(bad.join(STATE_FILE_NAME), "garbage").expect("bad write");

        let found = discover_unfinished(root.path()).expect("walk");
        assert_eq!(found.len(), 1, "corrupt entry must not break the walk");
        assert_eq!(found[0].cache_dir, good);
    }

    #[test]
    fn discover_unfinished_returns_empty_when_root_missing() {
        let dir = tmpdir();
        let phantom = dir.path().join("does-not-exist");
        let found = discover_unfinished(&phantom).expect("walk");
        assert!(found.is_empty());
    }

    #[test]
    fn discover_unfinished_ignores_non_directory_entries() {
        let root = tmpdir();
        // A stray file at the root level is not a session dir; the
        // walker must not try to read state.json from it.
        std::fs::write(root.path().join("README.md"), "just notes").expect("file");
        let dir_entry = root.path().join("active");
        std::fs::create_dir_all(&dir_entry).expect("mkdir");
        write_state(&rec_at(dir_entry, SessionPhase::Recording, 0)).expect("write");
        let found = discover_unfinished(root.path()).expect("walk");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn record_is_unfinished_for_every_non_done_phase() {
        // Pin the contract: any future SessionPhase variant must
        // explicitly opt out of `is_unfinished` if it's terminal, so
        // a forgotten match arm doesn't silently lose salvage signals.
        for p in [
            SessionPhase::Armed,
            SessionPhase::Recording,
            SessionPhase::Transcribing,
            SessionPhase::Summarizing,
        ] {
            assert!(p.is_unfinished(), "{p:?} should be unfinished");
        }
        assert!(!SessionPhase::Done.is_unfinished());
    }

    #[test]
    fn read_state_rejects_file_over_cap() {
        let dir = tmpdir();
        let path = dir.path().join(STATE_FILE_NAME);
        // Plant a 2 MiB file — well over the 1 MiB cap. Body doesn't
        // need to be valid JSON; the cap fires before parse.
        let payload = vec![b'{'; (MAX_STATE_FILE_BYTES + 1024) as usize];
        std::fs::write(&path, &payload).expect("write");
        let err = read_state(dir.path()).expect_err("over-cap should error");
        assert!(matches!(err, RecoveryError::TooLarge { .. }));
    }

    #[test]
    fn atomic_write_does_not_leak_temp_file_on_success() {
        // After a successful write, no `.state-*.tmp` should remain
        // beside the final state.json.
        let dir = tmpdir();
        let rec = rec_at(dir.path().to_path_buf(), SessionPhase::Recording, 0);
        write_state(&rec).expect("write");
        let temps: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".state-"))
            .collect();
        assert!(temps.is_empty(), "leaked tmps: {temps:?}");
    }

    #[test]
    fn discover_unfinished_dedupes_duplicate_session_ids_keeping_freshest() {
        // Crash-on-launch can leave two cache dirs claiming the same
        // session_id. The salvage UI should see one, not two.
        let root = tmpdir();
        let stale = root.path().join("a");
        let fresh = root.path().join("b");
        for d in [&stale, &fresh] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        let stale_rec = rec_at(stale.clone(), SessionPhase::Recording, 600);
        let mut fresh_rec = rec_at(fresh.clone(), SessionPhase::Transcribing, 60);
        // Force same session_id; vary last_updated.
        fresh_rec.session_id = stale_rec.session_id;
        write_state(&stale_rec).expect("stale");
        write_state(&fresh_rec).expect("fresh");

        let found = discover_unfinished(root.path()).expect("walk");
        assert_eq!(found.len(), 1, "duplicate session_ids must collapse");
        assert_eq!(found[0].cache_dir, fresh, "freshest last_updated wins");
    }

    #[test]
    fn session_phase_round_trips_via_serde_snake_case() {
        for (phase, expected) in [
            (SessionPhase::Armed, r#""armed""#),
            (SessionPhase::Recording, r#""recording""#),
            (SessionPhase::Transcribing, r#""transcribing""#),
            (SessionPhase::Summarizing, r#""summarizing""#),
            (SessionPhase::Done, r#""done""#),
        ] {
            let s = serde_json::to_string(&phase).expect("ser");
            assert_eq!(s, expected);
            let back: SessionPhase = serde_json::from_str(&s).expect("de");
            assert_eq!(back, phase);
        }
    }
}
