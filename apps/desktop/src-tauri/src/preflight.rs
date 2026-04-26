//! Pre-flight checks per PR-λ phase 73.
//!
//! v1's recording-start path needs a "do we have enough disk?" gate
//! before it spins up the capture pipeline — running out of space
//! mid-call corrupts the salvage cache (PR-η) and silently truncates
//! the m4a (§12.3). The user already declares the threshold via
//! [`crate::settings::Settings::min_free_disk_mib`] (PR-ζ); this
//! module turns that threshold into a Yes/No answer for the React
//! layer.
//!
//! ## Resolution shape
//!
//! [`heron_check_disk_for_recording`] reads the user's settings, asks
//! the OS for free bytes on the disk hosting the cache root (where
//! salvage `.raw` files land per PR-η), and returns a discriminated
//! union: [`DiskCheckOutcome::Ok`] when free ≥ threshold, or
//! [`DiskCheckOutcome::BelowThreshold`] when the user is out of room.
//!
//! The wire shape matches `AssetSource` in `asset_protocol.rs`
//! (`#[serde(tag = "kind", rename_all = "snake_case")]`) so the
//! frontend's discriminated-union pattern stays identical across
//! commands.
//!
//! ## What we do NOT do here
//!
//! - We do not block recording-start in the Rust layer. The frontend
//!   chooses what to do with the outcome (warning modal, "Continue
//!   anyway" override, app-mount banner). Keeping this command pure
//!   makes it cheap to call from multiple sites without coordinating
//!   side effects.
//! - We do not query per-file usage; that's `heron_disk_usage` (PR-ζ)
//!   and answers a different question ("how much have *we* written?").
//!   This module asks "how much is left on the volume?".
//! - We do not poll. The check fires on demand: once on app mount, and
//!   once before each consent-gate confirmation. A sustained-watch
//!   loop would burn battery, and the FSM's `StorageCritical` event
//!   already covers the mid-recording case (§14.1).

use std::path::Path;

use serde::Serialize;

use crate::default_cache_root;
use crate::settings::{Settings, read_settings};

/// Result of the pre-flight disk check.
///
/// Tagged union with `tag = "kind"` (lowercase variants) so the React
/// layer can pattern-match on `outcome.kind === "ok"` /
/// `"below_threshold"` without a custom decoder. Mirrors the
/// `AssetSource` convention in `asset_protocol.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiskCheckOutcome {
    /// Enough free space on the cache volume to safely start a session.
    /// `free_mib` is the raw probe result for the UI to render
    /// ("X MiB free") even on the happy path.
    Ok { free_mib: u64 },
    /// Free space dipped below `threshold_mib`. The frontend shows a
    /// confirmation modal at recording-start and a Sonner toast at
    /// app-mount.
    BelowThreshold { free_mib: u64, threshold_mib: u64 },
}

/// Decide whether `free_mib` clears the user's threshold in
/// `settings`. Pure function so the unit tests below pin the comparator
/// (off-by-one would break the recording-start gate) without
/// depending on the runner's actual free space — `check_disk` is the
/// thin wrapper that wires the real `statvfs(2)` probe in.
///
/// Returns [`DiskCheckOutcome::BelowThreshold`] when `free_mib <
/// threshold_mib`. Returns [`DiskCheckOutcome::Ok`] otherwise,
/// including the boundary `free_mib == threshold_mib` case (the user
/// said "stop *below* this" — at-the-line is fine).
fn evaluate(free_mib: u64, settings: &Settings) -> DiskCheckOutcome {
    let threshold_mib = u64::from(settings.min_free_disk_mib);
    if free_mib < threshold_mib {
        DiskCheckOutcome::BelowThreshold {
            free_mib,
            threshold_mib,
        }
    } else {
        DiskCheckOutcome::Ok { free_mib }
    }
}

/// Public entry: combine a real `statvfs(2)` probe over `cache_root`
/// with the user's threshold and return the decision.
pub fn check_disk(cache_root: &Path, settings: &Settings) -> DiskCheckOutcome {
    let free_mib = free_mib_for_path(cache_root);
    evaluate(free_mib, settings)
}

/// Free-MiB query for the volume hosting `path`.
///
/// Walks up the path until `statvfs(2)` accepts the argument — a
/// cache_root whose prefix doesn't yet exist (first launch on a fresh
/// install) walks up to the parent until we hit something the kernel
/// can stat. Returns `0` on every error so the caller "fails closed":
/// a missing cache directory surfaces as `BelowThreshold`, prompting
/// the user to investigate rather than silently letting recording
/// proceed against unknown storage.
///
/// `cfg(unix)`-gated. Off-Unix targets (Tauri 2 still compiles for
/// Windows; v1 ships macOS-only but workspace `cargo check` runs on
/// the broader matrix) fall back to "0 MiB free", which the
/// `BelowThreshold` branch handles uniformly.
#[cfg(unix)]
fn free_mib_for_path(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Walk up until `statvfs` accepts the arg. The cache root may not
    // exist on a first launch ("~/Library/Caches/com.heronnote.heron"
    // gets created on demand by PR-η); the closest existing ancestor
    // (the user's home dir, then `/`) is on the same volume in
    // practice on macOS, so the answer is correct enough to gate
    // recording-start on.
    let mut probe: Option<&Path> = Some(path);
    while let Some(candidate) = probe {
        let Ok(c_path) = CString::new(candidate.as_os_str().as_bytes()) else {
            // A path containing a NUL byte can't reach `statvfs`. This
            // is exotic enough to fail closed — the user's install is
            // doing something unusual that we'd rather not silently
            // accept.
            return 0;
        };
        // SAFETY: `statvfs` reads from `c_path` (a NUL-terminated path)
        // and writes to `stat`. Both lifetimes are bounded by this
        // block; no raw pointers escape.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if ret == 0 {
            // `f_bavail` is "blocks available to non-superuser"; on
            // every Unix we target that's the right answer for a
            // userland app (root would use `f_bfree`). `f_frsize` is
            // the fundamental block size; multiplying gives free
            // bytes.
            //
            // The platform `fsblkcnt_t` widens differently across
            // targets — `u32` on macOS, `u64` on most Linux. The
            // `as u64` cast normalises to a width where the multiply
            // can't wrap (a 4 PiB volume would still fit). Clippy is
            // aware that the cast is a no-op on Linux and would flag
            // it; the `#[allow]` documents intent for the macOS path
            // where the widening is load-bearing.
            #[allow(clippy::unnecessary_cast)]
            let bavail = stat.f_bavail as u64;
            #[allow(clippy::unnecessary_cast)]
            let frsize = stat.f_frsize as u64;
            let free_bytes = bavail.saturating_mul(frsize);
            return free_bytes / (1024 * 1024);
        }
        // `statvfs` failed (typically ENOENT). Try the parent path; if
        // we've already walked to the filesystem root, give up.
        probe = candidate.parent().filter(|p| *p != candidate);
    }
    0
}

/// Off-Unix stub. v1 ships macOS-only and CI runs on macOS+Linux, so
/// this branch only fires for cross-compile probes on Windows. The "0
/// MiB free" answer routes the caller through `BelowThreshold`, which
/// matches the rest of the "fail closed" policy.
#[cfg(not(unix))]
fn free_mib_for_path(_path: &Path) -> u64 {
    0
}

/// Tauri command: run the pre-flight disk check against the user's
/// current settings.
///
/// Errors map to `String` so they reach the frontend without the
/// frontend needing the `SettingsError` type — same convention as
/// `heron_resolve_recording` and `heron_diagnostics`.
#[tauri::command]
pub fn heron_check_disk_for_recording(settings_path: String) -> Result<DiskCheckOutcome, String> {
    let settings = read_settings(Path::new(&settings_path)).map_err(|e| e.to_string())?;
    let cache_root = default_cache_root();
    Ok(check_disk(&cache_root, &settings))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_returns_ok_when_free_above_threshold() {
        let settings = Settings {
            min_free_disk_mib: 100,
            ..Default::default()
        };
        let outcome = evaluate(500, &settings);
        assert_eq!(outcome, DiskCheckOutcome::Ok { free_mib: 500 });
    }

    #[test]
    fn evaluate_returns_below_threshold_when_free_under_threshold() {
        let settings = Settings {
            min_free_disk_mib: 1024,
            ..Default::default()
        };
        let outcome = evaluate(500, &settings);
        assert_eq!(
            outcome,
            DiskCheckOutcome::BelowThreshold {
                free_mib: 500,
                threshold_mib: 1024,
            },
        );
    }

    #[test]
    fn evaluate_boundary_free_equals_threshold_is_ok() {
        // Contract: "stop *below* the threshold" — at-the-line is the
        // happy path. Pin this so a future off-by-one rewrite (e.g.
        // `<=` instead of `<`) trips the test.
        let settings = Settings {
            min_free_disk_mib: 2048,
            ..Default::default()
        };
        let outcome = evaluate(2048, &settings);
        assert_eq!(outcome, DiskCheckOutcome::Ok { free_mib: 2048 });
    }

    #[test]
    fn evaluate_zero_threshold_is_always_ok() {
        // A threshold of zero is the "I don't care" setting. Even with
        // zero free MiB, the comparison is `0 < 0 = false`, so the
        // happy path fires. Validates the chosen comparator without
        // depending on volume state.
        let settings = Settings {
            min_free_disk_mib: 0,
            ..Default::default()
        };
        for free in [0_u64, 1, 100, u64::MAX] {
            let outcome = evaluate(free, &settings);
            assert!(
                matches!(outcome, DiskCheckOutcome::Ok { .. }),
                "free={free} with zero threshold should be Ok, got {outcome:?}",
            );
        }
    }

    #[test]
    fn check_disk_below_threshold_when_threshold_exceeds_capacity() {
        // A threshold orders of magnitude larger than any plausible
        // CI disk (~4 PiB) forces the BelowThreshold branch
        // deterministically without a fixture filesystem. Using
        // `u32::MAX` MiB keeps the value within the `u32` settings
        // field bound.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let settings = Settings {
            min_free_disk_mib: u32::MAX,
            ..Default::default()
        };
        let outcome = check_disk(tmp.path(), &settings);
        match outcome {
            DiskCheckOutcome::BelowThreshold {
                free_mib,
                threshold_mib,
            } => {
                assert_eq!(threshold_mib, u64::from(u32::MAX));
                assert!(
                    free_mib < threshold_mib,
                    "free_mib={free_mib} threshold={threshold_mib}",
                );
            }
            DiskCheckOutcome::Ok { .. } => {
                panic!("expected BelowThreshold with u32::MAX threshold, got {outcome:?}")
            }
        }
    }

    #[test]
    fn check_disk_ok_when_threshold_is_one_mib() {
        // The dual of the previous test: a 1 MiB threshold is below
        // any reasonable CI runner's free space, so the call must
        // return `Ok` deterministically. Together these two pin both
        // branches of the production path against a real filesystem.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let settings = Settings {
            min_free_disk_mib: 1,
            ..Default::default()
        };
        let outcome = check_disk(tmp.path(), &settings);
        assert!(
            matches!(outcome, DiskCheckOutcome::Ok { .. }),
            "expected Ok with 1 MiB threshold on a real tmp dir, got {outcome:?}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn free_mib_walks_up_to_existing_ancestor() {
        // A first-launch user has no `~/Library/Caches/com.heronnote.heron`
        // directory yet. `statvfs` on the missing path errors; the
        // walk-up logic must reach the parent (which does exist) and
        // return that volume's free space. Pin the contract: the
        // returned MiB must be > 0 because the temp dir's parent
        // (the system temp filesystem) is real.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let nonexistent = tmp.path().join("does-not-exist-yet").join("nested");
        let free = free_mib_for_path(&nonexistent);
        assert!(
            free > 0,
            "walk-up should land on the temp volume; got 0 MiB free",
        );
    }

    #[test]
    fn missing_settings_file_uses_defaults() {
        // The Tauri command reads the user's settings.json by path.
        // A first-launch user has no file on disk yet; `read_settings`
        // returns `Settings::default()`. The command path must thread
        // that through cleanly rather than erroring out — first-run
        // users would otherwise see a broken pre-flight banner the
        // moment they open the app.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let missing = tmp.path().join("never-written.json");
        let outcome =
            heron_check_disk_for_recording(missing.to_str().expect("tmp path is utf-8").to_owned())
                .expect("command must not error on a missing settings file");
        // The default threshold is 2048 MiB; the result is environment-
        // dependent (CI runner free space). Asserting the discriminant
        // pattern is enough — we're proving the command didn't panic.
        match outcome {
            DiskCheckOutcome::Ok { .. } | DiskCheckOutcome::BelowThreshold { .. } => {}
        }
    }

    #[test]
    fn outcome_serializes_with_kind_tag() {
        // The frontend's discriminated-union decoder switches on
        // `outcome.kind`. Pin both variants' wire shapes here so a
        // future serde rename (e.g. dropping the `tag = "kind"`
        // attribute, or flipping `rename_all`) fails loudly instead of
        // silently breaking the UI.
        let ok = DiskCheckOutcome::Ok { free_mib: 4096 };
        let s = serde_json::to_string(&ok).expect("serialize Ok");
        assert_eq!(s, r#"{"kind":"ok","free_mib":4096}"#);

        let bt = DiskCheckOutcome::BelowThreshold {
            free_mib: 100,
            threshold_mib: 2048,
        };
        let s = serde_json::to_string(&bt).expect("serialize BelowThreshold");
        assert_eq!(
            s,
            r#"{"kind":"below_threshold","free_mib":100,"threshold_mib":2048}"#,
        );
    }
}
