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
    #[error(transparent)]
    Send(#[from] tokio::sync::mpsc::error::SendError<SpeakerEvent>),
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
        _out: mpsc::Sender<SpeakerEvent>,
        _events: mpsc::Sender<Event>,
    ) -> Result<AxHandle, AxError> {
        Err(AxError::NotYetImplemented)
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
    async fn observer_stub_returns_not_yet_implemented() {
        let backend = AxObserverBackend::new();
        let (tx_evt, _rx_evt) = mpsc::channel(8);
        let (tx_speaker, _rx_speaker) = mpsc::channel(8);
        let result = backend
            .start(SessionId::nil(), SessionClock::new(), tx_speaker, tx_evt)
            .await;
        assert!(matches!(result, Err(AxError::NotYetImplemented)));
        assert_eq!(backend.name(), "ax-observer");
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
