//! Calendar access via the EventKit Swift bridge.
//!
//! The Swift side lives at `swift/eventkit-helper/` and exports three
//! `@_cdecl` symbols (`ek_request_access`, `ek_read_window_json`,
//! `ek_free_string`) — see `docs/implementation.md` §5.4 and
//! `docs/swift-bridge-pattern.md`.
//!
//! Functions in this module trigger a TCC prompt the first time they
//! are called on a clean machine. Call `calendar_has_access` once at
//! app start; downstream calls succeed silently if the user granted
//! access in the past.

#[cfg(target_vendor = "apple")]
mod ffi {
    // `ek_read_window_json` and `ek_free_string` are declared so the
    // bridge surface is locked in from week 1, even though the
    // calendar-reading consumer ships in week 10 per §12. Suppress the
    // unused-symbol warnings until then.
    #![allow(dead_code)]

    use std::os::raw::{c_char, c_longlong};

    unsafe extern "C" {
        pub(super) fn ek_request_access() -> i32;
        pub(super) fn ek_read_window_json(
            start: c_longlong,
            end: c_longlong,
            out: *mut *mut c_char,
        ) -> i32;
        pub(super) fn ek_free_string(s: *mut c_char);
    }
}

/// Request full calendar access.
///
/// On the first call after a TCC reset (or first-run install) macOS
/// will display a permission prompt. The call blocks the calling
/// thread until the user makes a choice; subsequent calls return
/// immediately.
///
/// Returns `true` only when the user has explicitly granted access.
///
/// # Threading
///
/// **Do not call from the UI/main thread on a fresh install** — the
/// underlying `DispatchSemaphore.wait()` in the Swift bridge blocks
/// the caller indefinitely until the user dismisses the TCC dialog,
/// which would freeze the Tauri main loop. The week-11 onboarding
/// flow (per §13) wraps this in `tokio::task::spawn_blocking` so the
/// UI stays responsive.
///
/// `Task.detached` in the Swift side prevents the same-queue
/// deadlock called out in §5.4, but the wait remains *unbounded* —
/// if `EKEventStore.requestFullAccessToEvents` never resumes (e.g.,
/// a wedged TCC daemon), the caller hangs forever. There is no
/// timeout knob in EventKit's request API; if this ever bites in
/// practice, swap to a polling check on `EKEventStore.authorizationStatus`.
#[cfg(target_vendor = "apple")]
pub fn calendar_has_access() -> bool {
    // SAFETY: `ek_request_access` takes no arguments and returns an
    // i32. The Swift side dispatches the async request onto a
    // detached Task so re-entrancy on the caller's queue cannot
    // deadlock — see EventKitHelper.swift comment at `ek_request_access`.
    unsafe { ffi::ek_request_access() == 1 }
}

/// Off-Apple stub. Always returns `false` — there is no EventKit on
/// non-Apple platforms.
#[cfg(not(target_vendor = "apple"))]
pub fn calendar_has_access() -> bool {
    false
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Smoke test for the Rust → Swift FFI link.
    ///
    /// Ignored by default because the first call triggers a TCC
    /// prompt (`Task.detached` blocks until the user chooses), and CI
    /// has no human to click it. Run manually as the §5.4 boundary
    /// verification:
    ///
    /// ```sh
    /// cargo test -p heron-vault calendar_smoke -- --ignored
    /// ```
    #[test]
    #[ignore = "TCC prompt; run manually with `--ignored`"]
    fn calendar_smoke() {
        let granted = calendar_has_access();
        // The point of this test is to prove the Rust → Swift bridge
        // links and the `ek_request_access` symbol resolves. The grant
        // outcome is whatever the user clicked in the TCC dialog.
        println!("calendar access: {granted}");
    }
}
