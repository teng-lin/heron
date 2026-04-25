//! Ringbuffer purge with verification per §12.3.
//!
//! After the m4a archival encode (§11.3), the session's `.raw` cache
//! is the only thing keeping the on-disk footprint above the
//! "ambient note-taker" target. Purging is therefore the last step of
//! `finalize_session`, but only if the m4a really is sound — a
//! truncated archival file would silently lose the meeting.
//!
//! The contract:
//!
//! 1. `verify_m4a(&m4a_path, expected_sec)` is called first.
//! 2. **Iff it returns `Ok(true)`**, `cache_dir` is removed.
//! 3. Otherwise the cache is retained and the caller surfaces a
//!    `PurgeOutcome::Salvage` so the review UI (§15) can present a
//!    "salvage from cache" banner instead of dropping the session.
//!
//! Verification *errors* (ffprobe missing, transient I/O, malformed
//! ffprobe output) also retain the cache. Better to keep the raw
//! frames around than to delete on a flaky tool failure.

use std::fs;
use std::path::Path;

use crate::encode::{self, EncodeError};

/// What happened when we tried to purge.
///
/// The variants match the three states the review UI cares about:
/// "all good, nothing to surface", "show the salvage banner so the
/// user can recover from cache", "show the salvage banner *and* an
/// error toast about the verification tool itself".
#[derive(Debug)]
pub enum PurgeOutcome {
    /// m4a verified, cache removed. No further action required.
    Purged,
    /// m4a did not verify; cache retained for salvage. The review
    /// UI should surface the salvage banner per §12.3.
    Salvaged,
    /// Verification could not run (ffprobe missing, I/O error). Cache
    /// retained; surface both the salvage banner and the underlying
    /// error so the user can fix the toolchain.
    SalvagedDueToError(EncodeError),
}

impl PurgeOutcome {
    /// True iff the cache directory has been removed.
    pub fn cache_purged(&self) -> bool {
        matches!(self, PurgeOutcome::Purged)
    }

    /// True iff the review UI should display the §12.3 salvage banner.
    pub fn needs_salvage_banner(&self) -> bool {
        !self.cache_purged()
    }
}

/// Verify `m4a_path` against `expected_sec`; remove `cache_dir` only
/// if the verification returns `Ok(true)`.
///
/// Never propagates [`EncodeError`] — verification failure is *not* a
/// reason to fail the surrounding `finalize_session`. The session has
/// already been written; the cache is just storage hygiene. The
/// outcome enum carries any underlying tool error so the caller can
/// log it without aborting.
///
/// `cache_dir` is allowed to not exist (a session re-finalized after
/// a previous run already purged): treated as `Purged`.
pub fn purge_after_verify(m4a_path: &Path, expected_sec: f64, cache_dir: &Path) -> PurgeOutcome {
    match encode::verify_m4a(m4a_path, expected_sec) {
        Ok(true) => match remove_cache(cache_dir) {
            Ok(()) => PurgeOutcome::Purged,
            Err(_) => PurgeOutcome::Salvaged,
        },
        Ok(false) => PurgeOutcome::Salvaged,
        Err(e) => PurgeOutcome::SalvagedDueToError(e),
    }
}

fn remove_cache(cache_dir: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(cache_dir) {
        Ok(()) => Ok(()),
        // Already gone (idempotent re-finalize) is success — the
        // post-condition "cache is not on disk" is already met.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn purge_outcome_classification() {
        assert!(PurgeOutcome::Purged.cache_purged());
        assert!(!PurgeOutcome::Purged.needs_salvage_banner());

        assert!(!PurgeOutcome::Salvaged.cache_purged());
        assert!(PurgeOutcome::Salvaged.needs_salvage_banner());

        let err = EncodeError::FfprobeMissing;
        let outcome = PurgeOutcome::SalvagedDueToError(err);
        assert!(!outcome.cache_purged());
        assert!(outcome.needs_salvage_banner());
    }

    #[test]
    fn purge_with_missing_m4a_keeps_cache() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).expect("mkdir");
        fs::write(cache.join("mic.raw"), b"\x00\x00\x00\x00").expect("seed");

        let m4a = tmp.path().join("does-not-exist.m4a");
        let outcome = purge_after_verify(&m4a, 60.0, &cache);

        assert!(!outcome.cache_purged(), "missing m4a must retain cache");
        assert!(outcome.needs_salvage_banner());
        assert!(cache.exists(), "cache must still be on disk for salvage");
    }

    #[test]
    fn purge_is_idempotent_when_cache_already_gone() {
        // Simulate verify_m4a returning true via a real m4a is overkill
        // for this property — exercise the inner remove_cache helper.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("never-existed");

        remove_cache(&cache).expect("missing cache must be ok");
        assert!(!cache.exists());
    }

    /// End-to-end happy path: synthesize a 1s m4a (ffmpeg required),
    /// run purge_after_verify, assert cache is gone. Ignored unless
    /// ffmpeg/ffprobe are on PATH.
    #[test]
    #[ignore = "requires ffmpeg + ffprobe; run with --ignored"]
    fn end_to_end_happy_path_purges_cache() {
        use std::process::Command;

        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).expect("mkdir");
        fs::write(cache.join("mic.raw"), b"\x00\x00\x00\x00").expect("seed mic");
        fs::write(cache.join("tap.raw"), b"\x00\x00\x00\x00").expect("seed tap");

        let m4a = tmp.path().join("session.m4a");
        let s = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=stereo:sample_rate=48000",
                "-t",
                "1.0",
                "-c:a",
                "aac",
                &m4a.display().to_string(),
            ])
            .status()
            .expect("synth m4a");
        assert!(s.success());

        let outcome = purge_after_verify(&m4a, 1.0, &cache);
        assert!(
            outcome.cache_purged(),
            "valid m4a must trigger purge: {outcome:?}"
        );
        assert!(!cache.exists(), "cache dir must be gone after purge");
    }
}
