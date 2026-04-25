//! `heron-audio` — per-app audio capture, AEC, and recording.
//!
//! This crate exposes the v0 surface from
//! [`docs/implementation.md`](../../../docs/implementation.md) §6.2.
//! The real implementation (Core Audio process tap, WebRTC APM AEC,
//! disk-spill ringbuffer) lands across weeks 2–3 (§6 + §7).
//!
//! The type signatures committed here are load-bearing: downstream
//! crates (`heron-zoom` aligner in week 7, `heron-vault` writer in
//! week 10, the Tauri shell in week 11) compile and test against
//! them today, so changing the shape later means a wider blast
//! radius. Keep this surface stable; fill in the impls.

use std::path::{Path, PathBuf};
use std::time::Duration;

use heron_types::{Channel, Event, SessionClock, SessionId};
use thiserror::Error;
use tokio::sync::broadcast;

pub mod aec;
pub mod backpressure;
pub mod disk;
pub mod mic_capture;
#[cfg(target_os = "macos")]
pub mod process_tap;
pub mod ringbuffer;

pub use aec::{APM_FRAME_SAMPLES, APM_SAMPLE_RATE_HZ, EchoCanceller};
pub use backpressure::{BackpressureMonitor, SATURATION_THRESHOLD};
pub use disk::{DiskError, MIN_FREE_BYTES_TO_RECORD, can_record, free_bytes};
pub use ringbuffer::{
    Ringbuffer, RingbufferError, RingbufferState, recover, scan_recoverable_sessions,
};

/// One PCM frame as emitted by the capture pipeline. After APM/AEC
/// processing for the `Mic` channel, before any STT.
#[derive(Debug, Clone)]
pub struct CaptureFrame {
    /// Whether this frame came from the mic input (post-AEC) or the
    /// per-app process tap on the meeting client.
    pub channel: Channel,
    /// Mach `mach_absolute_time` at the start of this frame's PCM
    /// window. Intended for downstream conversion via
    /// [`SessionClock::host_to_session_secs`].
    pub host_time: u64,
    /// Convenience: `host_time` already converted into seconds since
    /// the session anchor. Equivalent to
    /// `clock.host_to_session_secs(self.host_time)`.
    pub session_secs: f64,
    /// 48 kHz mono f32 samples in `[-1.0, 1.0]`. Frame size is
    /// implementation-defined (typically 10 ms = 480 samples).
    pub samples: Vec<f32>,
}

/// Files written by the capture pipeline at session stop.
///
/// `mic_clean` is what the STT pipeline (week 4–5, §8) consumes;
/// `mic` and `tap` are kept for the AEC correctness regression
/// (§6.3) and for re-running APM offline if the AEC config changes.
#[derive(Debug, Clone)]
pub struct StopArtifacts {
    pub mic: PathBuf,
    pub tap: PathBuf,
    pub mic_clean: PathBuf,
    pub duration: Duration,
    /// Number of frames the realtime → SPSC → APM pipeline dropped
    /// under back-pressure (§7.4). `0` on a healthy session.
    pub dropped_frames: u32,
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("not yet implemented (arrives week 2 per §6); tracking in heron-audio v0 surface")]
    NotYetImplemented,
    #[error("target meeting app not running: {bundle_id}")]
    ProcessNotFound { bundle_id: String },
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("capture pipeline aborted: {0}")]
    Aborted(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One-shot factory for an audio capture session.
///
/// Real impl (week 2): spins up the Core Audio process tap on
/// `target_bundle_id`, opens the user's mic via `cpal`, wires WebRTC
/// APM (`process_reverse_stream` for AEC, future PR), and starts a
/// SPSC ring from each realtime callback into the broadcast channel
/// behind [`AudioCaptureHandle::frames`].
pub struct AudioCapture {
    _private: (),
}

impl AudioCapture {
    /// Start a capture session.
    ///
    /// On macOS this:
    /// 1. Resolves the meeting client by bundle id and builds a Core
    ///    Audio process tap (`Channel::Tap` frames). Required — a tap
    ///    failure fails the whole call.
    /// 2. Opens the default microphone via cpal (`Channel::Mic`
    ///    frames). Best-effort — a mic failure (no input device, TCC
    ///    denied, format unsupported) logs a warning, emits
    ///    `Event::CaptureDegraded`, and continues with a tap-only
    ///    session. The tap is the load-bearing capture surface for
    ///    v0; a tap-only session is still useful for transcribing
    ///    the remote side of the call.
    ///
    /// Both pipelines push into the same broadcast channel; consumers
    /// differentiate via `frame.channel`. WebRTC APM AEC isn't wired
    /// yet — mic frames are raw at this point. AEC integration is the
    /// next PR.
    ///
    /// On non-Apple platforms this returns
    /// [`AudioError::NotYetImplemented`] — there is no Core Audio
    /// process tap off-Apple, and `cpal` is gated to macOS in our
    /// `Cargo.toml` for v0 (§6).
    pub async fn start(
        session_id: SessionId,
        target_bundle_id: &str,
        cache_dir: &Path,
    ) -> Result<AudioCaptureHandle, AudioError> {
        // Cache dir is unused until §7 (week-3 disk-spill ringbuffer).
        // Touch it so clippy's unused_variables stays quiet without
        // renaming the public API. `session_id` *is* used now —
        // threaded into the IO-proc pipeline so `Event::CaptureDegraded`
        // events carry the correct id.
        let _ = cache_dir;

        #[cfg(target_os = "macos")]
        {
            let clock = SessionClock::new();
            let (frames_tx, frames_rx) = broadcast::channel::<CaptureFrame>(1024);
            let (events_tx, events_rx) = broadcast::channel::<Event>(256);
            // Pass `events_tx` + `session_id` through so the IO-proc
            // consumer task can fire `Event::CaptureDegraded` when
            // the SPSC ring saturates (§7.4).
            let pipeline = process_tap::open_tap(
                target_bundle_id,
                frames_tx.clone(),
                events_tx.clone(),
                session_id,
                clock,
            )?;

            // Mic capture runs in parallel with the tap. A mic failure
            // (no input device, TCC denied, format unsupported) is NOT
            // fatal: the orchestrator can fall back to a tap-only
            // session (still useful for transcribing the remote side
            // of the call). We surface the degradation as
            // `Event::CaptureDegraded` so the UI/onboarding flow can
            // prompt the user to fix the underlying cause, then
            // continue without a mic handle. The tap is the
            // load-bearing capture surface for v0.
            let mic_handle = match mic_capture::start_mic(
                frames_tx.clone(),
                events_tx.clone(),
                session_id,
                clock,
            ) {
                Ok(h) => Some(h),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "mic capture failed; continuing with tap-only session"
                    );
                    // Best-effort signal to subscribers. Send returns
                    // Err only when there are no receivers; ignore.
                    let _ = events_tx.send(Event::CaptureDegraded {
                        id: session_id,
                        at: Duration::from_secs(0),
                        dropped_frames: 0,
                        reason: format!("mic capture unavailable: {err}"),
                    });
                    None
                }
            };

            // Hold both the cidre resources (`pipeline`) + the cpal
            // mic handle and the sender end of the broadcast channels
            // alive for the lifetime of the handle — once the sender
            // is dropped, every receiver sees `RecvError::Closed`
            // immediately, which would defeat the point of returning
            // the handle.
            Ok(AudioCaptureHandle {
                frames: frames_rx,
                events: events_rx,
                clock,
                _macos_pipeline: Some(MacosPipelineGuard {
                    _pipeline: pipeline,
                    _mic: mic_handle,
                    _frames_tx: frames_tx,
                    _events_tx: events_tx,
                }),
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            // No process tap off-Apple — Linux/Windows builds exist
            // only so that `cargo check` works on CI runners that
            // can't compile cidre. They never run heron in anger.
            let _ = (session_id, target_bundle_id);
            Err(AudioError::NotYetImplemented)
        }
    }
}

/// macOS-only owner of the cidre + cpal resources backing a live
/// capture session. Held inside [`AudioCaptureHandle`] as an opaque
/// private field so dropping the handle releases the tap + aggregate
/// device + mic stream + broadcast senders in the right order.
///
/// Drop order (Rust drops fields in declaration order):
/// 1. `_pipeline` first — stops the tap IO proc, releases the tap.
/// 2. `_mic` next — stops the cpal input stream (callback quiescent),
///    aborts the mic consumer task, frees the `MicCtx` box.
/// 3. `_frames_tx` / `_events_tx` last — receivers stay alive through
///    every prior step, so a final flush can land before the channel
///    closes.
#[cfg(target_os = "macos")]
struct MacosPipelineGuard {
    _pipeline: process_tap::TapPipeline,
    /// `None` when mic capture failed at session start (no input
    /// device / TCC denied / format unsupported). The `_pipeline`
    /// keeps producing tap frames either way.
    _mic: Option<mic_capture::MicHandle>,
    _frames_tx: broadcast::Sender<CaptureFrame>,
    _events_tx: broadcast::Sender<Event>,
}

/// Live handle to a capture session.
///
/// Holds the receiving end of two broadcast channels:
/// - `frames`: every post-APM PCM frame, both channels.
/// - `events`: lifecycle events from [`heron_types::Event`] (mute /
///   unmute, device change, capture-degraded, etc.).
///
/// The clock is captured at session start and exposed so downstream
/// crates that need to align AX events against the audio timeline
/// (the §9.3 aligner) don't have to reconstruct it.
pub struct AudioCaptureHandle {
    pub frames: broadcast::Receiver<CaptureFrame>,
    pub events: broadcast::Receiver<Event>,
    pub clock: SessionClock,
    /// Owns the cidre `TapPipeline` + the broadcast senders so the
    /// receivers above stay alive for the lifetime of the handle.
    /// `None` on non-Apple builds (where `start()` always returns
    /// `Err(NotYetImplemented)` so a handle is never constructed
    /// anyway).
    #[cfg(target_os = "macos")]
    _macos_pipeline: Option<MacosPipelineGuard>,
}

impl AudioCaptureHandle {
    /// Stop the session, flush in-flight frames, finalize the
    /// `mic.wav` / `tap.wav` / `mic_clean.wav` files, and return
    /// their paths.
    pub async fn stop(self) -> Result<StopArtifacts, AudioError> {
        Err(AudioError::NotYetImplemented)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Off-Apple targets have no Core Audio, so `start` is hard-wired
    /// to return `NotYetImplemented`. Locked down so the cfg gate in
    /// `start()` doesn't drift.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn start_returns_not_yet_implemented_off_apple() {
        let session = SessionId::nil();
        let cache = std::env::temp_dir();
        let result = AudioCapture::start(session, "us.zoom.xos", &cache).await;
        assert!(matches!(result, Err(AudioError::NotYetImplemented)));
    }

    /// On macOS we expect either a live handle (target app running +
    /// TCC granted) or a recoverable error like
    /// `ProcessNotFound` / `PermissionDenied` / `Aborted` — but
    /// **never** `NotYetImplemented`. That regression is what catches
    /// a future patch accidentally short-circuiting the macOS branch
    /// back to the v0 stub.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn start_does_not_return_not_yet_implemented_on_macos() {
        // Pick a bundle id that is almost certainly NOT running on a
        // CI runner — that way we exercise the lookup path without
        // requiring TCC. A live tap requires "system audio recording"
        // grant, which CI doesn't have, so an error is expected; the
        // assertion is just that it's not `NotYetImplemented`.
        let session = SessionId::nil();
        let cache = std::env::temp_dir();
        let result = AudioCapture::start(session, "com.heron.no-such-app", &cache).await;
        match result {
            Err(AudioError::NotYetImplemented) => {
                panic!("macOS branch must not return NotYetImplemented");
            }
            Err(AudioError::ProcessNotFound { .. })
            | Err(AudioError::PermissionDenied(_))
            | Err(AudioError::Aborted(_))
            | Ok(_) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn capture_frame_round_trip_via_session_clock() {
        // The CaptureFrame contract: host_time is raw mach ticks,
        // session_secs is what host_to_session_secs returns. Verify
        // the two are at least consistent with each other when fed
        // through a fresh SessionClock — this is the invariant that
        // the §9.3 aligner relies on.
        let clock = SessionClock::new();
        let host_time = clock.mach_anchor + 480_000;
        let frame = CaptureFrame {
            channel: Channel::Mic,
            host_time,
            session_secs: clock.host_to_session_secs(host_time),
            samples: vec![0.0; 480],
        };
        let recomputed = clock.host_to_session_secs(frame.host_time);
        assert!(
            (frame.session_secs - recomputed).abs() < 1e-9,
            "frame.session_secs must agree with clock.host_to_session_secs"
        );
    }

    #[test]
    fn audio_error_is_send_sync() {
        // Capture errors cross broadcast channels, so they must be
        // Send + Sync + 'static. Compile-time check.
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<AudioError>();
        assert_send_sync::<CaptureFrame>();
        assert_send_sync::<StopArtifacts>();
    }
}
