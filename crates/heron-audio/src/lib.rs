//! `heron-audio` — per-app audio capture, AEC, and recording.
//!
//! This crate exposes the v0 surface from
//! [`docs/implementation.md`](../../docs/implementation.md) §6.2.
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
    PermissionDenied(&'static str),
    #[error("capture pipeline aborted: {0}")]
    Aborted(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One-shot factory for an audio capture session.
///
/// Real impl (week 2): spins up the Core Audio process tap on
/// `target_bundle_id`, opens the user's mic via `cpal`, wires WebRTC
/// APM (`process_reverse_stream` for AEC), and starts a SPSC ring
/// from the realtime callback into the APM thread, which broadcasts
/// post-APM frames out via [`AudioCaptureHandle::frames`].
pub struct AudioCapture {
    _private: (),
}

impl AudioCapture {
    /// Start a capture session.
    ///
    /// Returns immediately once the realtime taps are wired (or
    /// errors if the target app isn't running / TCC denied). Frames
    /// flow over [`AudioCaptureHandle::frames`] for the lifetime of
    /// the handle; `stop()` flushes any in-flight buffers before
    /// returning [`StopArtifacts`].
    ///
    /// Until the week-2 implementation lands this returns
    /// [`AudioError::NotYetImplemented`] so the type signature is
    /// usable from downstream crates without any audio actually
    /// flowing.
    pub async fn start(
        session_id: SessionId,
        target_bundle_id: &str,
        cache_dir: &Path,
    ) -> Result<AudioCaptureHandle, AudioError> {
        // Touch the args so clippy's unused_variables stays quiet
        // without renaming the public API.
        let _ = (session_id, target_bundle_id, cache_dir);
        Err(AudioError::NotYetImplemented)
    }
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

    #[tokio::test]
    async fn start_returns_not_yet_implemented_until_week_2() {
        let session = SessionId::nil();
        let cache = std::env::temp_dir();
        let result = AudioCapture::start(session, "us.zoom.xos", &cache).await;
        assert!(matches!(result, Err(AudioError::NotYetImplemented)));
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
