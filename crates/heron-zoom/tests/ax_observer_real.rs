#![allow(clippy::expect_used)]

//! Live AXObserver smoke test for the Zoom helper.
//!
//! Skipped on every host where `HERON_ZOOM_RUNNING` is unset — i.e.
//! every CI host and most dev boxes. To run:
//!
//! ```text
//! HERON_ZOOM_RUNNING=1 cargo test -p heron-zoom --test ax_observer_real \
//!     -- --ignored --nocapture
//! ```
//!
//! Preconditions for a real run:
//! 1. Zoom (`us.zoom.xos`) is running and the user is in a meeting.
//! 2. The test binary has been granted Accessibility (System Settings
//!    → Privacy & Security → Accessibility). The first run will fail
//!    with `AccessibilityDenied` until granted.
//! 3. Someone — usually the user — is talking during the 5-second
//!    capture window so the speaker indicator actually changes value.
//!
//! See `docs/archives/manual-test-matrix.md` → "Zoom AX observer (heron-zoom)".

use heron_types::{SessionClock, SessionId};
use heron_zoom::{AxBackend, AxObserverBackend};
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
#[ignore = "requires HERON_ZOOM_RUNNING + live Zoom call + Accessibility grant"]
async fn observer_emits_speaker_event_on_live_zoom_call() {
    if std::env::var_os("HERON_ZOOM_RUNNING").is_none() {
        eprintln!(
            "skipping: HERON_ZOOM_RUNNING is unset. \
             See docs/archives/manual-test-matrix.md → 'Zoom AX observer (heron-zoom)'."
        );
        return;
    }

    let backend = AxObserverBackend::new();
    let (tx_evt, _rx_evt) = mpsc::channel(64);
    let (tx_speaker, mut rx_speaker) = mpsc::channel(64);

    let handle = backend
        .start(SessionId::nil(), SessionClock::new(), tx_speaker, tx_evt)
        .await
        .expect("AxObserverBackend::start should succeed with Zoom running + AX granted");

    // 5-second capture window.
    let recv = tokio::time::timeout(Duration::from_secs(5), rx_speaker.recv()).await;

    let result = handle.stop().await;
    assert!(result.is_ok(), "stop() should be clean, got {result:?}");

    match recv {
        Ok(Some(ev)) => {
            eprintln!("captured SpeakerEvent: {ev:?}");
        }
        Ok(None) => panic!(
            "channel closed before any SpeakerEvent arrived; \
             observer task likely tore down early"
        ),
        Err(_) => panic!(
            "no SpeakerEvent received in 5s. Check that someone is \
             talking, Accessibility is granted, and the (role, subrole, \
             identifier) triple in ZoomAxHelper.swift matches the live \
             Zoom AX tree (see docs/archives/plan.md §3.3)"
        ),
    }
}
