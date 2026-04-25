//! Microphone capture pipeline (cross-platform via `cpal`).
//!
//! Sibling to [`crate::process_tap`]. The process tap captures the
//! meeting client's audio stream (`Channel::Tap`); this module captures
//! the user's microphone (`Channel::Mic`). Both push into the same
//! `broadcast::Sender<CaptureFrame>` so downstream consumers (week 4
//! STT, the future APM/AEC hop in §6.3) see a unified frame stream
//! differentiated by [`heron_types::Channel`].
//!
//! Pipeline shape mirrors `process_tap::install_io_proc` exactly:
//!
//! 1. cpal opens the default input device at 48 kHz mono f32 and
//!    registers a callback that fires on a realtime audio thread.
//! 2. The callback (RT-safe constraints — no locks, bounded time)
//!    reads `mach_absolute_time`, copies the f32 samples into a fresh
//!    `Vec<f32>`, and pushes a `CaptureFrame { channel: Mic, .. }` onto
//!    an `rtrb::Producer<CaptureFrame>`. Ring full = drop the frame +
//!    bump an atomic counter.
//! 3. A Tokio consumer task drains the rtrb consumer end into a
//!    `broadcast::Sender<CaptureFrame>`, samples ring depth before each
//!    drain, and feeds [`BackpressureMonitor`] so a saturation episode
//!    emits exactly one `Event::CaptureDegraded`.
//!
//! ## host_time provenance — cpal vs Core Audio
//!
//! `cpal::InputCallbackInfo::timestamp().capture` is a
//! [`cpal::StreamInstant`] — opaque seconds-since-stream-origin, not
//! the raw mach tick that `SessionClock::host_to_session_secs` expects.
//! On macOS the underlying source IS `mach_absolute_time` (see cpal's
//! docs on `StreamInstant`), but the public conversion is lossy and
//! the origin floats per stream.
//!
//! For frame-timestamp parity with the tap side we read
//! `mach_absolute_time()` directly inside the callback and use that as
//! `host_time`. Trade-off: a few ns of jitter (the read happens after
//! the audio unit captured the buffer, not at the exact ADC sample
//! instant) in exchange for one shared `SessionClock` axis across both
//! channels. The §9.3 aligner is robust to ms-scale skew, so ns-scale
//! callback-entry jitter is not load-bearing.
//!
//! ## v0 scope
//!
//! - macOS only. The `cpal` dep is gated to `target_os = "macos"` in
//!   `Cargo.toml`; on Linux/Windows [`start_mic`] returns
//!   `Err(AudioError::NotYetImplemented)`.
//! - 48 kHz mono f32 only. If the default input device can't honor
//!   that exact `StreamConfig`, the build fails and the caller treats
//!   it as a soft degradation (tap-only session).
//! - Raw mic audio — no APM / AEC. PR3 wires WebRTC APM with this
//!   module's frames as the near-end input and the tap as the far-end
//!   reference.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use heron_types::{Channel, Event, SessionClock, SessionId};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::backpressure::BackpressureMonitor;
use crate::{AudioError, CaptureFrame};

#[cfg(target_os = "macos")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// SPSC ringbuffer capacity, in `CaptureFrame`s. Sized identically to
/// the tap pipeline (`process_tap::RING_CAPACITY`) so a saturation
/// signal on either side has comparable timing characteristics.
const RING_CAPACITY: usize = 1024;

/// Target capture format for v0. Both the tap and the mic emit mono
/// f32 at 48 kHz — STT (week 4) consumes one channel at this rate, and
/// WebRTC APM (PR3) is configured for the same. Diverging here would
/// force a resampler on the AEC near-end path.
///
/// We request 48 kHz at the device's reported channel count and
/// downmix to mono in the callback — see `mic_callback` for the
/// downmix detail.
const TARGET_SAMPLE_RATE_HZ: u32 = 48_000;

/// Context owned by the cpal callback (realtime thread). Same shape as
/// `process_tap::IoProcCtx`. Heap-allocated once, accessed only from
/// the audio callback (single writer for `producer`/`clock`/`channel`,
/// atomic for `dropped`).
#[cfg(target_os = "macos")]
struct MicCtx {
    producer: Producer<CaptureFrame>,
    dropped: Arc<AtomicU64>,
    clock: SessionClock,
    /// Number of input channels the device delivers per frame. f32
    /// samples in cpal's input buffer are interleaved; `channels == 1`
    /// is the common Apple-Silicon-internal-mic case, but external
    /// USB interfaces routinely deliver stereo and we downmix in the
    /// callback to keep the broadcast contract (mono) intact.
    input_channels: usize,
}

/// Owned wrapper around a live cpal input stream.
///
/// Drop order, matching the tap-side [`process_tap::IoProcHandle`]:
///
/// 1. `_stream` drops first → cpal stops the audio unit, the realtime
///    callback is guaranteed not to fire again. cpal's macOS Stream
///    holds an `Arc<Mutex<StreamInner>>` and tears down the
///    `AudioUnit` synchronously on drop.
/// 2. `_consumer_task` aborts → no more pops from the ring.
/// 3. `_ctx` (the heap-allocated `MicCtx`) drops last → the rtrb
///    `Producer` is freed safely now that no realtime thread can race.
///
/// Rust drops fields in declaration order; the field order below
/// matches the safety invariant.
#[cfg(target_os = "macos")]
pub struct MicHandle {
    _stream: cpal::Stream,
    _consumer_task: ConsumerTaskGuard,
    _ctx: Box<MicCtx>,
}

/// Off-Apple stub. The `cpal` dep is macOS-only in `Cargo.toml`, so
/// the real `MicHandle` doesn't exist on Linux/Windows; this empty
/// type keeps the public type referenceable in cfg-independent code.
#[cfg(not(target_os = "macos"))]
pub struct MicHandle {
    _private: (),
}

/// Wrapper that aborts the consumer task on drop. Without this the
/// task would idle forever once the producer is freed (the rtrb
/// consumer's `pop` would always return `Err(Empty)`).
struct ConsumerTaskGuard(Option<JoinHandle<()>>);

impl Drop for ConsumerTaskGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Open the default input device, configure it for 48 kHz mono f32,
/// install a realtime callback that pushes frames onto an rtrb SPSC
/// ringbuffer, and spawn a Tokio consumer task that drains the ring
/// into `frames_tx` while feeding `events_tx` through a
/// [`BackpressureMonitor`].
///
/// MUST be called from inside a Tokio runtime — the consumer task is
/// `tokio::spawn`ed on the current runtime.
///
/// On non-Apple platforms returns `Err(AudioError::NotYetImplemented)`.
///
/// On macOS, common failure modes the caller should expect:
/// - No default input device (rare — only happens on a headless box
///   with no audio hardware). Returned as `AudioError::Aborted`.
/// - TCC microphone permission denied. cpal surfaces this as a
///   `BuildStreamError`; we map to `AudioError::PermissionDenied`.
/// - Device doesn't support 48 kHz mono f32. Surfaced as
///   `AudioError::Aborted` with the cpal error message attached.
///
/// All three are non-fatal in the orchestrator's eyes — see
/// `lib.rs::AudioCapture::start` for the "mic failure does not fail
/// the session" policy.
#[cfg(target_os = "macos")]
pub fn start_mic(
    frames_tx: broadcast::Sender<CaptureFrame>,
    events_tx: broadcast::Sender<Event>,
    session_id: SessionId,
    clock: SessionClock,
) -> Result<MicHandle, AudioError> {
    // 1) Verify a Tokio runtime is available BEFORE building the
    //    stream — `tokio::spawn` panics otherwise, and we don't want
    //    to be holding a registered cpal Stream when that happens.
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(AudioError::Aborted(
            "start_mic requires a Tokio runtime context".to_string(),
        ));
    }

    // 2) Resolve the default input device. cpal's `default_host` is
    //    Core Audio on macOS; `default_input_device()` returns the
    //    user's currently selected mic in System Settings.
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| AudioError::Aborted("no default input device available".to_string()))?;

    // 3) Pick a config. We probe `default_input_config()` for the
    //    actual channel count the device delivers — internal Apple
    //    Silicon mic is mono, but external USB interfaces routinely
    //    deliver stereo at 48 kHz. We downmix to mono in the
    //    callback so the broadcast contract (mono CaptureFrame) is
    //    preserved without forcing cpal to do channel reduction.
    let default_config = device
        .default_input_config()
        .map_err(|e| AudioError::Aborted(format!("default_input_config failed: {e}")))?;
    let input_channels = default_config.channels() as usize;
    if input_channels == 0 {
        return Err(AudioError::Aborted(
            "default input config reports zero channels".to_string(),
        ));
    }

    // Build an explicit StreamConfig at our target rate. We keep the
    // device's reported channel count (so cpal doesn't have to remix)
    // and downmix in the callback.
    let stream_config = cpal::StreamConfig {
        channels: default_config.channels(),
        sample_rate: cpal::SampleRate(TARGET_SAMPLE_RATE_HZ),
        buffer_size: cpal::BufferSize::Default,
    };

    // 4) Build the SPSC ring + heap-allocated callback context.
    let (producer, consumer) = RingBuffer::<CaptureFrame>::new(RING_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let mut ctx = Box::new(MicCtx {
        producer,
        dropped: Arc::clone(&dropped),
        clock,
        input_channels,
    });

    // 5) Spawn the consumer task BEFORE building the stream so it's
    //    ready to drain on the first callback. We checked above that
    //    a runtime is present, so `tokio::spawn` won't panic.
    let started_at = std::time::Instant::now();
    let consumer_task = spawn_consumer_task(
        consumer,
        Arc::clone(&dropped),
        frames_tx,
        events_tx,
        session_id,
        started_at,
    );

    // 6) Build the input stream. cpal's `build_input_stream` takes
    //    an owning closure with `Send + 'static` bounds; we reach
    //    into `ctx` via a raw pointer so the Box is the single owner
    //    of the heap allocation (mirrors the `IoProcCtx` raw-pointer
    //    pattern in `process_tap.rs`).
    //
    //    Stash the pointer as `usize` to skirt the `*mut T: !Send`
    //    auto trait — the closure has to be `Send` for cpal to ferry
    //    it onto the realtime thread, but raw pointers aren't `Send`
    //    even when the referent is.
    //
    //    SAFETY (the deref inside the closure body below): the
    //    pointer's referent (`MicCtx`) is owned by the `MicHandle`
    //    returned from this function. The handle's drop order
    //    (stream → ctx) guarantees the stream is fully torn down —
    //    and therefore no callback is in flight — before the Box is
    //    freed. cpal's macOS `Stream` drops `StreamInner`, which
    //    drops `AudioUnit`, whose `Drop` impl calls
    //    `AudioOutputUnitStop` + `AudioUnitUninitialize` synchronously
    //    (see `coreaudio-rs::audio_unit::AudioUnit::drop`); after
    //    `AudioOutputUnitStop` returns, no new realtime callbacks
    //    fire. cpal does not clone the inner `Arc<Mutex<StreamInner>>`
    //    in 0.15, so our `Stream` drop is the last reference.
    let ctx_addr: usize = ctx.as_mut() as *mut MicCtx as usize;

    let stream = device
        .build_input_stream(
            &stream_config,
            move |samples: &[f32], _info: &cpal::InputCallbackInfo| {
                // SAFETY: see comment above the `ctx_addr` binding.
                // The callback runs on cpal's macOS realtime thread
                // (single-writer per stream — cpal's coreaudio host
                // fires AURenderCallback from one thread); `ctx` is
                // single-writer for the producer and clock fields.
                let ctx = unsafe { &mut *(ctx_addr as *mut MicCtx) };
                mic_callback(ctx, samples);
            },
            move |err| {
                // cpal's error callback runs off the realtime thread.
                // Logging is fine here. We deliberately DON'T tear
                // the stream down — cpal keeps trying on transient
                // errors, and a permanent failure will eventually
                // surface as a stalled callback that the
                // BackpressureMonitor catches as zero-fill.
                tracing::warn!(error = %err, "cpal input stream error");
            },
            None,
        )
        .map_err(|e| {
            // cpal returns `BuildStreamError::DeviceNotAvailable` for
            // TCC denial on macOS. Surface that as PermissionDenied
            // so the onboarding flow (week 6) can prompt; everything
            // else is a generic Aborted.
            consumer_task.abort();
            map_build_stream_error(e)
        })?;

    // 7) Start the stream. macOS auto-starts on build, but per cpal's
    //    docs not all platforms do; calling `play` is idempotent.
    stream.play().map_err(|e| {
        consumer_task.abort();
        AudioError::Aborted(format!("cpal stream.play() failed: {e}"))
    })?;

    Ok(MicHandle {
        _stream: stream,
        _consumer_task: ConsumerTaskGuard(Some(consumer_task)),
        _ctx: ctx,
    })
}

/// Off-Apple variant — the `cpal` dep is gated to macOS in our
/// `Cargo.toml`, so the real implementation only compiles on Apple
/// platforms. Linux/Windows builds exist for `cargo check` only and
/// shouldn't have a way to silently start a half-mic session.
#[cfg(not(target_os = "macos"))]
pub fn start_mic(
    _frames_tx: broadcast::Sender<CaptureFrame>,
    _events_tx: broadcast::Sender<Event>,
    _session_id: SessionId,
    _clock: SessionClock,
) -> Result<MicHandle, AudioError> {
    Err(AudioError::NotYetImplemented)
}

/// Realtime-thread callback. Runs on a Core Audio realtime thread
/// (cpal's macOS host wraps `AURenderCallback`). Constraints mirror
/// the [`process_tap::io_proc`] doc:
/// - No locks, no syscalls.
/// - Bounded execution: one f32 buffer copy + one rtrb push.
///
/// **Allocation trade-off:** identical to the tap path — `Vec::to_vec`
/// allocates per callback (~ns on modern Apple Silicon), well under
/// the ~10 ms callback budget. A pool would be the next optimization
/// if §7.4 surfaces alloc-driven jitter.
#[cfg(target_os = "macos")]
fn mic_callback(ctx: &mut MicCtx, interleaved: &[f32]) {
    if interleaved.is_empty() {
        return;
    }

    // Read mach_absolute_time directly so `host_time` is comparable
    // with the tap-side IO proc's `now.host_time`. See the module
    // docs (§"host_time provenance") for the trade-off rationale.
    let host_time = read_mach_absolute_time();
    let session_secs = ctx.clock.host_to_session_secs(host_time);

    // Downmix to mono if the device delivers a multi-channel buffer.
    // `interleaved.len()` is `frames * channels`. For channels == 1
    // we just clone — the explicit branch avoids the per-sample
    // division in the hot path. For channels > 1 we average across
    // channels, which is the standard "naive but correct" downmix.
    let frame_count = interleaved.len() / ctx.input_channels;
    let samples: Vec<f32> = if ctx.input_channels == 1 {
        interleaved.to_vec()
    } else {
        let scale = 1.0_f32 / ctx.input_channels as f32;
        let mut mono = Vec::with_capacity(frame_count);
        for frame in interleaved.chunks_exact(ctx.input_channels) {
            let sum: f32 = frame.iter().sum();
            mono.push(sum * scale);
        }
        mono
    };

    let frame = CaptureFrame {
        channel: Channel::Mic,
        host_time,
        session_secs,
        samples,
    };

    if let Err(PushError::Full(_)) = ctx.producer.push(frame) {
        ctx.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Read `mach_absolute_time()` from the realtime callback. Same trick
/// as `heron_types::SessionClock`'s anchor read — the extern is
/// declared locally to avoid a public re-export from heron-types.
#[cfg(target_os = "macos")]
#[inline]
fn read_mach_absolute_time() -> u64 {
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
    }
    // SAFETY: documented mach kernel API, takes no args, returns a
    // raw tick count. Available on every supported macOS version.
    unsafe { mach_absolute_time() }
}

/// Spawn the consumer task that drains the SPSC ring into the
/// broadcast channel and emits `Event::CaptureDegraded` on saturation.
///
/// Identical structure to `process_tap::spawn_consumer_task` — the
/// 1 ms poll cadence + sample-depth-before-drain ordering is the
/// invariant landed in PR #57. If the tap side moves to
/// `tokio::sync::Notify`, this should follow.
fn spawn_consumer_task(
    mut consumer: Consumer<CaptureFrame>,
    dropped: Arc<AtomicU64>,
    frames_tx: broadcast::Sender<CaptureFrame>,
    events_tx: broadcast::Sender<Event>,
    session_id: SessionId,
    started_at: std::time::Instant,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut monitor = BackpressureMonitor::new(session_id, RING_CAPACITY, events_tx);
        loop {
            // Sample ring depth BEFORE draining — same invariant as
            // process_tap.rs's consumer: post-drain depth is always
            // ~0 (the drain runs until empty), so a saturation check
            // against post-drain state would never fire.
            let depth = consumer.slots();

            while let Ok(frame) = consumer.pop() {
                // `broadcast::Sender::send` returns `Err` only when
                // there are no receivers; that's not a back-pressure
                // signal, the broadcast channel itself is bounded by
                // its initial capacity. Swallow.
                let _ = frames_tx.send(frame);
            }

            let drops_now = dropped.load(Ordering::Relaxed);
            // Cast u64 → u32 saturating. `Event::CaptureDegraded`'s
            // payload is u32; a session that drops > u32::MAX frames
            // is so degraded the exact count doesn't matter.
            let drops_u32: u32 = drops_now.try_into().unwrap_or(u32::MAX);
            monitor.observe(depth, drops_u32, started_at.elapsed());

            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
}

/// Map cpal's `BuildStreamError` to our `AudioError`. macOS-only
/// because `BuildStreamError` itself is only in scope when the cpal
/// dep is compiled in.
///
/// Note on the `DeviceNotAvailable` mapping: cpal 0.15 routes most
/// `coreaudio::Error` values through `DeviceNotAvailable` (see
/// `cpal::host::coreaudio::From<coreaudio::Error> for BuildStreamError`),
/// so this variant covers both TCC mic denial AND a genuinely
/// disconnected device. We surface as `PermissionDenied` because
/// that's the actionable case for the onboarding flow (week 6's TCC
/// prompt re-trigger); a true disconnect retries on the next
/// `AudioCapture::start`.
#[cfg(target_os = "macos")]
fn map_build_stream_error(err: cpal::BuildStreamError) -> AudioError {
    match err {
        cpal::BuildStreamError::DeviceNotAvailable => AudioError::PermissionDenied(
            "default input device not available; \
             check Privacy & Security → Microphone (TCC), \
             then re-attach the input device if removed"
                .to_string(),
        ),
        other => AudioError::Aborted(format!("cpal build_input_stream failed: {other}")),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Pure-Rust unit test mirroring
    /// `process_tap::tests::full_ring_increments_drop_counter`. Pushing
    /// onto a full SPSC ring increments the drop counter — validates
    /// the realtime back-pressure accounting for the mic side without
    /// touching cpal or any audio device.
    #[test]
    fn full_ring_increments_drop_counter() {
        let (mut producer, _consumer) = RingBuffer::<CaptureFrame>::new(2);
        let dropped = Arc::new(AtomicU64::new(0));

        let make_frame = || CaptureFrame {
            channel: Channel::Mic,
            host_time: 0,
            session_secs: 0.0,
            samples: vec![],
        };

        for _ in 0..2 {
            producer.push(make_frame()).expect("push under capacity");
        }

        for _ in 0..5 {
            if let Err(PushError::Full(_)) = producer.push(make_frame()) {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        assert_eq!(
            dropped.load(Ordering::Relaxed),
            5,
            "five over-capacity pushes should bump the counter five times"
        );
    }

    /// Saturation observed from a full ring fires exactly one
    /// `Event::CaptureDegraded` via `BackpressureMonitor` — same shape
    /// as the tap-side test, locked down so a future patch that
    /// rewires the consumer task can't silently skip the saturation
    /// signal.
    #[test]
    fn full_ring_drives_capture_degraded_event() {
        let (events_tx, mut events_rx) = broadcast::channel::<Event>(8);
        let mut monitor = BackpressureMonitor::new(SessionId::nil(), RING_CAPACITY, events_tx);

        // 95 % full → above SATURATION_THRESHOLD (90 %).
        let in_flight = (RING_CAPACITY as f32 * 0.95) as usize;
        assert!(monitor.observe(in_flight, 11, Duration::from_secs(1)));

        let evt = events_rx.try_recv().expect("event delivered");
        match evt {
            Event::CaptureDegraded { dropped_frames, .. } => {
                assert_eq!(dropped_frames, 11);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Off-Apple `start_mic` returns `NotYetImplemented` — same
    /// behavior as `AudioCapture::start` off-Apple, consistent so the
    /// orchestrator can treat both paths uniformly.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn start_mic_returns_not_yet_implemented_off_apple() {
        let (frames_tx, _frames_rx) = broadcast::channel(8);
        let (events_tx, _events_rx) = broadcast::channel(8);
        let result = start_mic(frames_tx, events_tx, SessionId::nil(), SessionClock::new());
        assert!(matches!(result, Err(AudioError::NotYetImplemented)));
    }
}
