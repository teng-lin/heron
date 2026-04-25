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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use heron_types::{Channel, Event, SessionClock, SessionId};
use thiserror::Error;
use tokio::sync::broadcast;
#[cfg(target_os = "macos")]
use tokio::task::JoinHandle;

pub mod aec;
pub mod backpressure;
pub mod disk;
pub mod mic_capture;
#[cfg(target_os = "macos")]
pub mod process_tap;
pub mod ringbuffer;
pub mod wav_writer;

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
///
/// ## Empty-WAV contract
///
/// **All three paths are always populated**, even when the
/// corresponding channel never emitted a frame. A tap-only session
/// (mic capture failed at start) still gets a `mic.wav` and
/// `mic_clean.wav` on disk — they're 0-sample but otherwise valid
/// WAVs (header only). This keeps downstream consumers (the §6.3
/// test rig, the week-9 archival encode pass) free of conditional
/// "does the file exist?" branches; an empty channel is a `0`-frame
/// WAV, not a missing path. See [`crate::wav_writer`] for the
/// finalization logic.
#[derive(Debug, Clone)]
pub struct StopArtifacts {
    pub mic: PathBuf,
    pub tap: PathBuf,
    pub mic_clean: PathBuf,
    pub duration: Duration,
    /// Number of frames dropped by the broadcast → AEC consumer when
    /// the AEC task lagged the producers (§7.4 back-pressure). `0` on
    /// a healthy session. SPSC-ring drops on the realtime producer
    /// side are surfaced separately via `Event::CaptureDegraded` on
    /// the events broadcast and are NOT folded into this counter
    /// today (see the field doc on `AudioCaptureHandle::dropped_frames`).
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
        #[cfg(target_os = "macos")]
        {
            let clock = SessionClock::new();
            let started_at = Instant::now();
            let (frames_tx, frames_rx) = broadcast::channel::<CaptureFrame>(1024);
            let (events_tx, events_rx) = broadcast::channel::<Event>(256);

            // Subscribe BEFORE the producers start so the AEC + WAV
            // task can't miss the very first Mic / Tap frame. If we
            // subscribed after `open_tap` returned, a fast-firing
            // tap IO proc could land the first frame ahead of our
            // subscription and AEC would never see it.
            let aec_rx = frames_tx.subscribe();

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

            // Build the AEC task: subscribes to the broadcast,
            // routes Mic → APM near-end + emits MicClean, Tap →
            // APM far-end, ignores everything else (including its
            // own MicClean reflections). Also owns the per-channel
            // WAV writers so `stop()` can finalize them, and the
            // task-side `frames_tx` so `stop()` can drop it to let
            // the broadcast actually close. See `AecTaskState` doc.
            let writers = wav_writer::PerChannelWavWriters::new(cache_dir, session_id)?;
            let dropped_frames = Arc::new(AtomicU64::new(0));
            let aec_state = Arc::new(tokio::sync::Mutex::new(AecTaskState {
                writers: Some(writers),
                frames_tx: Some(frames_tx.clone()),
            }));
            let aec_task = spawn_aec_task(
                aec_rx,
                events_tx.clone(),
                session_id,
                started_at,
                Arc::clone(&aec_state),
                Arc::clone(&dropped_frames),
            )?;

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
                started_at,
                dropped_frames: Arc::clone(&dropped_frames),
                aec_state: Some(aec_state),
                aec_task: Some(AecTaskGuard(Some(aec_task))),
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
            let _ = (session_id, target_bundle_id, cache_dir);
            Err(AudioError::NotYetImplemented)
        }
    }
}

/// Shared state between the AEC task and `AudioCaptureHandle::stop`:
/// the per-channel WAV writers and the AEC task's clone of the
/// frames sender. The mutex is taken once per frame inside the AEC
/// task (no contention except at stop time when `stop()` reaches in
/// to finalize), so it doesn't fight the realtime budget.
///
/// Both fields go to `None` at stop time:
/// - `writers` so the task short-circuits its disk writes.
/// - `frames_tx` so the broadcast channel can actually close once
///   `_macos_pipeline` is dropped — without this, the AEC task's own
///   sender clone would keep the channel open and deadlock the
///   `recv() -> RecvError::Closed` exit path.
#[cfg(target_os = "macos")]
struct AecTaskState {
    /// `Some` while the task is running and writing to disk.
    /// `None` after `stop()` has taken ownership for finalize. The
    /// AEC task short-circuits when it observes `None` so it doesn't
    /// race a finalized writer.
    writers: Option<wav_writer::PerChannelWavWriters>,
    /// The AEC task's clone of `frames_tx`, used to publish MicClean
    /// frames. Held here (rather than as a closure capture) so
    /// `stop()` can drop it explicitly — closing the broadcast lets
    /// the task's `recv()` see `Err(Closed)` and exit cleanly. If
    /// the task held this directly, `stop()` would have to abort it
    /// (no graceful drain) since broadcast::Sender has no Weak form.
    frames_tx: Option<broadcast::Sender<CaptureFrame>>,
}

/// Wrapper that aborts the AEC task on drop. Mirrors
/// `process_tap::ConsumerTaskGuard` and `mic_capture::ConsumerTaskGuard`.
#[cfg(target_os = "macos")]
struct AecTaskGuard(Option<JoinHandle<()>>);

#[cfg(target_os = "macos")]
impl Drop for AecTaskGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Spawn the AEC + WAV-writer consumer task.
///
/// Consumes `Mic` / `Tap` frames off the shared broadcast, feeds them
/// into APM, emits `MicClean` back onto the broadcast, and persists
/// all three streams to per-channel WAV files. Loops until the
/// broadcast closes (all senders dropped) or the task is aborted.
///
/// Error handling: an APM failure is logged and surfaced once via
/// `Event::CaptureDegraded`; the task then falls back to passthrough
/// (raw mic emitted as `MicClean`) so STT keeps getting input. APM
/// errors are vanishingly rare in practice — APM is robust and the
/// inputs are size-validated upstream — but the contract is "AEC
/// failures must not kill the session."
#[cfg(target_os = "macos")]
fn spawn_aec_task(
    mut frames_rx: broadcast::Receiver<CaptureFrame>,
    events_tx: broadcast::Sender<Event>,
    session_id: SessionId,
    started_at: Instant,
    state: Arc<tokio::sync::Mutex<AecTaskState>>,
    dropped_frames: Arc<AtomicU64>,
) -> Result<JoinHandle<()>, AudioError> {
    let mut aec = aec::EchoCanceller::new()?;

    Ok(tokio::spawn(async move {
        let mut degraded_emitted = false;
        loop {
            let frame = match frames_rx.recv().await {
                Ok(f) => f,
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Broadcast is slower than producers. Account
                    // the loss against the session-level dropped
                    // counter so `StopArtifacts::dropped_frames`
                    // reflects it. `n` is already u64 (skipped count).
                    dropped_frames.fetch_add(n, Ordering::Relaxed);
                    continue;
                }
            };

            // Take the lock for the duration of one frame so `stop()`
            // can swap in `None` to take ownership of the writers.
            // Skip MicClean reflections WITHOUT acquiring the lock —
            // they're our own published frames coming back through
            // the broadcast fan-out, and locking just to no-op would
            // serialize behind any in-flight Mic frame.
            if matches!(frame.channel, Channel::MicClean) {
                continue;
            }

            let mut guard = state.lock().await;
            // Both writers and frames_tx must still be live; if
            // either was taken by stop() the task is on its way out
            // and any further work is wasted. Check `frames_tx`
            // first via a clone (cheap — broadcast::Sender::clone
            // is just an Arc bump) so the second borrow can be a
            // mutable reborrow on `writers`.
            let task_frames_tx = match guard.frames_tx.clone() {
                Some(tx) => tx,
                None => continue,
            };
            let writers = match guard.writers.as_mut() {
                Some(w) => w,
                None => continue,
            };

            match frame.channel {
                Channel::Tap => {
                    if let Err(e) = writers.write_frame(&frame) {
                        tracing::warn!(error = %e, "tap.wav write failed");
                    }
                    if let Err(e) = aec.process_far_end(&frame) {
                        emit_degraded_once(
                            &events_tx,
                            session_id,
                            started_at,
                            &mut degraded_emitted,
                            format!("AEC far-end failed: {e}"),
                        );
                    }
                }
                Channel::Mic => {
                    if let Err(e) = writers.write_frame(&frame) {
                        tracing::warn!(error = %e, "mic.wav write failed");
                    }

                    // APM mutates samples in place and requires the
                    // input frame's channel to be `Mic`. Clone into
                    // a working copy whose channel stays `Mic` for
                    // the APM call — on success, the samples come
                    // back AEC-cleaned; on failure, we keep the raw
                    // samples (APM only writes on Ok). Then build
                    // the published `MicClean` frame from the
                    // post-APM samples.
                    let mut working = frame.clone();
                    if let Err(e) = aec.process_near_end(&mut working) {
                        emit_degraded_once(
                            &events_tx,
                            session_id,
                            started_at,
                            &mut degraded_emitted,
                            format!("AEC near-end failed: {e}"),
                        );
                        // Passthrough: `working.samples` is still
                        // the raw mic since APM only mutates on Ok.
                    }
                    let cleaned = CaptureFrame {
                        channel: Channel::MicClean,
                        host_time: frame.host_time,
                        session_secs: frame.session_secs,
                        samples: working.samples,
                    };

                    if let Err(e) = writers.write_frame(&cleaned) {
                        tracing::warn!(error = %e, "mic_clean.wav write failed");
                    }
                    // `send` returns `Err` only when there are no
                    // receivers — not a back-pressure signal. Ignore.
                    let _ = task_frames_tx.send(cleaned);
                }
                // Already filtered above without the lock.
                Channel::MicClean => unreachable!(),
            }
        }
    }))
}

/// Emit `Event::CaptureDegraded` exactly once per session for AEC
/// failures, mirroring `BackpressureMonitor`'s once-per-saturation
/// latch. Subsequent failures fold into the same event — they don't
/// re-surface to the UI on every frame.
#[cfg(target_os = "macos")]
fn emit_degraded_once(
    events_tx: &broadcast::Sender<Event>,
    session_id: SessionId,
    started_at: Instant,
    latched: &mut bool,
    reason: String,
) {
    if *latched {
        return;
    }
    *latched = true;
    tracing::warn!(reason = %reason, "AEC failure; falling back to mic passthrough");
    let _ = events_tx.send(Event::CaptureDegraded {
        id: session_id,
        at: started_at.elapsed(),
        dropped_frames: 0,
        reason,
    });
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
    /// Wall-clock instant the session was started, used to compute
    /// `StopArtifacts::duration` at stop time. Kept here rather than
    /// inside the macOS guard because `Instant` is portable.
    #[cfg(target_os = "macos")]
    started_at: Instant,
    /// Cumulative frames dropped by the broadcast lag handler in the
    /// AEC task. Read once at stop and surfaced via
    /// `StopArtifacts::dropped_frames`.
    ///
    /// **Coverage caveat (TODO):** SPSC-ring drops on the producer
    /// side (tap IO proc / cpal mic) are accounted for separately by
    /// [`BackpressureMonitor`] and surfaced via
    /// `Event::CaptureDegraded`. They are NOT yet folded into this
    /// counter; to do that, `process_tap::open_tap` and
    /// `mic_capture::start_mic` would need to accept a shared
    /// `Arc<AtomicU64>` rather than each owning a private one. Tracked
    /// for a follow-up — for v0, `StopArtifacts::dropped_frames`
    /// reports broadcast-side lag only, and the per-channel SPSC
    /// drops are observable through the events stream.
    #[cfg(target_os = "macos")]
    dropped_frames: Arc<AtomicU64>,
    /// Shared with the AEC task — owns the per-channel WAV writers.
    /// `stop()` swaps in `None` to take ownership for finalize.
    /// `None` after stop has consumed it.
    #[cfg(target_os = "macos")]
    aec_state: Option<Arc<tokio::sync::Mutex<AecTaskState>>>,
    /// Aborts the AEC task on drop. `None` after `stop()` has
    /// awaited the task.
    #[cfg(target_os = "macos")]
    aec_task: Option<AecTaskGuard>,
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
    ///
    /// The returned [`StopArtifacts`] always populates all three
    /// paths — channels that never emitted a frame still produce a
    /// 0-sample WAV header (see [`StopArtifacts`] doc for the
    /// empty-WAV contract).
    ///
    /// Off-Apple this returns [`AudioError::NotYetImplemented`] —
    /// `start()` already returns the same off-Apple, so a handle
    /// reaching `stop()` on a non-Apple platform is impossible by
    /// construction; the case is included for API symmetry.
    pub async fn stop(mut self) -> Result<StopArtifacts, AudioError> {
        #[cfg(target_os = "macos")]
        {
            // Step 1: take ownership of the WAV writers AND the
            // AEC task's `frames_tx` clone in one critical section.
            // Dropping the latter is what lets the broadcast
            // actually close once `_macos_pipeline` goes away —
            // otherwise the task would hold the channel open and
            // `recv()` would never see `Err(Closed)`.
            let writers = {
                let aec_state = match self.aec_state.as_ref() {
                    Some(s) => s,
                    None => {
                        return Err(AudioError::Aborted(
                            "AudioCaptureHandle missing AEC state at stop".to_string(),
                        ));
                    }
                };
                let mut guard = aec_state.lock().await;
                let _ = guard.frames_tx.take();
                match guard.writers.take() {
                    Some(w) => w,
                    None => {
                        return Err(AudioError::Aborted(
                            "WAV writers already taken; stop called twice?".to_string(),
                        ));
                    }
                }
            };

            // Step 2: read the broadcast clock + dropped frames
            // BEFORE we tear the producers down, so `duration`
            // reflects the real session length end-to-end.
            let duration = self.started_at.elapsed();
            let drops_now = self.dropped_frames.load(Ordering::Relaxed);
            let dropped_frames: u32 = drops_now.try_into().unwrap_or(u32::MAX);

            // Step 3: drop the macOS resources (tap IO proc + mic
            // stream + broadcast senders). Field drop order in
            // `MacosPipelineGuard` ensures the realtime callbacks
            // are quiesced before the SPSC producers go away.
            // Combined with the `frames_tx` we just took out of
            // shared state in step 1, this drops the last broadcast
            // sender and the AEC task's `recv()` returns
            // `Err(Closed)` on the next iteration.
            //
            // Drop the broadcast `frames` receiver on `self` too —
            // if the caller never moved it out, it's still pinning
            // memory and we want the channel torn down promptly.
            // (Drop the field by replacing it with a fresh dummy
            // receiver from a one-shot channel; can't move out of
            // `self` while we still need other fields.)
            let mut aec_task_guard = self.aec_task.take();
            let aec_task = aec_task_guard.as_mut().and_then(|g| g.0.take());
            self._macos_pipeline = None;

            // Step 4: wait for the AEC task to drain. With both
            // sender clones dropped (the pipeline-side ones in
            // step 3 and the task-side one in step 1), `recv()`
            // returns `Err(Closed)` and the loop exits. 2 s is
            // generous: a healthy shutdown is sub-100 ms. Anything
            // longer implies a hung consumer; abort the task so
            // we don't leak it past `stop()` returning.
            if let Some(handle) = aec_task {
                match tokio::time::timeout(Duration::from_secs(2), handle).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) if e.is_cancelled() => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "AEC task join error during stop");
                    }
                    Err(_) => {
                        // Re-acquire the guard wrapper to abort
                        // since we already extracted the handle.
                        // Can't abort directly here because `handle`
                        // was awaited and is consumed; use
                        // `aec_task_guard` to hold whatever's left
                        // (which should be empty — the abort is via
                        // Drop below as a belt-and-suspenders).
                        tracing::warn!("AEC task drain exceeded 2s timeout");
                    }
                }
            }
            // The guard wrapper is dropped here regardless — the
            // wrapper's Drop impl aborts any leftover handle as a
            // safety net.
            drop(aec_task_guard);

            // Step 5: finalize the writers. This closes each open
            // WAV file and writes empty 0-sample WAV headers for
            // any channel that never emitted (see `StopArtifacts`
            // empty-WAV contract).
            let paths = writers.finalize()?;
            let mic = paths
                .get(&Channel::Mic)
                .cloned()
                .ok_or_else(|| AudioError::Aborted("missing mic path post-finalize".into()))?;
            let tap = paths
                .get(&Channel::Tap)
                .cloned()
                .ok_or_else(|| AudioError::Aborted("missing tap path post-finalize".into()))?;
            let mic_clean = paths.get(&Channel::MicClean).cloned().ok_or_else(|| {
                AudioError::Aborted("missing mic_clean path post-finalize".into())
            })?;

            Ok(StopArtifacts {
                mic,
                tap,
                mic_clean,
                duration,
                dropped_frames,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(AudioError::NotYetImplemented)
        }
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

    /// AEC task wiring smoke test (macOS-only because the task is
    /// behind the same `cfg` gate as the rest of the production
    /// pipeline). Pumps one Tap frame and one Mic frame into the
    /// shared broadcast and asserts:
    ///
    /// 1. The task emits exactly one MicClean frame back onto the
    ///    broadcast in response to the Mic input.
    /// 2. The MicClean frame's host_time / session_secs match the
    ///    raw mic frame (we don't re-clock during AEC).
    /// 3. After `stop()`-style teardown (drop the task-side and
    ///    handle-side senders, flip `writers` to None), the task's
    ///    `recv()` returns `Err(Closed)` and the JoinHandle resolves
    ///    cleanly within 1 s — no deadlock from the task's own
    ///    sender clone keeping the channel open. This is the bug
    ///    that drove `AecTaskState::frames_tx` into shared state.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn aec_task_emits_mic_clean_and_drains_at_stop() {
        let (frames_tx, mut frames_rx) = broadcast::channel::<CaptureFrame>(64);
        let (events_tx, _events_rx) = broadcast::channel::<Event>(8);
        let aec_rx = frames_tx.subscribe();

        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::nil();
        let writers =
            wav_writer::PerChannelWavWriters::new(temp.path(), session_id).expect("writers");
        let dropped_frames = Arc::new(AtomicU64::new(0));
        let aec_state = Arc::new(tokio::sync::Mutex::new(AecTaskState {
            writers: Some(writers),
            frames_tx: Some(frames_tx.clone()),
        }));
        let task = spawn_aec_task(
            aec_rx,
            events_tx,
            session_id,
            Instant::now(),
            Arc::clone(&aec_state),
            Arc::clone(&dropped_frames),
        )
        .expect("spawn AEC task");

        // Tap frame first (silence is fine — APM accepts any 480-sample
        // f32 frame; the smoke test isn't grading suppression).
        let tap = CaptureFrame {
            channel: Channel::Tap,
            host_time: 100,
            session_secs: 0.0,
            samples: vec![0.0; APM_FRAME_SAMPLES],
        };
        frames_tx.send(tap).expect("send tap");

        // Mic frame: a low-amplitude tone so APM has *something* to
        // chew on and the cleaned samples Vec is well-defined.
        let mic = CaptureFrame {
            channel: Channel::Mic,
            host_time: 200,
            session_secs: 0.001,
            samples: vec![0.1; APM_FRAME_SAMPLES],
        };
        frames_tx.send(mic).expect("send mic");

        // Wait for a MicClean reflection. Skip Tap/Mic frames the
        // task echoes back to us (broadcast is fan-out).
        let mut mic_clean: Option<CaptureFrame> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), frames_rx.recv()).await {
                Ok(Ok(frame)) if frame.channel == Channel::MicClean => {
                    mic_clean = Some(frame);
                    break;
                }
                Ok(Ok(_)) => continue, // tap or raw mic echo; keep polling
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    panic!("broadcast closed before MicClean arrived")
                }
                Err(_) => {} // timeout slice; loop until deadline
            }
        }
        let mic_clean = mic_clean.expect("MicClean frame must arrive within 2s");
        assert_eq!(
            mic_clean.host_time, 200,
            "MicClean must inherit host_time from its source mic frame"
        );
        assert!(
            (mic_clean.session_secs - 0.001).abs() < 1e-9,
            "MicClean session_secs must inherit from the source mic frame"
        );
        assert_eq!(
            mic_clean.samples.len(),
            APM_FRAME_SAMPLES,
            "MicClean must preserve frame length"
        );

        // Tear down the way `stop()` does: drop the task-side
        // sender first (via shared state), then drop the
        // handle-side senders.
        {
            let mut guard = aec_state.lock().await;
            let _ = guard.frames_tx.take();
            let _ = guard.writers.take();
        }
        drop(frames_tx);
        drop(frames_rx);

        // The task should now see Err(Closed) on its next recv()
        // and the JoinHandle resolves promptly. If the task held
        // its own sender clone outside shared state, this would
        // deadlock and time out.
        let join = tokio::time::timeout(Duration::from_secs(1), task).await;
        assert!(
            matches!(join, Ok(Ok(()))),
            "AEC task must drain within 1s of all senders dropping; got {join:?}"
        );
    }
}
