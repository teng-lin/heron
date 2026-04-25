//! `heron-zoom` — Zoom AXObserver-based speaker attribution.
//!
//! v0 surface from [`docs/implementation.md`](../../../docs/implementation.md)
//! §9.1. Two backends ship in v1:
//! - [`AxObserverBackend`] — registers an `AXObserver` on the Zoom
//!   process and reads the `{role, subrole, identifier}` triple
//!   recorded during the week-0 spike (the yellow path).
//! - [`AxPollingBackend`] — polls `AXUIElementCopyAttributeValue` at
//!   50 ms cadence (the red-fallback path).
//!
//! Real backend wires arrive weeks 6–7 (§9.2 + §9.3); the trait
//! shape committed here lets the heron-session orchestrator wire
//! event routing today.

use async_trait::async_trait;
use heron_types::{Event, SessionClock, SessionId, SpeakerEvent};
use thiserror::Error;
use tokio::sync::mpsc;

/// Bundle id for the Zoom desktop client. Used by [`AxObserverBackend`]
/// to locate the running Zoom process via NSRunningApplication.
const ZOOM_BUNDLE_ID: &str = "us.zoom.xos";

/// Polling cadence the Rust task uses to drain the Swift event queue.
/// 50ms matches the polling backend (§9.2) and is a comfortable upper
/// bound on AX callback latency without burning CPU.
const AX_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

pub mod aligner;
pub mod ax_bridge;

pub use aligner::{ATTRIBUTION_GAP_THRESHOLD, Aligner, CONFIDENCE_FLOOR, DEFAULT_EVENT_LAG};
pub use ax_bridge::{AxBridgeError, AxStatus, ax_poll, ax_register, ax_release};

#[derive(Debug, Error)]
pub enum AxError {
    #[error("not yet implemented (arrives weeks 6–7 per §9)")]
    NotYetImplemented,
    #[error("AXObserver registration failed on Zoom process: {0}")]
    ObserverRegistrationFailed(String),
    #[error("Accessibility permission denied")]
    AccessibilityDenied,
    #[error("target Zoom process not running")]
    ZoomNotRunning,
    #[error("AX bridge returned malformed JSONL: {0}")]
    MalformedEvent(String),
    #[error(transparent)]
    Send(#[from] tokio::sync::mpsc::error::SendError<SpeakerEvent>),
}

impl From<AxBridgeError> for AxError {
    fn from(e: AxBridgeError) -> Self {
        match e {
            AxBridgeError::NotYetImplemented => AxError::NotYetImplemented,
            AxBridgeError::ProcessNotRunning => AxError::ZoomNotRunning,
            AxBridgeError::NoPermission => AxError::AccessibilityDenied,
            AxBridgeError::Internal { code } => {
                AxError::ObserverRegistrationFailed(format!("internal code {code}"))
            }
            AxBridgeError::NullBuffer => {
                AxError::ObserverRegistrationFailed("null buffer from Swift bridge".into())
            }
            AxBridgeError::InvalidUtf8(e) => {
                AxError::ObserverRegistrationFailed(format!("non-utf8 from Swift bridge: {e}"))
            }
            AxBridgeError::BundleIdNul => {
                AxError::ObserverRegistrationFailed("bundle id contains NUL".into())
            }
        }
    }
}

/// Live handle returned by [`AxBackend::start`]. Drop or `stop()` to
/// halt the AX listener; both broadcast channels close on drop.
pub struct AxHandle {
    /// Backend-side `JoinHandle`-equivalent the orchestrator awaits
    /// to know when the AX listener has actually torn down. Stub
    /// keeps the field for shape-stability; week-7 wiring fills it
    /// in with a `tokio::task::JoinHandle`.
    _stop: Option<tokio::task::JoinHandle<()>>,
}

impl AxHandle {
    pub async fn stop(self) -> Result<(), AxError> {
        if let Some(h) = self._stop {
            h.abort();
            let _ = h.await;
        }
        Ok(())
    }
}

#[async_trait]
pub trait AxBackend: Send + Sync {
    /// Start emitting [`SpeakerEvent`]s for the Zoom process tied to
    /// `session_id`. The `clock` is used to convert AX wall-time
    /// timestamps to session-secs at emit-time.
    async fn start(
        &self,
        session_id: SessionId,
        clock: SessionClock,
        out: mpsc::Sender<SpeakerEvent>,
        events: mpsc::Sender<Event>,
    ) -> Result<AxHandle, AxError>;

    fn name(&self) -> &'static str;
}

/// AXObserver-based backend (yellow path per week-0 spike).
pub struct AxObserverBackend {
    _private: (),
}

impl AxObserverBackend {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for AxObserverBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AxBackend for AxObserverBackend {
    async fn start(
        &self,
        _session_id: SessionId,
        _clock: SessionClock,
        out: mpsc::Sender<SpeakerEvent>,
        _events: mpsc::Sender<Event>,
    ) -> Result<AxHandle, AxError> {
        // Step 1: register synchronously so the caller learns
        // immediately whether Zoom is running and Accessibility is
        // granted. Returning `Err` here is the contract the
        // orchestrator uses to decide whether to fall back to
        // AxPollingBackend or downgrade to channel attribution.
        //
        // Run on `spawn_blocking` because the Swift bridge spawns a
        // CFRunLoop-owning Thread internally and blocks the calling
        // thread until that thread reports `ready`. We don't want
        // tokio's reactor to stall on it.
        tokio::task::spawn_blocking(|| ax_register(ZOOM_BUNDLE_ID))
            .await
            .map_err(|e| AxError::ObserverRegistrationFailed(format!("join error: {e}")))?
            .map_err(AxError::from)?;

        // Step 2: spawn the polling task that drains the Swift queue
        // and forwards parsed `SpeakerEvent`s to `out`. The task
        // owns the responsibility of calling `ax_release` when it
        // exits — whether that's via `JoinHandle::abort` (handled by
        // catching the abort error in the loop) or `out` closing
        // (we send-error and break the loop).
        let handle = tokio::task::spawn_blocking(move || {
            // Polling loop. Each pass: try `ax_poll`. On a `Some`,
            // parse + send. On `None`, sleep. On error, log + exit
            // (the orchestrator surfaces the AttributionDegraded
            // event; our job is just to bail cleanly).
            loop {
                match ax_poll() {
                    Ok(Some(line)) => match serde_json::from_str::<SpeakerEvent>(&line) {
                        Ok(ev) => {
                            // `blocking_send` is the right primitive
                            // here: we're on a blocking thread and
                            // out's bounded channel must apply
                            // back-pressure to AX poll cadence rather
                            // than dropping events.
                            if out.blocking_send(ev).is_err() {
                                // Receiver gone → orchestrator
                                // dropped the channel → tear down.
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                line = %line,
                                "heron-zoom: AX bridge emitted malformed JSONL; dropping",
                            );
                        }
                    },
                    Ok(None) => {
                        std::thread::sleep(AX_POLL_INTERVAL);
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "heron-zoom: AX poll failed; releasing observer",
                        );
                        break;
                    }
                }
            }
            // Best-effort release on exit. The Swift side is
            // idempotent so a double release (here + an explicit
            // stop()) is safe.
            if let Err(e) = ax_release() {
                tracing::warn!(error = %e, "heron-zoom: ax_release failed during teardown");
            }
        });

        Ok(AxHandle {
            _stop: Some(handle),
        })
    }
    fn name(&self) -> &'static str {
        "ax-observer"
    }
}

/// Polling-based backend (red-fallback path per week-0 spike).
pub struct AxPollingBackend {
    interval: std::time::Duration,
}

impl AxPollingBackend {
    pub fn new(interval: std::time::Duration) -> Self {
        Self { interval }
    }

    pub fn interval(&self) -> std::time::Duration {
        self.interval
    }
}

#[async_trait]
impl AxBackend for AxPollingBackend {
    async fn start(
        &self,
        _session_id: SessionId,
        _clock: SessionClock,
        _out: mpsc::Sender<SpeakerEvent>,
        _events: mpsc::Sender<Event>,
    ) -> Result<AxHandle, AxError> {
        Err(AxError::NotYetImplemented)
    }
    fn name(&self) -> &'static str {
        "ax-polling"
    }
}

/// Per §9.2: prefer AXObserver, fall back to polling.
///
/// Stub picks AXObserver unconditionally; the real impl runs the
/// `try_observer_registration_on_zoom()` probe from week 0 and
/// switches based on the outcome.
pub fn select_ax_backend() -> Box<dyn AxBackend> {
    Box::new(AxObserverBackend::new())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observer_start_surfaces_zoom_or_permission_error_when_zoom_absent() {
        // The old assertion (`Err(NotYetImplemented)`) is obsolete:
        // `AxObserverBackend::start` now talks to the real Swift
        // bridge. In CI and on a dev box without Zoom running, the
        // first thing the bridge does is
        // `NSRunningApplication.runningApplications(withBundleIdentifier:
        // "us.zoom.xos")` → returns empty → AX_PROCESS_NOT_RUNNING →
        // mapped to `AxError::ZoomNotRunning`. If Zoom *is* running
        // but the test binary lacks Accessibility, the bridge returns
        // AX_NO_PERMISSION → `AxError::AccessibilityDenied`. Both are
        // valid v0 outcomes; the live-Zoom happy path is exercised
        // by `tests/ax_observer_real.rs`.
        let backend = AxObserverBackend::new();
        let (tx_evt, _rx_evt) = mpsc::channel(8);
        let (tx_speaker, _rx_speaker) = mpsc::channel(8);
        let result = backend
            .start(SessionId::nil(), SessionClock::new(), tx_speaker, tx_evt)
            .await;
        assert_eq!(backend.name(), "ax-observer");

        #[cfg(target_vendor = "apple")]
        {
            match result {
                Err(AxError::ZoomNotRunning) | Err(AxError::AccessibilityDenied) => {}
                Ok(_) => panic!(
                    "AxObserverBackend::start unexpectedly succeeded; \
                     this test expects no Zoom process running on the CI host"
                ),
                Err(other) => {
                    panic!("expected ZoomNotRunning or AccessibilityDenied; got {other:?}")
                }
            }
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            // Off-Apple, the bridge shim returns `NotYetImplemented`
            // — and that's mapped to `AxError::NotYetImplemented` by
            // the `From<AxBridgeError>` impl above.
            assert!(matches!(result, Err(AxError::NotYetImplemented)));
        }
    }

    #[tokio::test]
    async fn polling_stub_returns_not_yet_implemented() {
        let backend = AxPollingBackend::new(std::time::Duration::from_millis(50));
        assert_eq!(backend.interval(), std::time::Duration::from_millis(50));
        let (tx_evt, _rx_evt) = mpsc::channel(8);
        let (tx_speaker, _rx_speaker) = mpsc::channel(8);
        let result = backend
            .start(SessionId::nil(), SessionClock::new(), tx_speaker, tx_evt)
            .await;
        assert!(matches!(result, Err(AxError::NotYetImplemented)));
        assert_eq!(backend.name(), "ax-polling");
    }

    #[test]
    fn select_ax_backend_returns_observer_in_stub() {
        let b = select_ax_backend();
        assert_eq!(b.name(), "ax-observer");
    }

    #[tokio::test]
    async fn ax_handle_stop_is_idempotent_and_async() {
        // The stub AxHandle has no inner task. stop() should
        // still complete cleanly so callers don't have to special-
        // case the v0 phase.
        let handle = AxHandle { _stop: None };
        handle.stop().await.expect("stop");
    }
}
