//! Rust side of the §9 AXObserver Swift bridge.
//!
//! Mirrors `swift/zoomax-helper/Sources/ZoomAxHelper.swift`. v0
//! ships the FFI declarations + safe wrappers + `NotYetImplemented`
//! contract; the real `(role, subrole, identifier)` tree-walk lands
//! week 6 / §9 once the §3.3 spike pins the Zoom-specific triple.
//!
//! Drift between the Swift constants and the Rust enum is caught at
//! compile time by the unit tests below — same pattern as
//! `whisperkit_bridge`.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use thiserror::Error;

#[cfg(target_vendor = "apple")]
mod ffi {
    use std::os::raw::c_char;

    unsafe extern "C" {
        pub(super) fn ax_register_observer(bundle_id: *const c_char) -> i32;
        pub(super) fn ax_poll(out: *mut *mut c_char) -> i32;
        pub(super) fn ax_release_observer() -> i32;
        pub(super) fn ax_free_string(p: *mut c_char);
        pub(super) fn ax_dump_tree(
            bundle_id: *const c_char,
            max_nodes: i32,
            out: *mut *mut c_char,
        ) -> i32;
    }
}

/// Pinned constants matching the Swift side. Drift fails CI at the
/// unit-test layer below, not at runtime in production.
pub const AX_OK_RAW: i32 = 0;
pub const AX_NOT_IMPLEMENTED_RAW: i32 = -1;
pub const AX_PROCESS_NOT_RUNNING_RAW: i32 = -2;
pub const AX_NO_PERMISSION_RAW: i32 = -3;
pub const AX_INTERNAL_RAW: i32 = -4;

/// Status codes from the Swift bridge. `Internal` carries the raw
/// code so a future Swift renumber surfaces with its actual integer
/// rather than getting silently coerced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxStatus {
    Ok,
    NotYetImplemented,
    /// The target bundle id has no running process.
    ProcessNotRunning,
    /// macOS Accessibility permission has not been granted.
    NoPermission,
    /// Internal sentinel (-4) **or** any unknown code we haven't
    /// mapped, with the raw integer preserved.
    Internal(i32),
}

impl AxStatus {
    pub fn from_raw(code: i32) -> Self {
        match code {
            AX_OK_RAW => Self::Ok,
            AX_NOT_IMPLEMENTED_RAW => Self::NotYetImplemented,
            AX_PROCESS_NOT_RUNNING_RAW => Self::ProcessNotRunning,
            AX_NO_PERMISSION_RAW => Self::NoPermission,
            other => Self::Internal(other),
        }
    }
}

#[derive(Debug, Error)]
pub enum AxBridgeError {
    #[error("AX bridge not yet implemented (arrives week 6 per §9)")]
    NotYetImplemented,
    #[error("target process is not running")]
    ProcessNotRunning,
    #[error("macOS Accessibility permission not granted")]
    NoPermission,
    #[error("AX bridge returned a null buffer; allocator failure?")]
    NullBuffer,
    #[error("AX bridge returned non-utf8 bytes")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("bundle id contains a NUL byte; can't pass to FFI")]
    BundleIdNul,
    #[error("AX bridge internal error (code {code})")]
    Internal { code: i32 },
}

/// Convert a *non-`Ok`* status into [`AxBridgeError`]. Callers must
/// filter `Ok` first.
impl From<AxStatus> for AxBridgeError {
    fn from(status: AxStatus) -> Self {
        match status {
            AxStatus::Ok => AxBridgeError::Internal { code: AX_OK_RAW },
            AxStatus::NotYetImplemented => AxBridgeError::NotYetImplemented,
            AxStatus::ProcessNotRunning => AxBridgeError::ProcessNotRunning,
            AxStatus::NoPermission => AxBridgeError::NoPermission,
            AxStatus::Internal(code) => AxBridgeError::Internal { code },
        }
    }
}

/// Register an AXObserver against the running process matching
/// `bundle_id` (e.g. `"us.zoom.xos"`). Returns `Ok(())` once the
/// observer is wired; v0 always returns `NotYetImplemented`.
///
/// # Threading
///
/// The real impl will register an `AXObserver` on the calling
/// thread's RunLoop. The Rust orchestrator dedicates a thread to
/// this; do not call from the Tauri main thread.
#[cfg(target_vendor = "apple")]
pub fn ax_register(bundle_id: &str) -> Result<(), AxBridgeError> {
    let c = bundle_id_to_cstring(bundle_id)?;
    // SAFETY: `ax_register_observer` reads the C string and returns
    // a status code. The CString outlives the call.
    let raw = unsafe { ffi::ax_register_observer(c.as_ptr()) };
    match AxStatus::from_raw(raw) {
        AxStatus::Ok => Ok(()),
        other => Err(AxBridgeError::from(other)),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub fn ax_register(_bundle_id: &str) -> Result<(), AxBridgeError> {
    Err(AxBridgeError::NotYetImplemented)
}

/// Poll for the next speaker change. Returns the JSONL line as a
/// `String` (one event), or `Ok(None)` if no change was queued.
/// Currently always returns `Err(NotYetImplemented)` since v0 has no
/// real observer wired.
#[cfg(target_vendor = "apple")]
pub fn ax_poll() -> Result<Option<String>, AxBridgeError> {
    let mut buf: *mut c_char = std::ptr::null_mut();
    // SAFETY: `ax_poll` writes a malloc'd buffer into `*out` and
    // returns the status code.
    let raw = unsafe { ffi::ax_poll(&mut buf) };
    let status = AxStatus::from_raw(raw);

    if buf.is_null() {
        return match status {
            AxStatus::Ok => Err(AxBridgeError::NullBuffer),
            other => Err(AxBridgeError::from(other)),
        };
    }

    // SAFETY: `buf` is NUL-terminated; copy bytes into a Rust String
    // and free the C buffer regardless of which arm we take.
    let parsed: Result<String, AxBridgeError> = unsafe {
        let cstr = CStr::from_ptr(buf);
        cstr.to_str()
            .map(|s| s.to_owned())
            .map_err(AxBridgeError::from)
    };
    unsafe { ffi::ax_free_string(buf) };

    let body = parsed?;
    match status {
        AxStatus::Ok => Ok(if body.is_empty() { None } else { Some(body) }),
        other => Err(AxBridgeError::from(other)),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub fn ax_poll() -> Result<Option<String>, AxBridgeError> {
    Err(AxBridgeError::NotYetImplemented)
}

/// Release the observer registered via [`ax_register`]. Idempotent;
/// safe to call when no observer is registered.
#[cfg(target_vendor = "apple")]
pub fn ax_release() -> Result<(), AxBridgeError> {
    // SAFETY: `ax_release_observer` takes no arguments and returns
    // the status code; the Swift side is idempotent.
    let raw = unsafe { ffi::ax_release_observer() };
    match AxStatus::from_raw(raw) {
        AxStatus::Ok => Ok(()),
        other => Err(AxBridgeError::from(other)),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub fn ax_release() -> Result<(), AxBridgeError> {
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn bundle_id_to_cstring(s: &str) -> Result<CString, AxBridgeError> {
    CString::new(s.as_bytes()).map_err(|_| AxBridgeError::BundleIdNul)
}

/// Walk the AX tree under `bundle_id`'s running process and return a
/// JSON document describing every visited node. Used by the
/// `heron ax-dump` subcommand to capture the speaker-indicator
/// `(role, subrole, identifier)` triple during a live Zoom call (per
/// `docs/archives/plan.md` §3.3 — the week-0 spike deliverable).
///
/// `max_nodes` caps the walk; pass `0` to use the bridge's internal
/// default (4096 — enough for a fully-populated 49-tile gallery).
#[cfg(target_vendor = "apple")]
pub fn ax_dump_tree(bundle_id: &str, max_nodes: i32) -> Result<String, AxBridgeError> {
    let c = bundle_id_to_cstring(bundle_id)?;
    let mut buf: *mut c_char = std::ptr::null_mut();
    // SAFETY: `ax_dump_tree` reads the bundle_id C string, writes a
    // malloc'd buffer into `*out`, and returns the status code. We
    // free the buffer below regardless of which branch we take.
    let raw = unsafe { ffi::ax_dump_tree(c.as_ptr(), max_nodes, &mut buf) };
    let status = AxStatus::from_raw(raw);

    if buf.is_null() {
        return match status {
            AxStatus::Ok => Err(AxBridgeError::NullBuffer),
            other => Err(AxBridgeError::from(other)),
        };
    }

    // SAFETY: NUL-terminated string allocated by Swift; copy into
    // a Rust String, then free regardless of parse outcome.
    let parsed: Result<String, AxBridgeError> = unsafe {
        let cstr = CStr::from_ptr(buf);
        cstr.to_str()
            .map(|s| s.to_owned())
            .map_err(AxBridgeError::from)
    };
    unsafe { ffi::ax_free_string(buf) };

    let body = parsed?;
    match status {
        AxStatus::Ok => Ok(body),
        other => Err(AxBridgeError::from(other)),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub fn ax_dump_tree(_bundle_id: &str, _max_nodes: i32) -> Result<String, AxBridgeError> {
    Err(AxBridgeError::NotYetImplemented)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn raw_constants_match_swift_side_verbatim() {
        // The Swift side defines `private let AX_*: Int32 = …` with
        // these exact values.
        assert_eq!(AX_OK_RAW, 0);
        assert_eq!(AX_NOT_IMPLEMENTED_RAW, -1);
        assert_eq!(AX_PROCESS_NOT_RUNNING_RAW, -2);
        assert_eq!(AX_NO_PERMISSION_RAW, -3);
        assert_eq!(AX_INTERNAL_RAW, -4);
    }

    #[test]
    fn status_from_raw_round_trips_every_known_code() {
        assert_eq!(AxStatus::from_raw(AX_OK_RAW), AxStatus::Ok);
        assert_eq!(
            AxStatus::from_raw(AX_NOT_IMPLEMENTED_RAW),
            AxStatus::NotYetImplemented
        );
        assert_eq!(
            AxStatus::from_raw(AX_PROCESS_NOT_RUNNING_RAW),
            AxStatus::ProcessNotRunning
        );
        assert_eq!(
            AxStatus::from_raw(AX_NO_PERMISSION_RAW),
            AxStatus::NoPermission
        );
    }

    #[test]
    fn status_from_raw_preserves_unknown_codes() {
        // -4 is the sentinel; preserved verbatim.
        assert_eq!(AxStatus::from_raw(-4), AxStatus::Internal(-4));
        // Unknown codes also surface via Internal with the raw value.
        assert_eq!(AxStatus::from_raw(-99), AxStatus::Internal(-99));
        assert_eq!(AxStatus::from_raw(7), AxStatus::Internal(7));
    }

    #[test]
    fn ax_error_internal_carries_the_raw_code() {
        let e = AxBridgeError::from(AxStatus::Internal(-99));
        match e {
            AxBridgeError::Internal { code } => assert_eq!(code, -99),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn register_against_zoom_surfaces_process_or_permission_error_in_ci() {
        // Real-impl behavior: with no Zoom process running, the Swift
        // bridge returns AX_PROCESS_NOT_RUNNING. If Zoom *is* running
        // but Accessibility isn't granted, AX_NO_PERMISSION. Both are
        // valid CI outcomes; the live-Zoom happy path lives in
        // tests/ax_observer_real.rs.
        let result = ax_register("us.zoom.xos");
        match result {
            Err(AxBridgeError::ProcessNotRunning) | Err(AxBridgeError::NoPermission) => {}
            Ok(()) => panic!(
                "ax_register unexpectedly succeeded; this test expects no Zoom \
                 running on the CI host. Run tests/ax_observer_real.rs instead."
            ),
            Err(other) => panic!("expected ProcessNotRunning or NoPermission; got {other:?}"),
        }
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn poll_with_no_observer_returns_none() {
        // Real-impl behavior: with nothing registered, the Swift queue
        // is empty and the bridge writes "" → AX_OK, which the Rust
        // wrapper translates to Ok(None).
        let result = ax_poll();
        assert!(
            matches!(result, Ok(None)),
            "expected Ok(None), got {result:?}"
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn release_is_idempotent_and_succeeds() {
        // Swift side is idempotent; calling release twice with no
        // observer registered must succeed twice.
        ax_release().expect("first release");
        ax_release().expect("second release");
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn bundle_id_with_internal_nul_is_rejected() {
        let result = ax_register("us.zoom\0xos");
        assert!(matches!(result, Err(AxBridgeError::BundleIdNul)));
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn off_apple_shims_return_not_yet_implemented() {
        assert!(matches!(
            ax_register("us.zoom.xos"),
            Err(AxBridgeError::NotYetImplemented)
        ));
        assert!(matches!(ax_poll(), Err(AxBridgeError::NotYetImplemented)));
        assert!(matches!(
            ax_dump_tree("us.zoom.xos", 0),
            Err(AxBridgeError::NotYetImplemented)
        ));
        // Off-Apple release is a no-op success — there's nothing to
        // release on a platform without the bridge.
        ax_release().expect("off-Apple release is a no-op");
    }
}
