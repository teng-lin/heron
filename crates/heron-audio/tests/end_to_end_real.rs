//! End-to-end integration test for the full capture → AEC → WAV
//! finalization pipeline.
//!
//! This test is `#[ignore]`d by default — it requires:
//! - macOS 14.2+
//! - The target meeting client (`us.zoom.xos` by default) running
//!   AND in a state that produces audio (joined call, not muted)
//! - TCC "system audio recording" granted to the test binary
//! - TCC "microphone" granted to the test binary (mic best-effort —
//!   a tap-only run still passes the assertions for tap.wav and the
//!   empty-WAV contract on mic.wav / mic_clean.wav)
//! - The env var `HERON_PROCESS_TAP_REAL=1` set
//!
//! Without the env var the test prints a skip message and returns
//! early, so `cargo test -p heron-audio -- --ignored` still reports
//! a clean result on a CI runner.
//!
//! On non-Apple platforms the file compiles to an empty test binary —
//! there is no Core Audio process tap off-Apple in v0.
//!
//! Runbook: see `docs/archives/manual-test-matrix.md` — the §6.3 AEC test rig
//! row is now actually runnable end-to-end via this harness.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use heron_audio::{AudioCapture, AudioError};
use heron_types::SessionId;
use hound::WavReader;

#[tokio::test]
#[ignore = "needs HERON_PROCESS_TAP_REAL=1 + TCC + a live meeting client; see docs/archives/manual-test-matrix.md"]
async fn end_to_end_session_writes_three_wavs() {
    if std::env::var_os("HERON_PROCESS_TAP_REAL").is_none() {
        eprintln!(
            "end_to_end_session_writes_three_wavs: SKIPPED — \
             set HERON_PROCESS_TAP_REAL=1 to run. \
             Requires macOS 14.2+, TCC system-audio-recording + \
             (best-effort) microphone, and a meeting client matching \
             the target bundle id actively producing audio. The §6.3 \
             AEC test rig depends on this harness."
        );
        return;
    }

    let bundle_id =
        std::env::var("HERON_PROCESS_TAP_BUNDLE_ID").unwrap_or_else(|_| "us.zoom.xos".to_string());

    let temp = tempfile::tempdir().expect("tempdir for cache_dir");

    let handle = match AudioCapture::start(SessionId::nil(), &bundle_id, temp.path()).await {
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
                "TCC denied: {msg} — grant System Settings → \
                 Privacy & Security → System Audio Recording (and \
                 Microphone, best-effort) to the test runner"
            );
        }
        Err(other) => panic!("AudioCapture::start failed: {other}"),
    };

    // Drive the session for ~2 s. APM's adaptive filter typically
    // converges within ~500 ms, and 2 s gives ~200 frames of
    // headroom for the WAV writers to land bytes on disk before
    // we stop.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let artifacts = handle.stop().await.expect("stop returns artifacts");

    // All three paths must exist on disk per the StopArtifacts
    // empty-WAV contract.
    assert!(
        artifacts.mic.exists(),
        "mic.wav must exist at {}",
        artifacts.mic.display()
    );
    assert!(
        artifacts.tap.exists(),
        "tap.wav must exist at {}",
        artifacts.tap.display()
    );
    assert!(
        artifacts.mic_clean.exists(),
        "mic_clean.wav must exist at {}",
        artifacts.mic_clean.display()
    );

    // tap.wav must have non-zero frames (the test gate is "live
    // meeting client producing audio", so the tap pipeline should
    // have emitted at least one frame in 2 s).
    let tap_reader = WavReader::open(&artifacts.tap).expect("open tap.wav");
    assert!(
        tap_reader.duration() > 0,
        "tap.wav should have frames — was the meeting client silent? \
         duration: {} samples",
        tap_reader.duration()
    );

    // mic_clean.wav: ideally non-zero (mic capture worked, AEC ran).
    // If mic capture failed (TCC denied / no input device), the
    // empty-WAV contract holds — both mic.wav AND mic_clean.wav
    // are 0 samples, and that's a valid tap-only session.
    let mic_reader = WavReader::open(&artifacts.mic).expect("open mic.wav");
    let mic_clean_reader = WavReader::open(&artifacts.mic_clean).expect("open mic_clean.wav");

    if mic_reader.duration() > 0 {
        assert!(
            mic_clean_reader.duration() > 0,
            "mic.wav has frames but mic_clean.wav is empty — \
             AEC task may have failed silently. mic={} samples, \
             mic_clean={} samples",
            mic_reader.duration(),
            mic_clean_reader.duration()
        );
    } else {
        assert_eq!(
            mic_clean_reader.duration(),
            0,
            "mic.wav empty (tap-only session); mic_clean.wav must \
             also be empty per empty-WAV contract, got {} samples",
            mic_clean_reader.duration()
        );
        eprintln!(
            "end_to_end_session_writes_three_wavs: tap-only run — \
             mic.wav and mic_clean.wav are 0-sample WAVs (empty-WAV \
             contract). To exercise the AEC path, grant TCC microphone \
             and re-run."
        );
    }

    // duration should be at least the 2 s we slept (with some
    // tolerance for stop-time scheduling).
    assert!(
        artifacts.duration >= Duration::from_millis(1_500),
        "session duration suspiciously short: {:?}",
        artifacts.duration
    );
}
