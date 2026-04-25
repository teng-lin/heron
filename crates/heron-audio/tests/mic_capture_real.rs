//! Integration test for the real cpal mic capture pipeline.
//!
//! This test is `#[ignore]`d by default — it requires:
//! - macOS 14.2+ (matches v0's supported floor)
//! - A working default input device (built-in mic, USB headset, etc.)
//! - TCC "Microphone" granted to the test runner's parent process
//!   (Terminal / iTerm / VS Code)
//! - The env var `HERON_MIC_CAPTURE_REAL=1` set
//!
//! Without the env var the test prints a skip message and returns
//! early, so `cargo test -p heron-audio -- --ignored` still reports
//! a clean result on a CI runner.
//!
//! On non-Apple platforms the file compiles to an empty test
//! binary — there is no cpal mic capture off-Apple in v0.
//!
//! Runbook: see `docs/manual-test-matrix.md` — search for the
//! "mic capture real" row.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]

use std::time::Duration;

use heron_audio::AudioError;
use heron_types::{Channel, SessionId};

#[tokio::test]
#[ignore = "needs HERON_MIC_CAPTURE_REAL=1 + TCC microphone + a working input device; see docs/manual-test-matrix.md"]
async fn mic_capture_emits_at_least_one_mic_frame() {
    if std::env::var_os("HERON_MIC_CAPTURE_REAL").is_none() {
        eprintln!(
            "mic_capture_emits_at_least_one_mic_frame: SKIPPED — \
             set HERON_MIC_CAPTURE_REAL=1 to run. Requires macOS 14.2+, \
             TCC microphone granted to the test runner, and a default \
             input device that supports 48 kHz f32 input."
        );
        return;
    }

    // The mic pipeline lives inside `AudioCapture::start`, which also
    // tries to open a process tap. Use a deliberately-not-running
    // bundle id so the tap path returns `ProcessNotFound` cleanly —
    // BUT note: under the "mic-failure-doesn't-fail-the-session"
    // policy, the orchestrator's `start()` is the one that swallows
    // mic failures, while a tap failure DOES propagate. So this test
    // can't reuse `AudioCapture::start` against a no-such-app and
    // exercise the mic path. Instead we call `mic_capture::start_mic`
    // directly to keep the test focused.
    use heron_audio::mic_capture;
    use heron_types::{Event, SessionClock};
    use tokio::sync::broadcast;

    let (frames_tx, mut frames_rx) = broadcast::channel::<heron_audio::CaptureFrame>(256);
    let (events_tx, _events_rx) = broadcast::channel::<Event>(64);
    let clock = SessionClock::new();

    let _handle = match mic_capture::start_mic(
        frames_tx.clone(),
        events_tx.clone(),
        SessionId::nil(),
        clock,
    ) {
        Ok(h) => h,
        Err(AudioError::PermissionDenied(msg)) => {
            panic!(
                "TCC denied: {msg} — grant System Settings → Privacy & Security → \
                 Microphone to the test runner and re-run"
            );
        }
        Err(AudioError::NotYetImplemented) => {
            panic!("start_mic returned NotYetImplemented on macOS — cfg gate regressed");
        }
        Err(other) => panic!("start_mic failed: {other}"),
    };

    // Drop our owning sender end so the broadcast channel relies on
    // the consumer task's clone for the receiver to stay live (the
    // mic handle clones from inside `start_mic`). Confirms the handle
    // owns enough of the producer side to keep recv() alive.
    drop(frames_tx);

    // Up to 5 s for the first mic frame. Internal mics on Apple
    // Silicon typically deliver buffers every ~10 ms once the audio
    // unit is running; 5 s is ~500 frames of headroom even on a
    // heavily loaded laptop.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got_mic_frame = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), frames_rx.recv()).await {
            Ok(Ok(frame)) => {
                if frame.channel == Channel::Mic && !frame.samples.is_empty() {
                    got_mic_frame = true;
                    break;
                }
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                panic!("frames broadcast closed unexpectedly")
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // Receiver fell behind — that's fine, just keep
                // polling. The mic pipeline is alive and producing.
                continue;
            }
            Err(_elapsed) => {
                // No frame this round; keep polling.
            }
        }
    }

    assert!(
        got_mic_frame,
        "no Channel::Mic CaptureFrame arrived within 5s. \
         Common causes: (1) TCC microphone not granted to the test runner — \
         check System Settings → Privacy & Security → Microphone; \
         (2) default input device doesn't support 48 kHz f32 — try a \
         different input in System Settings → Sound → Input; \
         (3) mic muted or input volume at zero."
    );
}
