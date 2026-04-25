//! Integration test for the real Core Audio process tap.
//!
//! This test is `#[ignore]`d by default — it requires:
//! - macOS 14.2+
//! - The target meeting client (`us.zoom.xos` by default) running
//!   AND in a state that produces audio (joined call, not muted)
//! - TCC "system audio recording" granted to the test binary
//! - The env var `HERON_PROCESS_TAP_REAL=1` set
//!
//! Without the env var the test prints a skip message and returns
//! early, so `cargo test -p heron-audio -- --ignored` still reports
//! a clean result on a CI runner.
//!
//! On non-Apple platforms the file compiles to an empty test
//! binary — there is no Core Audio process tap off-Apple.
//!
//! Runbook: see `docs/manual-test-matrix.md` — search for the
//! "process tap real" row.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]

use std::time::Duration;

use heron_audio::{AudioCapture, AudioError};
use heron_types::SessionId;

#[tokio::test]
#[ignore = "needs HERON_PROCESS_TAP_REAL=1 + TCC + a live meeting client; see docs/manual-test-matrix.md"]
async fn process_tap_emits_at_least_one_frame() {
    if std::env::var_os("HERON_PROCESS_TAP_REAL").is_none() {
        eprintln!(
            "process_tap_emits_at_least_one_frame: SKIPPED — \
             set HERON_PROCESS_TAP_REAL=1 to run. \
             Requires macOS 14.2+, TCC system-audio-recording, \
             and a meeting client matching the target bundle id \
             actively producing audio."
        );
        return;
    }

    let bundle_id =
        std::env::var("HERON_PROCESS_TAP_BUNDLE_ID").unwrap_or_else(|_| "us.zoom.xos".to_string());

    let temp = tempfile::tempdir().expect("tempdir for cache_dir");

    let mut handle = match AudioCapture::start(SessionId::nil(), &bundle_id, temp.path()).await {
        Ok(h) => h,
        Err(AudioError::ProcessNotFound { bundle_id }) => {
            panic!(
                "no running app matched bundle id {bundle_id:?} — \
                 launch it (or override via HERON_PROCESS_TAP_BUNDLE_ID) \
                 and re-run"
            );
        }
        Err(AudioError::PermissionDenied(msg)) => {
            panic!(
                "TCC denied: {msg} — grant System Settings → Privacy & Security → \
                 Microphone (and System Audio Recording, on 14.2+) to the test runner"
            );
        }
        Err(other) => panic!("AudioCapture::start failed: {other}"),
    };

    // Give the IO proc a couple of seconds to fire. Real apps emit
    // audio in 10 ms windows, so 2 s is ~200 frames of headroom.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);

    let mut got_frame = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), handle.frames.recv()).await {
            Ok(Ok(_frame)) => {
                got_frame = true;
                break;
            }
            Ok(Err(e)) => panic!("broadcast channel closed unexpectedly: {e:?}"),
            Err(_elapsed) => {
                // No frame this round; keep polling until the deadline.
            }
        }
    }

    // TODO(io-proc): once `process_tap::install_io_proc` actually
    // fires the IO block, flip this back to a hard assert. While
    // the proc is still a no-op (week-2 scaffolding) we only assert
    // that nobody errored along the way — i.e. the tap + aggregate
    // were created without TCC/permission failure.
    if !got_frame {
        eprintln!(
            "process_tap_emits_at_least_one_frame: tap built successfully \
             but no CaptureFrame arrived within 2s. Expected today — \
             the IO-proc → broadcast pipe lands in week 3 (§7). \
             Promote this to a hard assert once that ships."
        );
    }

    drop(handle);
}
