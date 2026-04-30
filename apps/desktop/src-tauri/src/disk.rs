//! Vault disk-usage gauge + audio-retention purge per §16.1 (Audio tab).
//!
//! Phase 68 (PR-ζ) ships two read/write surfaces for the Settings pane's
//! Audio tab:
//!
//! - [`disk_usage`] — sums byte counts and counts sessions across
//!   `<vault>/*.md` plus the co-located `.wav` / `.m4a` (and the `.bak`
//!   PR-ε leaves behind on note-edits) so the UI can render
//!   "1.4 GB across 38 sessions".
//! - [`purge_audio_older_than`] — deletes `.wav` / `.m4a` files whose
//!   `mtime` is older than the configured retention window. The
//!   matching `.md` summary is **always** kept; only the lossy/lossless
//!   audio sidecars are candidates.
//! - [`purge_summaries_older_than`] — Tier 4 sibling of the audio
//!   sweeper: deletes `.md` summary files whose `mtime` is older than
//!   the `Settings.summary_retention_days` window. The two predicates
//!   are mutually exclusive — the audio sweeper never touches `.md`,
//!   and the summary sweeper never touches `.wav` / `.m4a` / `.bak`.
//!
//! ## Path-safety contract
//!
//! Both surfaces canonicalize the vault root before walking. The walk
//! is **non-recursive** (vault layout per `notes.rs` is flat — sessions
//! live at the vault root) so we don't need to guard against deep
//! symlink loops, but a top-level entry that *is* a symlink is
//! refused: a malicious or misconfigured vault should not let a `.wav`
//! purge delete a file outside the vault. The check is per-entry via
//! `Metadata::file_type().is_symlink()` (which does **not** dereference,
//! unlike `Path::is_symlink`).
//!
//! ## Why a fixed extension allow-list, not "everything not `.md`"
//!
//! The vault holds user-curated markdown plus a small set of audio
//! sidecars. Any future telemetry/backup file that isn't `.wav` /
//! `.m4a` should default to "do not delete" — the conservative bias is
//! to never surprise the user. The allow-list lives in
//! [`AUDIO_EXTENSIONS`] so adding a future codec (e.g. `.opus`)
//! requires an explicit code change + test update.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use thiserror::Error;

/// File extensions PR-ζ considers "audio sidecars" — the only
/// candidates [`purge_audio_older_than`] will delete.
///
/// Compared in lowercase against `Path::extension()`'s lossy UTF-8
/// view; mixed-case (`Foo.WAV`) is treated as audio.
const AUDIO_EXTENSIONS: &[&str] = &["wav", "m4a"];

/// File extensions Tier 4 considers "summary notes" — the only
/// candidates [`purge_summaries_older_than`] will delete.
///
/// Disjoint from [`AUDIO_EXTENSIONS`] by construction: the summary
/// sweeper must never delete an audio sidecar, and the audio sweeper
/// must never delete a summary. The
/// `summary_extensions_disjoint_from_audio_extensions` test pins the
/// invariant so a future maintainer who accidentally adds `wav` here
/// (or `md` to the audio list) sees a red test, not a data-loss bug.
const SUMMARY_EXTENSIONS: &[&str] = &["md"];

/// Extensions that count toward the "session count" gauge. PR-ε ships
/// `.bak` next to edited notes; counting them here would double-count
/// a session, so the gauge derives session count from `.md` files
/// alone. The `.bak` is still summed into `vault_bytes` as part of
/// total disk consumption.
const NOTE_EXTENSIONS: &[&str] = &["md"];

/// File extensions whose bytes contribute to the vault disk-usage
/// total. Includes `.md`, the audio sidecars, and PR-ε's `.bak` so a
/// freshly-edited session fully accounts for itself.
const TOTAL_BYTES_EXTENSIONS: &[&str] = &["md", "wav", "m4a", "bak"];

#[derive(Debug, Error)]
pub enum DiskError {
    #[error("vault path could not be canonicalized: {0}")]
    Canonicalize(std::io::Error),
    #[error("vault path is not a directory: {0}")]
    NotADirectory(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Wire-format struct returned by [`disk_usage`]. Field names are
/// snake_case to match the rest of `apps/desktop/src-tauri` so the
/// frontend type in `lib/invoke.ts` mirrors the Rust shape verbatim.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiskUsage {
    /// Total bytes across `.md`, `.wav`, `.m4a`, and `.bak` files at
    /// the vault root. The Settings pane humanizes this for display.
    pub vault_bytes: u64,
    /// Number of `.md` files at the vault root. Each `.md` is one
    /// session per the `notes.rs` layout.
    pub vault_session_count: u32,
}

/// Sum disk usage and count sessions at the vault root.
///
/// Returns `(0, 0)` if the vault is empty rather than erroring — an
/// empty vault is the first-run state, not a fault condition. Returns
/// an error only if the vault path itself is missing, not a directory,
/// or unreadable (e.g. permissions).
pub fn disk_usage(vault: &Path) -> Result<DiskUsage, DiskError> {
    let vault = canonical_vault(vault)?;

    let mut bytes: u64 = 0;
    let mut sessions: u32 = 0;

    for entry in fs::read_dir(&vault)? {
        let entry = entry?;
        // `metadata()` follows symlinks, but the policy here is to skip
        // any top-level symlink outright — gauges should not show
        // bytes belonging to a directory the vault merely points at.
        // We use `file_type()` (cheap, no traversal) instead.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let ext = lowercase_extension(&path);

        if TOTAL_BYTES_EXTENSIONS.contains(&ext.as_str()) {
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
        if NOTE_EXTENSIONS.contains(&ext.as_str()) {
            sessions = sessions.saturating_add(1);
        }
    }

    Ok(DiskUsage {
        vault_bytes: bytes,
        vault_session_count: sessions,
    })
}

/// Delete `.wav` / `.m4a` files at the vault root whose `mtime` is
/// older than `days` days ago. Returns the count actually purged.
///
/// `.md` summaries and `.bak` rollbacks are never deleted. Symlinks
/// are skipped wholesale. A `days = 0` argument is treated as
/// "everything is older than 0 days, purge it all" — the Settings
/// pane should not let this through (the form clamps to ≥ 1) but the
/// command is permissive about it for tests + scripted purges.
pub fn purge_audio_older_than(vault: &Path, days: u32) -> Result<u32, DiskError> {
    purge_by_extension(vault, days, AUDIO_EXTENSIONS)
}

/// Delete `.md` summary files at the vault root whose `mtime` is
/// older than `days` days ago. Returns the count actually purged.
///
/// Tier 4 sibling of [`purge_audio_older_than`]. Mirrors the same
/// shape: walk the (flat) vault, match against an extension allow-
/// list, delete entries older than the cutoff, return the count.
///
/// **Cross-sweeper contract:** `.wav` / `.m4a` audio sidecars are
/// **never** candidates. The two sweepers' allow-lists are disjoint
/// by construction (see [`SUMMARY_EXTENSIONS`] vs
/// [`AUDIO_EXTENSIONS`]) and the
/// `summary_extensions_disjoint_from_audio_extensions` test pins the
/// invariant. A regression that conflated the two predicates
/// (e.g. a future refactor that shared a single allow-list) would
/// cause the summary sweeper to wipe audio sidecars at the same age,
/// silently widening the user's data-loss window. The
/// `purge_summaries_keeps_audio_deletes_old_md` test catches that.
///
/// Symlinks are skipped wholesale. A `days = 0` argument is permissive
/// (purge everything), matching the audio sweeper's semantics — the
/// Settings pane is expected to clamp at the form layer.
pub fn purge_summaries_older_than(vault: &Path, days: u32) -> Result<u32, DiskError> {
    purge_by_extension(vault, days, SUMMARY_EXTENSIONS)
}

/// Shared walk for the two retention sweepers. Centralizing the
/// canonicalize → cutoff → walk → filter-by-extension → delete loop
/// keeps the two surfaces identical except for the allow-list, so a
/// fix to one (concurrent-delete handling, symlink policy, cutoff
/// arithmetic) automatically applies to the other.
fn purge_by_extension(vault: &Path, days: u32, allowed: &[&str]) -> Result<u32, DiskError> {
    let vault = canonical_vault(vault)?;
    let now = SystemTime::now();
    // `u64` seconds in a day fits trivially; use `u64` so the
    // multiplication can't overflow even at the `u32::MAX` extreme.
    let threshold_secs = u64::from(days).saturating_mul(86_400);
    let cutoff = now
        .checked_sub(Duration::from_secs(threshold_secs))
        // If the user asks for a window so large it pre-dates the unix
        // epoch, every file qualifies — fall back to `UNIX_EPOCH` so
        // the comparison stays well-defined.
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut purged: u32 = 0;
    for entry in fs::read_dir(&vault)? {
        let entry = entry?;
        // Concurrency: a sibling process may delete an entry between
        // `read_dir` returning it and our `file_type` / `metadata`
        // probes. Treat `NotFound` from the probes as benign (continue
        // the sweep) — same posture the `remove_file` arm below has
        // taken since PR-ζ. Other IO errors still abort the sweep.
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(DiskError::Io(e)),
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        // Filter on the file *name*'s extension before allocating the
        // full `PathBuf`: most vaults will have non-matching entries
        // (`.bak`, `.txt`, `.DS_Store`) and `entry.path()` is the only
        // allocating step in the loop body.
        let file_name = entry.file_name();
        let ext = lowercase_extension(Path::new(&file_name));
        if !allowed.contains(&ext.as_str()) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(DiskError::Io(e)),
        };
        let mtime = metadata.modified()?;
        if mtime <= cutoff {
            let path = entry.path();
            // `remove_file` returns NotFound if a concurrent process
            // already deleted the file — treat that as "someone
            // beat us to it" rather than failing the whole purge.
            match fs::remove_file(&path) {
                Ok(()) => purged = purged.saturating_add(1),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(DiskError::Io(e)),
            }
        }
    }
    Ok(purged)
}

/// Resolve `vault` to an absolute, symlink-free path and confirm it
/// points at a directory. Centralized so both surfaces share the same
/// guarantees + error mapping.
fn canonical_vault(vault: &Path) -> Result<PathBuf, DiskError> {
    let canonical = fs::canonicalize(vault).map_err(DiskError::Canonicalize)?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_dir() {
        return Err(DiskError::NotADirectory(canonical));
    }
    Ok(canonical)
}

/// Lowercase extension (without the dot) or empty string. Centralized
/// so the case-fold rule + lossy UTF-8 fallback are obvious; the
/// per-call cost (heap-allocating a tiny String) is negligible against
/// the syscalls already in flight.
fn lowercase_extension(path: &Path) -> String {
    path.extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use filetime::{FileTime, set_file_mtime};

    fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, contents).expect("write fixture");
        p
    }

    fn backdate(path: &Path, days_ago: u64) {
        // `checked_mul` rather than the bare `*`: clippy's
        // `arithmetic_side_effects` lint isn't enabled at the
        // workspace level today, but the saturating form documents
        // intent + matches the production `purge_audio_older_than`
        // arithmetic.
        let secs = days_ago.saturating_mul(86_400);
        let target = SystemTime::now() - Duration::from_secs(secs);
        let ft = FileTime::from_system_time(target);
        set_file_mtime(path, ft).expect("backdate mtime");
    }

    #[test]
    fn disk_usage_counts_md_and_audio_at_vault_root() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_file(tmp.path(), "session-1.md", b"hello");
        write_file(tmp.path(), "session-1.wav", &[0u8; 1024]);
        write_file(tmp.path(), "session-2.md", b"world");
        write_file(tmp.path(), "session-2.m4a", &[0u8; 2048]);

        let usage = disk_usage(tmp.path()).expect("usage");
        assert_eq!(usage.vault_session_count, 2);
        assert_eq!(usage.vault_bytes, 5 + 1024 + 5 + 2048);
    }

    #[test]
    fn disk_usage_includes_bak_rollback_in_total_bytes() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_file(tmp.path(), "session-1.md", b"v2"); // 2 bytes
        write_file(tmp.path(), "session-1.md.bak", b"v1-old"); // 6 bytes

        let usage = disk_usage(tmp.path()).expect("usage");
        // `.bak` doesn't count toward sessions but does count toward
        // bytes — the user wants to see total disk consumption, not
        // just the live session payload.
        assert_eq!(usage.vault_session_count, 1);
        assert_eq!(usage.vault_bytes, 2 + 6);
    }

    #[test]
    fn disk_usage_ignores_unrelated_extensions() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_file(tmp.path(), "session-1.md", b"a");
        write_file(tmp.path(), "stash.txt", b"unrelated");
        write_file(tmp.path(), ".DS_Store", &[0u8; 64]);

        let usage = disk_usage(tmp.path()).expect("usage");
        // Only `session-1.md` (1 byte) counts toward both gauges; the
        // `.txt` + dot-file are skipped by the extension allow-list
        // even though they live at the vault root.
        assert_eq!(usage.vault_session_count, 1);
        assert_eq!(usage.vault_bytes, 1);
    }

    #[test]
    fn disk_usage_empty_vault_returns_zero_zero() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let usage = disk_usage(tmp.path()).expect("usage");
        assert_eq!(usage.vault_session_count, 0);
        assert_eq!(usage.vault_bytes, 0);
    }

    #[test]
    fn disk_usage_errors_when_vault_is_a_file() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let f = write_file(tmp.path(), "not-a-vault.md", b"x");
        let err = disk_usage(&f).expect_err("file should not be a vault");
        match err {
            DiskError::NotADirectory(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn disk_usage_errors_when_vault_does_not_exist() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let missing = tmp.path().join("nope");
        let err = disk_usage(&missing).expect_err("missing should error");
        match err {
            DiskError::Canonicalize(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn purge_keeps_md_deletes_old_audio() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let md = write_file(tmp.path(), "old.md", b"never-purged");
        let wav_old = write_file(tmp.path(), "old.wav", &[0u8; 8]);
        let m4a_old = write_file(tmp.path(), "old.m4a", &[0u8; 8]);
        let wav_new = write_file(tmp.path(), "new.wav", &[0u8; 8]);

        backdate(&wav_old, 10);
        backdate(&m4a_old, 10);
        backdate(&md, 365);
        // wav_new is fresh — leave its mtime alone.

        let purged = purge_audio_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 2);
        assert!(md.exists(), "md must survive even when ancient");
        assert!(!wav_old.exists());
        assert!(!m4a_old.exists());
        assert!(wav_new.exists(), "fresh audio must survive");
    }

    #[test]
    fn purge_skips_unrelated_extensions_even_when_old() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let bak = write_file(tmp.path(), "session.md.bak", b"old-rollback");
        let txt = write_file(tmp.path(), "stash.txt", b"old-note");
        backdate(&bak, 100);
        backdate(&txt, 100);

        let purged = purge_audio_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 0);
        assert!(bak.exists(), ".bak must not be purged");
        assert!(txt.exists(), ".txt must not be purged");
    }

    #[test]
    fn purge_zero_days_deletes_all_audio() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let wav = write_file(tmp.path(), "fresh.wav", &[0u8; 8]);
        let purged = purge_audio_older_than(tmp.path(), 0).expect("purge");
        assert_eq!(purged, 1);
        assert!(!wav.exists());
    }

    #[test]
    fn purge_returns_zero_when_nothing_to_purge() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_file(tmp.path(), "session.md", b"x");
        let purged = purge_audio_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 0);
    }

    /// Implicit invariant: every extension in [`AUDIO_EXTENSIONS`]
    /// must also be in [`TOTAL_BYTES_EXTENSIONS`]. Otherwise the gauge
    /// would understate disk usage for an extension we still purge,
    /// and "Purged 38 files freeing N bytes" would report N as 0.
    /// Adding `.opus` next year? Update both lists; this test catches
    /// a half-update.
    #[test]
    fn audio_extensions_are_subset_of_total_bytes_extensions() {
        for ext in AUDIO_EXTENSIONS {
            assert!(
                TOTAL_BYTES_EXTENSIONS.contains(ext),
                "{ext} is in AUDIO_EXTENSIONS but missing from TOTAL_BYTES_EXTENSIONS"
            );
        }
    }

    /// Sister invariant: every note extension must also be in
    /// [`TOTAL_BYTES_EXTENSIONS`] so a session's `.md` is part of the
    /// disk-usage total.
    #[test]
    fn note_extensions_are_subset_of_total_bytes_extensions() {
        for ext in NOTE_EXTENSIONS {
            assert!(
                TOTAL_BYTES_EXTENSIONS.contains(ext),
                "{ext} is in NOTE_EXTENSIONS but missing from TOTAL_BYTES_EXTENSIONS"
            );
        }
    }

    #[test]
    fn purge_case_insensitive_extension() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let upper = write_file(tmp.path(), "loud.WAV", &[0u8; 8]);
        backdate(&upper, 30);
        let purged = purge_audio_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 1);
        assert!(!upper.exists());
    }

    /// Tier 4 disjoint-list invariant: the audio and summary sweepers
    /// must never share an extension. A future change that put `md`
    /// in [`AUDIO_EXTENSIONS`] (or `wav`/`m4a` in
    /// [`SUMMARY_EXTENSIONS`]) would silently widen one sweeper's
    /// blast radius into the other's territory, deleting user data
    /// the corresponding setting promised to retain.
    #[test]
    fn summary_extensions_disjoint_from_audio_extensions() {
        for ext in SUMMARY_EXTENSIONS {
            assert!(
                !AUDIO_EXTENSIONS.contains(ext),
                "{ext} appears in both SUMMARY_EXTENSIONS and AUDIO_EXTENSIONS \
                 — the two retention sweepers must operate on disjoint sets"
            );
        }
    }

    #[test]
    fn purge_summaries_keeps_audio_deletes_old_md() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let md_old = write_file(tmp.path(), "old.md", b"old-summary");
        let md_new = write_file(tmp.path(), "new.md", b"fresh-summary");
        // The cross-test that pins the audio/summary boundary: even
        // when the audio sidecars are *also* old, the summary sweeper
        // must leave them untouched. A regression that conflates the
        // two predicates (e.g. shares a single allow-list) would purge
        // these too and the assertions below would fail loudly.
        let wav_old = write_file(tmp.path(), "old.wav", &[0u8; 8]);
        let m4a_old = write_file(tmp.path(), "old.m4a", &[0u8; 8]);

        backdate(&md_old, 10);
        backdate(&wav_old, 10);
        backdate(&m4a_old, 10);
        // md_new is fresh — leave its mtime alone.

        let purged = purge_summaries_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 1);
        assert!(!md_old.exists());
        assert!(md_new.exists(), "fresh summary must survive");
        assert!(
            wav_old.exists(),
            ".wav must not be purged by summary sweeper"
        );
        assert!(
            m4a_old.exists(),
            ".m4a must not be purged by summary sweeper"
        );
    }

    #[test]
    fn purge_summaries_skips_unrelated_extensions_even_when_old() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let bak = write_file(tmp.path(), "session.md.bak", b"old-rollback");
        let txt = write_file(tmp.path(), "stash.txt", b"old-note");
        let wav = write_file(tmp.path(), "old.wav", &[0u8; 8]);
        backdate(&bak, 100);
        backdate(&txt, 100);
        backdate(&wav, 100);

        let purged = purge_summaries_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 0);
        assert!(bak.exists(), ".bak must not be purged by summary sweeper");
        assert!(txt.exists(), ".txt must not be purged by summary sweeper");
        assert!(wav.exists(), ".wav must not be purged by summary sweeper");
    }

    #[test]
    fn purge_summaries_zero_days_deletes_all_md() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let md = write_file(tmp.path(), "fresh.md", b"x");
        // Pin the audio-boundary contract even at the `days = 0`
        // extreme: a fresh `.wav` co-located with the `.md` must
        // survive. The audio sweeper's matching `purge_zero_days_*`
        // test pins the symmetric direction.
        let wav = write_file(tmp.path(), "fresh.wav", &[0u8; 8]);
        let purged = purge_summaries_older_than(tmp.path(), 0).expect("purge");
        assert_eq!(purged, 1);
        assert!(!md.exists());
        assert!(wav.exists(), ".wav must survive a zero-day summary purge");
    }

    #[test]
    fn purge_summaries_returns_zero_when_nothing_to_purge() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_file(tmp.path(), "session.wav", &[0u8; 8]);
        let purged = purge_summaries_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 0);
    }

    #[test]
    fn purge_summaries_case_insensitive_extension() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let upper = write_file(tmp.path(), "LOUD.MD", &[0u8; 8]);
        backdate(&upper, 30);
        let purged = purge_summaries_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 1);
        assert!(!upper.exists());
    }

    #[cfg(unix)]
    #[test]
    fn purge_summaries_skips_top_level_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let real = tmp.path().join("real.md");
        fs::write(&real, b"real-summary").expect("real");
        backdate(&real, 30);

        // Mirror the audio sweeper's symlink-safety test: a malicious
        // vault that contains a `.md` symlink pointing at a file
        // outside the vault must not get that target deleted by the
        // summary sweeper. The walk inspects `file_type` (which does
        // not dereference) before doing anything destructive.
        let outside_dir = tempfile::TempDir::new().expect("outside");
        let outside_target = outside_dir.path().join("outside.md");
        fs::write(&outside_target, b"victim").expect("outside fixture");
        backdate(&outside_target, 100);

        let link = tmp.path().join("link.md");
        symlink(&outside_target, &link).expect("symlink");

        let purged = purge_summaries_older_than(tmp.path(), 7).expect("purge");
        assert_eq!(purged, 1);
        assert!(!real.exists());
        assert!(
            outside_target.exists(),
            "symlink target outside the vault must not be deleted"
        );
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "symlink entry should remain in the vault after purge"
        );
    }

    #[cfg(unix)]
    #[test]
    fn purge_skips_top_level_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let real = tmp.path().join("real.wav");
        fs::write(&real, [0u8; 8]).expect("real");
        backdate(&real, 30);

        // Create a `.wav` symlink *inside* the vault that points at a
        // file we don't own. The walk must skip the symlink without
        // dereferencing — otherwise a malicious vault could trick the
        // purge into deleting arbitrary files.
        let outside_dir = tempfile::TempDir::new().expect("outside");
        let outside_target = outside_dir.path().join("outside.wav");
        fs::write(&outside_target, [0u8; 4]).expect("outside fixture");
        backdate(&outside_target, 100);

        let link = tmp.path().join("link.wav");
        symlink(&outside_target, &link).expect("symlink");

        let purged = purge_audio_older_than(tmp.path(), 7).expect("purge");
        // `real.wav` is purged; `link.wav` is skipped (file_type
        // reports `is_symlink`, so the walk never dereferences it).
        // What we care about is the *target* outside the vault: the
        // purge must not have followed the link and deleted it.
        assert_eq!(purged, 1);
        assert!(!real.exists());
        assert!(
            outside_target.exists(),
            "symlink target outside the vault must not be deleted"
        );
        // The symlink itself should still be present (we skipped it,
        // not deleted it). `symlink_metadata` follows nothing; if the
        // entry is gone the call errors.
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "symlink entry should remain in the vault after purge"
        );
    }
}
