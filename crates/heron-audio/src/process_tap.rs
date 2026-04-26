//! macOS Core Audio process tap pipeline.
//!
//! Per [`docs/archives/plan.md`](../../../docs/archives/plan.md) §6.2 and
//! [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §6, the
//! tap path looks roughly like:
//!
//! 1. Resolve the meeting client's pid via `NSRunningApplication`.
//! 2. Translate that pid into a Core Audio `AudioObjectID` (a
//!    `ca::Process`) and build a `CATapDescription` that targets only
//!    that process.
//! 3. Create the tap (`AudioHardwareCreateProcessTap`) — the `Tap`
//!    object can't be opened directly, so we wrap it inside a private
//!    aggregate device whose only sub-tap is the new tap.
//! 4. Install an IO proc on the aggregate device, start it, and pump
//!    each callback's f32 samples into the broadcast channel that
//!    `AudioCaptureHandle::frames` exposes.
//!
//! Step 4's realtime path uses an `rtrb` SPSC ringbuffer:
//! - The IO proc (Core Audio realtime thread) copies the input
//!   `AudioBufferList`'s f32 samples and pushes a `CaptureFrame` onto
//!   the producer end. Ring full = drop the frame and bump an atomic
//!   counter (the realtime thread cannot block).
//! - A consumer task on the Tokio runtime drains the ring and forwards
//!   each frame to a `broadcast::Sender<CaptureFrame>`. The same task
//!   polls the dropped-frames counter and feeds a
//!   [`BackpressureMonitor`] so a saturation episode emits exactly one
//!   `Event::CaptureDegraded` (§7.4).
//!
//! All public items in this file are macOS-only by virtue of
//! `#[cfg(target_os = "macos")]` on the `mod process_tap;` line in
//! `lib.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use heron_types::{Channel, Event, SessionClock, SessionId};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use cidre::{
    arc, cat, cf, core_audio as ca, core_audio::aggregate_device_keys as agg_keys,
    core_audio::hardware::StartedDevice, core_audio::sub_device_keys as sub_keys, ns, os, sys,
};

use crate::backpressure::BackpressureMonitor;
use crate::{AudioError, CaptureFrame};

/// Capacity of the realtime → consumer SPSC ringbuffer, in
/// `CaptureFrame`s. At 48 kHz with 480-sample frames each callback is
/// ~10 ms of audio; sized so the consumer has ~10 s of slack before
/// the producer would have to drop. Picked at `SATURATION_THRESHOLD`
/// (90 %) headroom over a typical 1 s burst — see `BackpressureMonitor`.
const RING_CAPACITY: usize = 1024;

/// Owned wrapper around a Core Audio process tap.
///
/// Drop releases the underlying tap via `AudioHardwareDestroyProcessTap`
/// (cidre's `TapGuard` handles the actual `extern "C"` call).
pub struct TapHandle {
    /// The tap object. Holding it alive keeps the process tap registered
    /// with the HAL; dropping it tears the tap down.
    tap: ca::TapGuard,
}

impl TapHandle {
    /// The tap UID (`kAudioTapPropertyUID`), formatted as a Core
    /// Foundation string. Used as the sub-tap UID when wiring an
    /// aggregate device on top of the tap.
    pub fn uid(&self) -> Result<arc::R<cf::String>, AudioError> {
        self.tap
            .uid()
            .map_err(|e| AudioError::Aborted(format!("tap.uid() failed: {e:?}")))
    }
}

/// Owned wrapper around the aggregate device that exposes the tap
/// as a readable input stream. Drop releases the aggregate device
/// (cidre's `AggregateDevice` handles `AudioHardwareDestroyAggregateDevice`).
pub struct AggregateHandle {
    pub device: ca::AggregateDevice,
}

/// Context passed to the realtime IO proc.
///
/// **All fields here are touched on a Core Audio realtime thread**, so
/// this struct is heap-allocated once at install time and the proc
/// only ever reads/writes through a stable `*mut` to it. The fields:
///
/// - `producer`: the SPSC producer end of the realtime → consumer
///   ringbuffer. `Producer::push` is wait-free.
/// - `dropped`: atomic counter bumped each time a `push` fails (ring
///   full). The Tokio consumer reads it to drive
///   [`BackpressureMonitor`].
/// - `clock`: snapshot of the session clock so we can convert
///   `host_time` → `session_secs` without locking.
/// - `channel`: always `Channel::Tap` for the process-tap path; kept
///   as a field so that re-using this struct for the mic input later
///   is a one-line change.
///
/// SAFETY: this struct is only accessed by the Core Audio IO proc
/// thread (single writer) for the producer + clock fields, and by the
/// consumer task for `dropped` (atomic). No data race.
struct IoProcCtx {
    producer: Producer<CaptureFrame>,
    dropped: Arc<AtomicU64>,
    clock: SessionClock,
    channel: Channel,
}

/// Handle returned by [`install_io_proc`]. While it's alive, the IO
/// proc keeps firing into the broadcast channel passed at install
/// time. Drop stops the device, aborts the consumer task, and frees
/// the realtime context.
///
/// Drop order matters:
/// 1. `_started_device` drops first → `AudioDeviceStop` runs, the IO
///    proc thread is guaranteed not to fire again.
/// 2. `_consumer_task` is aborted → no more pops from the ring.
/// 3. `_ctx` (the heap-allocated `IoProcCtx`) drops last → the
///    `Producer` is freed safely now that no realtime thread can race.
///
/// Rust drops fields in declaration order; the field order below
/// matches the safety invariant.
pub struct IoProcHandle {
    _started_device: StartedDevice<ca::AggregateDevice>,
    _consumer_task: ConsumerTaskGuard,
    /// Heap-allocated context the IO proc holds a raw pointer into.
    /// MUST drop after `_started_device` so the realtime thread is
    /// definitely quiescent before the `Producer` is freed.
    _ctx: Box<IoProcCtx>,
}

/// Wrapper that aborts the consumer task on drop. Without this, a
/// dropped `IoProcHandle` would leak the consumer task (it would idle
/// forever once the producer is freed).
struct ConsumerTaskGuard(Option<JoinHandle<()>>);

impl Drop for ConsumerTaskGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Resolve the unix pid of the running app whose bundle identifier
/// matches `bundle_id`.
///
/// Returns `Err(AudioError::ProcessNotFound)` if no running app
/// matches; if multiple match (rare — e.g. two Zoom helpers), picks
/// the first non-zero pid. The caller is expected to pass the
/// top-level meeting-client bundle id (e.g. `"us.zoom.xos"`); if
/// you pass the helper bundle (`"us.zoom.xos.ZoomClips"`) you'll get
/// the helper's tap, which is probably not what you want.
pub fn find_pid_by_bundle_id(bundle_id: &str) -> Result<i32, AudioError> {
    // Build an ns::String around the &str, then call
    // NSRunningApplication runningApplicationsWithBundleIdentifier:
    let bundle_ns = ns::String::with_str(bundle_id);
    let apps = ns::RunningApp::with_bundle_id(&bundle_ns);

    // Pick the first non-zero pid. Pid 0 is the "no associated
    // process" sentinel — observed when an app is listed but already
    // terminated mid-poll.
    for app in apps.iter() {
        let pid: sys::Pid = app.pid();
        if pid > 0 {
            return Ok(pid);
        }
    }

    Err(AudioError::ProcessNotFound {
        bundle_id: bundle_id.to_string(),
    })
}

/// Build a private `CATapDescription` targeting exactly `pid` and
/// call `AudioHardwareCreateProcessTap`. The returned [`TapHandle`]
/// owns the tap; drop it to release.
///
/// Wiring matches the §6.2 spec:
/// - `processes = [pid]`, `excludes_processes = false` (i.e. the tap
///   captures *only* this pid)
/// - `private = true` so the tap UID is not visible to other
///   processes on the system
/// - `mute_behavior = Unmuted` so the user still hears the meeting
///   client's audio normally
/// - mono mixdown — STT only ever gets one channel out of the tap,
///   so a stereo mixdown wastes ringbuffer space (week 3, §7.2)
pub fn create_process_tap(pid: i32) -> Result<TapHandle, AudioError> {
    // 1) PID -> Core Audio Process AudioObjectID. `Process::with_pid`
    //    returns `Obj::UNKNOWN` for an unknown pid rather than an
    //    error, so check that before building the description.
    let process = ca::Process::with_pid(pid)
        .map_err(|e| AudioError::Aborted(format!("Process::with_pid({pid}) failed: {e:?}")))?;
    if process.0 == ca::Obj::UNKNOWN {
        return Err(AudioError::ProcessNotFound {
            bundle_id: format!("pid {pid} (no Core Audio process)"),
        });
    }

    // 2) Build the include-list as ns::Array<ns::Number> of
    //    AudioObjectIDs.
    let process_obj_id_num = ns::Number::with_u32(process.0.0);
    let processes = ns::Array::from_slice(&[process_obj_id_num.as_ref()]);

    // 3) `with_mono_mixdown_of_processes` is the Apple-supplied
    //    "init only the listed processes, mono" constructor — exactly
    //    what we want. Set private + unmuted for clarity even though
    //    those are the defaults.
    let mut desc = ca::TapDesc::with_mono_mixdown_of_processes(&processes);
    desc.set_private(true);
    desc.set_mute_behavior(ca::TapMuteBehavior::Unmuted);

    // 4) Create the tap. cidre returns a TapGuard that calls
    //    AudioHardwareDestroyProcessTap on drop.
    let tap = desc.create_process_tap().map_err(|status| {
        // Common failure mode: TCC denied (kAudioHardwareIllegalOperationError
        // before the system audio recording grant lands). Surface as
        // PermissionDenied so the onboarding flow can prompt.
        AudioError::PermissionDenied(format!(
            "AudioHardwareCreateProcessTap failed: {status:?}; \
                 likely TCC system-audio-recording denied"
        ))
    })?;

    Ok(TapHandle { tap })
}

/// Build a private aggregate device whose only sub-tap is `tap_uid`,
/// returning the aggregate device wrapper. The aggregate device's
/// streams expose the tapped audio as if it were a regular input
/// device, so we can install an IO proc on it.
///
/// Mirrors the layout in `cidre`'s `examples/core-audio-record` —
/// private + auto-start + a single tap entry, no real sub-devices.
/// The default-output device UID is set as `main_sub_device` only
/// to give the aggregate a clock reference; we don't actually mix
/// or render through the speakers.
pub fn build_aggregate_device(tap_uid: &cf::String) -> Result<AggregateHandle, AudioError> {
    // Need an output device UID purely to satisfy the aggregate's
    // clock-reference field. We don't drive output through it.
    let output_device = ca::System::default_output_device()
        .map_err(|e| AudioError::Aborted(format!("default_output_device failed: {e:?}")))?;
    let output_uid = output_device
        .uid()
        .map_err(|e| AudioError::Aborted(format!("default_output uid failed: {e:?}")))?;

    let sub_tap = cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[tap_uid.as_type_ref()]);

    let agg_uuid = cf::Uuid::new().to_cf_string();
    let dict = cf::DictionaryOf::with_keys_values(
        &[
            agg_keys::is_private(),
            agg_keys::is_stacked(),
            agg_keys::tap_auto_start(),
            agg_keys::name(),
            agg_keys::main_sub_device(),
            agg_keys::uid(),
            agg_keys::tap_list(),
        ],
        &[
            cf::Boolean::value_true().as_type_ref(),
            cf::Boolean::value_false(),
            cf::Boolean::value_true(),
            cf::str!(c"heron-tap-aggregate"),
            &output_uid,
            &agg_uuid,
            &cf::ArrayOf::from_slice(&[sub_tap.as_ref()]),
        ],
    );

    let device = ca::AggregateDevice::with_desc(&dict)
        .map_err(|e| AudioError::Aborted(format!("AggregateDevice::with_desc failed: {e:?}")))?;

    Ok(AggregateHandle { device })
}

/// Realtime IO proc — runs on a Core Audio kernel-RT thread.
///
/// Constraints (per Apple's [Core Audio realtime thread guidelines]):
/// - No locks, no syscalls (modulo the allocator).
/// - Bounded execution time; this proc copies one buffer of f32
///   samples + pushes onto a wait-free SPSC, no more.
///
/// **Allocation trade-off:** `Vec::with_capacity(n)` is technically a
/// realtime hazard. For our case (one mono `n = 480` Vec per 10 ms,
/// modern macOS allocator), measured ~hundreds of ns per alloc — well
/// under the ~10 ms callback budget. A pre-allocated frame pool would
/// avoid even this; v0 ships with the simpler version. Revisit in §7
/// if the §7.4 60-min stress test surfaces alloc-driven jitter.
///
/// [Core Audio realtime thread guidelines]: https://developer.apple.com/documentation/coreaudio/audiodeviceioproc
extern "C" fn io_proc(
    _device: ca::Device,
    now: &cat::AudioTimeStamp,
    input_data: &cat::AudioBufList<1>,
    _input_time: &cat::AudioTimeStamp,
    _output_data: &mut cat::AudioBufList<1>,
    _output_time: &cat::AudioTimeStamp,
    ctx: Option<&mut IoProcCtx>,
) -> os::Status {
    let ctx = match ctx {
        Some(c) => c,
        // Should never happen — install_io_proc always passes Some.
        // If it does, returning OK keeps Core Audio happy without
        // touching uninitialized state.
        None => return os::Status::default(),
    };

    // Convert the Core Audio host time into seconds since session
    // start. `host_to_session_secs` is a pure arithmetic op on
    // `SessionClock` — no syscall, RT-safe.
    let host_time = now.host_time;
    let session_secs = ctx.clock.host_to_session_secs(host_time);

    // SAFETY: `input_data.buffers[0]` is a Core Audio-managed
    // `AudioBuffer` whose `data` pointer is valid for `data_bytes_size`
    // bytes for the duration of this callback. The tap is configured
    // for mono f32 (`with_mono_mixdown_of_processes`), so the byte
    // count is exactly `sample_count * sizeof::<f32>()`.
    let buf = &input_data.buffers[0];
    let sample_count = (buf.data_bytes_size as usize) / std::mem::size_of::<f32>();
    if sample_count == 0 || buf.data.is_null() {
        return os::Status::default();
    }
    let samples_slice = unsafe { std::slice::from_raw_parts(buf.data as *const f32, sample_count) };
    let samples = samples_slice.to_vec();

    let frame = CaptureFrame {
        channel: ctx.channel,
        host_time,
        session_secs,
        samples,
    };

    // Wait-free push. If the ring is full, the consumer is too slow
    // (or the broadcast channel back-pressured the consumer); drop
    // the frame and bump the counter so the consumer can fire a
    // CaptureDegraded event the next time it polls.
    if let Err(PushError::Full(_)) = ctx.producer.push(frame) {
        ctx.dropped.fetch_add(1, Ordering::Relaxed);
    }

    os::Status::default()
}

/// Spawn the consumer task that drains the SPSC ring into the
/// broadcast channel and emits `Event::CaptureDegraded` on saturation.
///
/// Runs on the current Tokio runtime (this is called from
/// `AudioCapture::start`, which is `async fn`, so a runtime is always
/// available). Polls every 1 ms — coarse but acceptable for v0; a
/// future optimization is to use `tokio::sync::Notify` to wake the
/// task on push (week 3 §7.4 may revisit).
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
            // Sample ring depth BEFORE draining: this is the backlog
            // that accumulated while the consumer was asleep, which
            // is precisely the saturation signal we care about.
            // Sampling AFTER drain would observe ~0 (the drain runs
            // until empty), defeating the saturation check entirely.
            // `Consumer::slots` reports the current readable count.
            let depth = consumer.slots();

            // Drain all currently-available frames before sleeping.
            // `pop()` is wait-free; `Err(_)` means empty.
            while let Ok(frame) = consumer.pop() {
                // `broadcast::Sender::send` returns Err only when no
                // receivers exist — that's not a backpressure signal,
                // it just means nobody's listening yet. Swallow.
                let _ = frames_tx.send(frame);
            }

            let drops_now = dropped.load(Ordering::Relaxed);
            // Cast u64 → u32 saturating: a long-lived session could
            // in principle accumulate more than u32::MAX drops; the
            // `Event::CaptureDegraded` payload is u32 per heron-types.
            let drops_u32: u32 = drops_now.try_into().unwrap_or(u32::MAX);
            // `BackpressureMonitor::observe` only emits an event when
            // crossing `SATURATION_THRESHOLD`; cheap on the steady-state
            // path (one float compare + branch).
            monitor.observe(depth, drops_u32, started_at.elapsed());

            // 1 ms tick. Audio frames arrive every ~10 ms, so we drain
            // ~10 frames per wake on a healthy session. Replace with
            // `tokio::sync::Notify` if profiling shows the polling
            // cost matters (week 3 §7.4 may revisit).
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
}

/// Install an IO proc on `aggregate` that forwards each callback's
/// PCM data to `frames_tx` as a [`CaptureFrame`], starts the device,
/// and spawns a Tokio consumer task that drains the SPSC ring + emits
/// `Event::CaptureDegraded` on saturation (§7.4).
///
/// MUST be called from inside a Tokio runtime (the consumer task is
/// `tokio::spawn`ed on the current runtime). Returns an error if no
/// runtime is available.
///
/// On success, the returned [`IoProcHandle`] owns:
/// - the cidre `StartedDevice` (drops via `AudioDeviceStop`),
/// - the consumer task's `JoinHandle` (aborts on drop),
/// - the heap-allocated realtime context (drops *after* the device is
///   stopped, so the IO proc thread is quiescent before the
///   `Producer` is freed).
pub fn install_io_proc(
    aggregate: AggregateHandle,
    frames_tx: broadcast::Sender<CaptureFrame>,
    events_tx: broadcast::Sender<Event>,
    session_id: SessionId,
    clock: SessionClock,
) -> Result<IoProcHandle, AudioError> {
    // 1) Verify a Tokio runtime is available BEFORE registering the
    //    IO proc — `tokio::spawn` panics otherwise, and we don't want
    //    to be holding a registered (but un-cleanupable) proc id at
    //    that point. cidre 0.15 doesn't expose
    //    `AudioDeviceDestroyIOProcID`; on failure we rely on
    //    `AggregateDevice` Drop to tear down the proc registration.
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(AudioError::Aborted(
            "install_io_proc requires a Tokio runtime context".to_string(),
        ));
    }

    // 2) Build the SPSC ringbuffer. RING_CAPACITY frames of headroom
    //    (~10 s at 480 samples/frame), large enough to absorb a 1 s
    //    consumer hiccup without dropping.
    let (producer, consumer) = RingBuffer::<CaptureFrame>::new(RING_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));

    // 3) Heap-allocate the IO proc context. Core Audio gets a raw
    //    pointer into this; the box must outlive the started device.
    let mut ctx = Box::new(IoProcCtx {
        producer,
        dropped: Arc::clone(&dropped),
        clock,
        channel: Channel::Tap,
    });

    // 4) Register the IO proc. cidre's `create_io_proc_id` transmutes
    //    the typed `&mut T` into a `*mut c_void` and stores it; the
    //    box guarantees the pointer stays valid until we drop the
    //    handle.
    let proc_id = aggregate
        .device
        .create_io_proc_id(io_proc, Some(ctx.as_mut()))
        .map_err(|e| AudioError::Aborted(format!("AudioDeviceCreateIOProcID failed: {e:?}")))?;

    // 5) Spawn the consumer task BEFORE starting the device so it's
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

    // 6) Start the device. AudioDeviceStart with a proc_id activates
    //    the realtime callback. From this point on the IO proc thread
    //    can fire at any time.
    let started = ca::device_start(aggregate.device, Some(proc_id)).map_err(|e| {
        // If start failed, abort the consumer (RAII guards take care
        // of the producer/ctx on drop). The proc id is technically
        // leaked here — see the §1 note above; the AggregateDevice
        // drop unregisters it.
        consumer_task.abort();
        AudioError::Aborted(format!("AudioDeviceStart failed: {e:?}"))
    })?;

    Ok(IoProcHandle {
        _started_device: started,
        _consumer_task: ConsumerTaskGuard(Some(consumer_task)),
        _ctx: ctx,
    })
}

/// Top-level convenience: bundle id -> (TapHandle, AggregateHandle,
/// IoProcHandle). The three handles are returned separately so the
/// caller (here, [`crate::AudioCapture::start`]) can compose them
/// into the public [`crate::AudioCaptureHandle`].
pub fn open_tap(
    bundle_id: &str,
    frames_tx: broadcast::Sender<CaptureFrame>,
    events_tx: broadcast::Sender<Event>,
    session_id: SessionId,
    clock: SessionClock,
) -> Result<TapPipeline, AudioError> {
    let pid = find_pid_by_bundle_id(bundle_id)?;
    let tap = create_process_tap(pid)?;
    let tap_uid = tap.uid()?;
    let aggregate = build_aggregate_device(&tap_uid)?;
    let io_proc = install_io_proc(aggregate, frames_tx, events_tx, session_id, clock)?;
    // Field order here matches the struct definition: `_io_proc`
    // first so it drops first (stops the device), then `_tap`.
    Ok(TapPipeline {
        _io_proc: io_proc,
        _tap: tap,
    })
}

/// All the macOS-side resources backing a live capture session.
///
/// Drop order matters: stop the device *before* destroying the tap,
/// otherwise the HAL can pull one more buffer out of a tap whose
/// underlying process object is mid-teardown. Rust drops fields in
/// declaration order, so `_io_proc` (which owns the cidre
/// `StartedDevice` and calls `AudioDeviceStop` on drop) is listed
/// first and `_tap` (which calls `AudioHardwareDestroyProcessTap`)
/// second. The cidre `core-audio-record` example relies on the same
/// ordering by construction. The `AggregateHandle` is owned by
/// `IoProcHandle` indirectly via the cidre `StartedDevice`.
pub struct TapPipeline {
    _io_proc: IoProcHandle,
    _tap: TapHandle,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Pure-Rust unit test: pushing onto a full SPSC ring increments
    /// the drop counter. Validates the realtime-side back-pressure
    /// accounting without touching Core Audio.
    #[test]
    fn full_ring_increments_drop_counter() {
        // Tiny capacity so we can fill it instantly. `_consumer` is
        // held alive (not popped) so the ring stays full.
        let (mut producer, _consumer) = RingBuffer::<CaptureFrame>::new(2);
        let dropped = Arc::new(AtomicU64::new(0));

        let make_frame = || CaptureFrame {
            channel: Channel::Tap,
            host_time: 0,
            session_secs: 0.0,
            samples: vec![],
        };

        // Fill the ring.
        for _ in 0..2 {
            producer.push(make_frame()).expect("push under capacity");
        }

        // Next pushes must fail. Mirror the realtime path: bump the
        // counter exactly the way `io_proc` does.
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
    /// `Event::CaptureDegraded` via `BackpressureMonitor`, matching
    /// the pattern the consumer task uses.
    #[test]
    fn full_ring_drives_capture_degraded_event() {
        let (events_tx, mut events_rx) = broadcast::channel::<Event>(8);
        let mut monitor = BackpressureMonitor::new(SessionId::nil(), RING_CAPACITY, events_tx);

        // 95 % full → above SATURATION_THRESHOLD (90 %).
        let in_flight = (RING_CAPACITY as f32 * 0.95) as usize;
        assert!(monitor.observe(in_flight, 7, Duration::from_secs(1)));

        let evt = events_rx.try_recv().expect("event delivered");
        match evt {
            Event::CaptureDegraded { dropped_frames, .. } => {
                assert_eq!(dropped_frames, 7);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
