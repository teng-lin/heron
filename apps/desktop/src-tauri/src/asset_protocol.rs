//! `heron://recording/<id>` asset-protocol resolver per §15.2.
//!
//! The review UI (§15) requests audio playback via Tauri's custom URI
//! scheme. The actual scheme handler is registered on the Tauri
//! `Builder`; this module exists so the resolution rules are pure Rust
//! and unit-testable without spinning up a Tauri runtime.
//!
//! ## Resolution rules
//!
//! 1. If `<vault>/meetings/<date>-<slug>.m4a` exists → serve it.
//! 2. Else if `<cache>/sessions/<id>/{mic,tap}.raw` exist → mix them
//!    into a WAV in the cache and serve the WAV. *This is the salvage
//!    fallback — it is what makes "play before m4a finished" work in
//!    week 13 demos.*
//! 3. Else → not found.
//!
//! v1's actual mixdown step is wired in week 13. Today this module
//! resolves the path the protocol handler should serve and reports
//! which fallback path it took, so the diagnostics tab can surface it.

use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

const MIC_FILENAME: &str = "mic.raw";
const TAP_FILENAME: &str = "tap.raw";

/// What the protocol handler should serve for a given session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssetSource {
    /// Serve the archival m4a directly. Happens once §11.3 has run
    /// and §12.3 has not yet purged the cache (or has, doesn't matter
    /// — the m4a is canonical).
    M4a { path: PathBuf },
    /// m4a not on disk yet; serve a mixdown WAV from the live cache.
    /// `mic_raw` and `tap_raw` are the inputs the mixer reads.
    SalvageFromCache { mic_raw: PathBuf, tap_raw: PathBuf },
}

#[derive(Debug, Error)]
pub enum AssetError {
    #[error("no archival m4a and no cache for session {session_id}")]
    NotFound { session_id: String },
    #[error(
        "session {session_id} cache is partial: {} present but {} missing",
        present.display(),
        missing
    )]
    PartialCache {
        session_id: String,
        present: PathBuf,
        missing: String,
    },
}

/// Resolve a `heron://recording/<id>` URI into a concrete asset.
///
/// Inputs:
/// - `session_id` — the path component the URI carried.
/// - `m4a_candidate` — where the writer would have placed the m4a if
///   the encode passed (e.g. `<vault>/meetings/<date>-<slug>.m4a`).
/// - `cache_root` — the `~/Library/Caches/heron` directory.
///
/// Returns the [`AssetSource`] the handler should serve. Errors when
/// neither a verified m4a nor a salvageable cache exists.
pub fn resolve_recording_uri(
    session_id: &str,
    m4a_candidate: &Path,
    cache_root: &Path,
) -> Result<AssetSource, AssetError> {
    if file_exists_and_nonempty(m4a_candidate) {
        return Ok(AssetSource::M4a {
            path: m4a_candidate.to_path_buf(),
        });
    }
    let session_cache = cache_root.join("sessions").join(session_id);
    let mic = session_cache.join(MIC_FILENAME);
    let tap = session_cache.join(TAP_FILENAME);
    let mic_present = file_exists_and_nonempty(&mic);
    let tap_present = file_exists_and_nonempty(&tap);

    match (mic_present, tap_present) {
        (true, true) => Ok(AssetSource::SalvageFromCache {
            mic_raw: mic,
            tap_raw: tap,
        }),
        (true, false) => Err(AssetError::PartialCache {
            session_id: session_id.to_owned(),
            present: mic,
            missing: TAP_FILENAME.to_owned(),
        }),
        (false, true) => Err(AssetError::PartialCache {
            session_id: session_id.to_owned(),
            present: tap,
            missing: MIC_FILENAME.to_owned(),
        }),
        (false, false) => Err(AssetError::NotFound {
            session_id: session_id.to_owned(),
        }),
    }
}

fn file_exists_and_nonempty(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    fn seed_cache(cache_root: &Path, session_id: &str, mic_bytes: usize, tap_bytes: usize) {
        let dir = cache_root.join("sessions").join(session_id);
        fs::create_dir_all(&dir).expect("mkdir");
        if mic_bytes > 0 {
            fs::write(dir.join(MIC_FILENAME), vec![0u8; mic_bytes]).expect("seed mic");
        }
        if tap_bytes > 0 {
            fs::write(dir.join(TAP_FILENAME), vec![0u8; tap_bytes]).expect("seed tap");
        }
    }

    #[test]
    fn m4a_wins_when_present_even_if_cache_is_also_present() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let m4a = tmp.path().join("meeting.m4a");
        fs::write(&m4a, b"\x00\x00ftypM4A ").expect("seed m4a");
        seed_cache(tmp.path(), "abc", 100, 100);

        let result = resolve_recording_uri("abc", &m4a, tmp.path()).expect("resolve");
        assert_eq!(result, AssetSource::M4a { path: m4a });
    }

    #[test]
    fn cache_is_used_when_m4a_missing() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let m4a = tmp.path().join("never.m4a");
        seed_cache(tmp.path(), "abc", 100, 100);

        let result = resolve_recording_uri("abc", &m4a, tmp.path()).expect("resolve");
        match result {
            AssetSource::SalvageFromCache { mic_raw, tap_raw } => {
                assert!(mic_raw.ends_with("sessions/abc/mic.raw"));
                assert!(tap_raw.ends_with("sessions/abc/tap.raw"));
            }
            other => panic!("expected SalvageFromCache, got {other:?}"),
        }
    }

    #[test]
    fn empty_m4a_is_treated_as_missing() {
        // The §12.3 verify_m4a returns false on zero-byte files, but
        // the asset-protocol resolver runs *before* the writer, so the
        // possibility of a zero-byte placeholder file from a crashed
        // ffmpeg run still needs to fall through to the cache.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let m4a = tmp.path().join("empty.m4a");
        fs::write(&m4a, b"").expect("seed empty");
        seed_cache(tmp.path(), "abc", 100, 100);

        let result = resolve_recording_uri("abc", &m4a, tmp.path()).expect("resolve");
        assert!(matches!(result, AssetSource::SalvageFromCache { .. }));
    }

    #[test]
    fn missing_everything_errors_with_session_id() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let err =
            resolve_recording_uri("missing-session", &tmp.path().join("nope.m4a"), tmp.path())
                .expect_err("missing must error");
        match err {
            AssetError::NotFound { session_id } => assert_eq!(session_id, "missing-session"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn partial_cache_reports_which_file_is_missing() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let m4a = tmp.path().join("nope.m4a");
        seed_cache(tmp.path(), "abc", 100, 0);

        let err = resolve_recording_uri("abc", &m4a, tmp.path()).expect_err("partial");
        match err {
            AssetError::PartialCache { missing, .. } => assert_eq!(missing, TAP_FILENAME),
            other => panic!("expected PartialCache, got {other:?}"),
        }
    }

    #[test]
    fn partial_cache_carries_owned_missing_string() {
        // Regression: gemini PR-24 finding — `missing` was previously
        // &'static str, which couldn't carry dynamic filenames if the
        // ringbuffer schema ever grew a third channel. The owned String
        // also matches the convention of every other field in
        // AssetError.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let m4a = tmp.path().join("nope.m4a");
        seed_cache(tmp.path(), "abc", 0, 100);

        let err = resolve_recording_uri("abc", &m4a, tmp.path()).expect_err("partial");
        if let AssetError::PartialCache { missing, .. } = err {
            assert_eq!(missing, "mic.raw");
        } else {
            panic!("expected PartialCache");
        }
    }
}
