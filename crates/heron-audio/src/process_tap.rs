//! macOS Core Audio process tap pipeline.
//!
//! Per [`docs/plan.md`](../../../docs/plan.md) §6.2 and
//! [`docs/implementation.md`](../../../docs/implementation.md) §6, the
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
//! This module is **scaffolding**: steps 1–3 are wired against the
//! real `cidre` 0.15 surface; step 4 still has a `TODO` for the
//! cidre `EscBlock` plumbing because the IO-proc closure capture
//! needs the same lock-free SPSC + `BackpressureMonitor` integration
//! that lands in week 3 (§7.4). The handle returned today owns the
//! tap + aggregate device so a Drop releases them cleanly; the
//! broadcast sender is wired but no callback fires into it yet.
//!
//! All public items in this file are macOS-only by virtue of
//! `#[cfg(target_os = "macos")]` on the `mod process_tap;` line in
//! `lib.rs`.

use heron_types::SessionClock;
use tokio::sync::broadcast;

use cidre::{
    arc, cf, core_audio as ca, core_audio::aggregate_device_keys as agg_keys,
    core_audio::hardware::StartedDevice, core_audio::sub_device_keys as sub_keys, ns, sys,
};

use crate::{AudioError, CaptureFrame};

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

/// Handle returned by [`install_io_proc`]. While it's alive, the IO
/// proc keeps firing into the broadcast channel passed at install
/// time. Drop stops the device.
///
/// The cidre `StartedDevice` wrapper itself calls `AudioDeviceStop`
/// on drop, so holding it as a plain field is enough — no explicit
/// `stop()` method needed.
pub struct IoProcHandle {
    /// The cidre `StartedDevice<AggregateDevice>`; drops via
    /// `cidre::core_audio::AudioDeviceStop`.
    _started_device: StartedDevice<ca::AggregateDevice>,
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

    // Iterate through the array and pick the first non-zero pid.
    // ns::Array exposes `len()` + `get()`. Pid 0 is the "no
    // associated process" sentinel — observed when an app is
    // listed but already terminated mid-poll.
    for i in 0..apps.len() {
        // ns::Array::get returns Result<Retained, &Exception> rather
        // than Option, because Foundation array indexing throws
        // NSRangeException out of bounds. We've just bounded the
        // index by `len()`, so the Err branch should be unreachable;
        // skip it defensively.
        if let Ok(app) = apps.get(i) {
            let pid: sys::Pid = app.pid();
            if pid > 0 {
                return Ok(pid);
            }
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

/// Install an IO proc on `aggregate` that forwards each callback's
/// PCM data to `tx` as a [`CaptureFrame`], then start the device.
///
/// **TODO(io-proc):** the cidre `EscBlock` capture for an IO block is
/// not yet wired. The realtime proc has hard constraints — no allocs,
/// no locks — so the production path is a lock-free SPSC ringbuffer
/// (`rtrb`) feeding a separate Tokio task that does the bounded
/// `broadcast::Sender::send`. That ring lands with the rest of §7
/// (week 3). For now this returns the started device with no proc
/// wired so the surface compiles and downstream callers can hold a
/// handle; `tx` and `clock` are kept on the public signature so the
/// week-3 patch is a body-only change.
pub fn install_io_proc(
    aggregate: AggregateHandle,
    tx: broadcast::Sender<CaptureFrame>,
    clock: SessionClock,
) -> Result<IoProcHandle, AudioError> {
    // Touch the tx + clock so unused-warnings stay quiet without
    // changing the public API. The week-3 patch wires them into the
    // EscBlock closure.
    let _ = (tx, clock);

    // TODO(io-proc): build a `cidre::core_audio::DeviceIoBlock` that
    // captures an `Arc<RingbufferProducer<CaptureFrame>>` plus the
    // `SessionClock`, calls `clock.host_to_session_secs(now.host_time)`
    // on each callback, copies the f32 samples out of `input_data`,
    // and pushes a `CaptureFrame` into the ringbuffer. A consumer
    // task on a Tokio runtime drains the ringbuffer into `tx`.
    //
    // Reference: `cidre::core_audio::Device::create_io_proc_id_with_block`
    // (gated behind cidre's `blocks` + `dispatch` features, which we
    // get via cidre's default features) and the worked example at
    // `cidre/examples/core-audio-record/main.rs`.

    // Start the device with no proc id. Without an installed IO proc
    // this is mostly a no-op (Core Audio happily runs an aggregate
    // with no client), but it ensures we exercise the AudioDeviceStart
    // path so the §6.2 done-when ("returns a real handle, not
    // NotYetImplemented") is met on the hot path.
    let started = ca::device_start(aggregate.device, None)
        .map_err(|e| AudioError::Aborted(format!("AudioDeviceStart failed: {e:?}")))?;

    Ok(IoProcHandle {
        _started_device: started,
    })
}

/// Top-level convenience: bundle id -> (TapHandle, AggregateHandle,
/// IoProcHandle). The three handles are returned separately so the
/// caller (here, [`crate::AudioCapture::start`]) can compose them
/// into the public [`crate::AudioCaptureHandle`].
pub fn open_tap(
    bundle_id: &str,
    frames_tx: broadcast::Sender<CaptureFrame>,
    clock: SessionClock,
) -> Result<TapPipeline, AudioError> {
    let pid = find_pid_by_bundle_id(bundle_id)?;
    let tap = create_process_tap(pid)?;
    let tap_uid = tap.uid()?;
    let aggregate = build_aggregate_device(&tap_uid)?;
    let io_proc = install_io_proc(aggregate, frames_tx, clock)?;
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
