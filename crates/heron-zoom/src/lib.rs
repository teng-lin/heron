//! `heron-zoom` — speaker attribution from Zoom's AX tree.
//!
//! Surface per [`docs/implementation.md`](../../../docs/implementation.md)
//! §9.1; the implementation reflects the §3.3 spike outcome
//! recorded at `fixtures/zoom/spike-triple/README.md`. Two backends
//! ship in v1:
//!
//! - [`AxObserverBackend`] — pairs with `swift/zoomax-helper`'s
//!   polling enumerator (`ZoomAxHelper.swift`). The Swift side
//!   walks Zoom's AX tree at 4 Hz, parses each participant tile's
//!   `AXDescription` (the only mute-state signal Zoom 7.0.0
//!   surfaces — the active-speaker frame is Metal-rendered outside
//!   the AX tree), and queues a [`SpeakerEvent`] JSONL line for
//!   every transition (mute/unmute, participant join/leave). The
//!   Rust side here drains that queue at 50 ms and forwards parsed
//!   events upstream.
//! - [`AxPollingBackend`] — pure-Rust fallback that polls
//!   `AXUIElementCopyAttributeValue` directly when the Swift bridge
//!   is unavailable (red path). Stub today; lights up if the §9.2
//!   selector ever picks it.
//!
//! The aligner ([`aligner::Aligner`]) intersects the resulting
//! `started=true` / `started=false` intervals with tap-audio turns
//! to attribute speakers; in the dominant 1:1 client-meeting case
//! exactly one remote participant is unmuted, so attribution is
//! exact. Free-for-all 3+ calls degrade to "best guess by overlap"
//! per the §20 risk-reducer — see the Swift module header for the
//! full rationale.

use async_trait::async_trait;
use heron_types::{Event, SessionClock, SessionId, SpeakerEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tokio::sync::mpsc;

/// Bundle id for the Zoom desktop client. Used by [`AxObserverBackend`]
/// to locate the running Zoom process via NSRunningApplication.
const ZOOM_BUNDLE_ID: &str = "us.zoom.xos";

/// Cadence at which the Rust worker drains the Swift event queue.
/// The Swift side polls Zoom's AX tree at 4 Hz (~250 ms) and
/// queues transitions; this 50 ms drain interval keeps end-to-end
/// detection latency bounded while leaving the bridge's own poll
/// cadence as the dominant cost.
const AX_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

pub mod aligner;
pub mod ax_bridge;

pub use aligner::{ATTRIBUTION_GAP_THRESHOLD, Aligner, CONFIDENCE_FLOOR, DEFAULT_EVENT_LAG};
pub use ax_bridge::{AxBridgeError, AxStatus, ax_dump_tree, ax_poll, ax_register, ax_release};

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
    /// JoinHandle for the spawn_blocking polling worker. `None` only
    /// in unit tests that exercise the stop() shape without a live
    /// observer.
    worker: Option<tokio::task::JoinHandle<()>>,
    /// Cooperative shutdown signal. `stop()` flips this; the worker
    /// checks it each iteration so it can exit promptly even when
    /// `ax_poll` is steadily returning `Ok(None)` (i.e. the receiver
    /// is alive but no events are flowing).
    stop_flag: Arc<AtomicBool>,
}

impl AxHandle {
    pub async fn stop(mut self) -> Result<(), AxError> {
        // 1. Tell the worker to bail on its next iteration.
        self.stop_flag.store(true, Ordering::Release);

        // 2. Tear down the Swift observer so any in-flight `ax_poll`
        //    returns promptly. `ax_release` is idempotent and the
        //    polling-loop's own teardown is too, so a double release
        //    is fine.
        let _ = tokio::task::spawn_blocking(ax_release).await;

        // 3. Wait for the worker to actually exit so the caller knows
        //    no more events will be sent on `out`. Take the worker
        //    out of `self` so `Drop` (which still runs after `stop`)
        //    has nothing left to do.
        if let Some(h) = self.worker.take() {
            let _ = h.await;
        }
        Ok(())
    }
}

/// Best-effort teardown if the caller forgets to `stop().await`. The
/// worker thread cannot be awaited synchronously, but we can still
/// flip the stop flag + tell Swift to release so the worker's next
/// iteration exits and the underlying CFRunLoop unwinds. Prefer
/// `stop().await` whenever possible — Drop here is a safety net, not
/// the primary path.
impl Drop for AxHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if self.worker.is_some() {
            // Best-effort, synchronous Swift teardown. We cannot await
            // the worker from a Drop impl, but the worker will see
            // `stop_flag` on its next iteration and exit on its own.
            if let Err(e) = ax_release() {
                tracing::warn!(
                    error = %e,
                    "heron-zoom: ax_release failed during AxHandle::drop teardown",
                );
            }
        }
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
        clock: SessionClock,
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
        // exits — whether that's via the `stop_flag` (set by
        // `AxHandle::stop`), `out` closing (send-error breaks the
        // loop), or `ax_poll` itself returning an error.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let worker_stop_flag = Arc::clone(&stop_flag);
        let handle = tokio::task::spawn_blocking(move || {
            // Polling loop. Each pass: check `stop_flag`, try
            // `ax_poll`. On a `Some`, parse + send. On `None`, sleep.
            // On error, log + exit (the orchestrator surfaces the
            // AttributionDegraded event; our job is just to bail
            // cleanly).
            loop {
                if worker_stop_flag.load(Ordering::Acquire) {
                    break;
                }
                match ax_poll() {
                    Ok(Some(line)) => match serde_json::from_str::<SpeakerEvent>(&line) {
                        Ok(mut ev) => {
                            // The Swift bridge emits `t = 0.0` because
                            // it has no session-clock reference (per
                            // ZoomAxHelper.swift); stamp the receive
                            // time here so SpeakerEvent.t lands in
                            // session-secs as the aligner expects.
                            // `now_session_secs` is monotonic (mach
                            // anchor on Apple) so a mid-meeting NTP
                            // adjustment can't regress `t` below a
                            // prior event and create a degenerate
                            // `SpeakingInterval` (t0 >= t1) that the
                            // aligner's interval_overlap silently
                            // returns 0 for. Worst-case skew is the
                            // polling cadence (~250 ms in the bridge
                            // + 50 ms here), well inside the aligner's
                            // 350 ms default event_lag prior.
                            ev.t = clock.now_session_secs();
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
            worker: Some(handle),
            stop_flag,
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
        let handle = AxHandle {
            worker: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
        };
        handle.stop().await.expect("stop");
    }

    #[test]
    fn ax_handle_drop_flips_stop_flag() {
        // Drop is the safety-net path when callers forget to await
        // `stop()`. It must at minimum signal the worker to exit so
        // the polling loop tears down on its own. We assert the
        // observable side-effect: `stop_flag` is true after drop.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let observer = Arc::clone(&stop_flag);
        {
            let _handle = AxHandle {
                worker: None, // skip the real Swift teardown branch
                stop_flag,
            };
            assert!(!observer.load(Ordering::Acquire));
        }
        assert!(
            observer.load(Ordering::Acquire),
            "Drop must flip the stop flag so the polling worker exits"
        );
    }
}
