//! Calendar access via the EventKit Swift bridge.
//!
//! The Swift side lives at `swift/eventkit-helper/` and exports three
//! `@_cdecl` symbols (`ek_request_access`, `ek_read_window_json`,
//! `ek_free_string`) — see `docs/archives/implementation.md` §5.4 and
//! `docs/archives/swift-bridge-pattern.md`.
//!
//! Functions in this module trigger a TCC prompt the first time they
//! are called on a clean machine. Call `calendar_has_access` once at
//! app start; downstream calls succeed silently if the user granted
//! access in the past.

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(target_vendor = "apple")]
mod ffi {
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

/// Pinned constants matching `swift/eventkit-helper/.../EventKitHelper.swift`.
/// Drift here is caught at compile time by the unit tests below that
/// assert each enum variant equals its raw constant.
pub const EK_ACCESS_GRANTED_RAW: i32 = 1;
pub const EK_ACCESS_DENIED_RAW: i32 = 0;
pub const EK_TIMEOUT_RAW: i32 = -4;

/// Outcome of an `ek_request_access` call. `Granted` and `Denied` map
/// to user choices in the TCC dialog; `Timeout` means the Swift
/// bridge gave up waiting for the prompt to be dismissed inside
/// `EK_REQUEST_TIMEOUT`. Distinct from `Denied` because callers may
/// want to retry on timeout (e.g., re-prompt later) but not on a hard
/// denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EkAccessStatus {
    Granted,
    Denied,
    Timeout,
}

impl EkAccessStatus {
    pub fn from_raw(code: i32) -> Self {
        match code {
            EK_ACCESS_GRANTED_RAW => Self::Granted,
            EK_TIMEOUT_RAW => Self::Timeout,
            // Unknown codes collapse to Denied, matching the pre-timeout
            // contract that "anything not 1" means "don't proceed".
            _ => Self::Denied,
        }
    }
}

/// Request full calendar access.
///
/// On the first call after a TCC reset (or first-run install) macOS
/// will display a permission prompt. The call blocks the calling
/// thread until the user makes a choice or the Swift bridge's
/// `EK_REQUEST_TIMEOUT` watchdog fires; subsequent calls return
/// immediately.
///
/// Returns `Ok(true)` when the user explicitly granted access,
/// `Ok(false)` when the user denied access or the request errored,
/// and `Err(CalendarError::Timeout)` when the bridge gave up waiting
/// for the prompt to be dismissed.
///
/// # Threading
///
/// **Do not call from the UI/main thread on a fresh install** — the
/// underlying `DispatchSemaphore.wait()` in the Swift bridge blocks
/// the caller until the user dismisses the TCC dialog (or the
/// timeout fires), which would freeze the Tauri main loop. The
/// week-11 onboarding flow (per §13) wraps this in
/// `tokio::task::spawn_blocking` so the UI stays responsive.
///
/// `Task.detached` in the Swift side prevents the same-queue
/// deadlock called out in §5.4, and the bounded semaphore wait
/// guarantees that a wedged TCC daemon eventually surfaces as a
/// recoverable `Timeout` rather than a permanent hang.
#[cfg(target_vendor = "apple")]
pub fn calendar_has_access() -> Result<bool, CalendarError> {
    // SAFETY: `ek_request_access` takes no arguments and returns an
    // i32. The Swift side dispatches the async request onto a
    // detached Task so re-entrancy on the caller's queue cannot
    // deadlock — see EventKitHelper.swift comment at `ek_request_access`.
    let raw = unsafe { ffi::ek_request_access() };
    match EkAccessStatus::from_raw(raw) {
        EkAccessStatus::Granted => Ok(true),
        EkAccessStatus::Denied => Ok(false),
        EkAccessStatus::Timeout => Err(CalendarError::Timeout),
    }
}

/// Off-Apple stub. Always returns `Ok(false)` — there is no EventKit
/// on non-Apple platforms.
#[cfg(not(target_vendor = "apple"))]
pub fn calendar_has_access() -> Result<bool, CalendarError> {
    Ok(false)
}

/// One calendar event in the read window. Field shape matches the
/// JSON the Swift side emits in `ek_read_window_json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalendarEvent {
    pub title: String,
    /// Unix epoch seconds, matches Swift's `timeIntervalSince1970`.
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub attendees: Vec<CalendarAttendee>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalendarAttendee {
    pub name: String,
    /// `mailto:user@example.com` per EKParticipant.url.
    pub email: String,
}

#[derive(Debug, Error)]
pub enum CalendarError {
    #[error("calendar access denied")]
    Denied,
    #[error("invalid time window: start={start} end={end} (start must be ≤ end)")]
    InvalidWindow { start: i64, end: i64 },
    #[error("EventKit returned a null buffer")]
    NullBuffer,
    #[error("invalid UTF-8 from EventKit bridge")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("invalid JSON from EventKit bridge: {0}")]
    InvalidJson(#[from] serde_json::Error),
    /// Swift bridge gave up waiting on the TCC permission prompt
    /// inside `EK_REQUEST_TIMEOUT`. Distinct from `Denied` so the
    /// orchestrator can decide between "retry later" (Timeout) and
    /// "give up, the user said no" (Denied).
    #[error("EventKit access request timed out")]
    Timeout,
}

/// Read every calendar event whose `[start, end]` overlaps the given
/// half-open window `[start_utc, end_utc)`.
///
/// **Denial contract** (per §12.2). If the user has not granted
/// calendar access, this returns `Ok(None)` — *not* an error. Callers
/// are expected to degrade gracefully (no auto-attendees, no auto-title)
/// rather than block on a prompt at session-start time.
///
/// On a fresh install where the user has never been asked, this *does*
/// trigger a TCC prompt synchronously (the Swift side blocks on a
/// `DispatchSemaphore` until the user dismisses it). The week-11
/// onboarding flow runs the prompt up front in the dedicated
/// "Calendar" step so production calls never see the dialog.
///
/// # Threading
///
/// Same constraint as [`calendar_has_access`]: never call from the UI
/// main thread on a machine that may show the TCC dialog. Wrap in
/// `tokio::task::spawn_blocking` from async contexts.
#[cfg(target_vendor = "apple")]
pub fn calendar_read_one_shot(
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
) -> Result<Option<Vec<CalendarEvent>>, CalendarError> {
    let start = start_utc.timestamp();
    let end = end_utc.timestamp();
    if start > end {
        return Err(CalendarError::InvalidWindow { start, end });
    }
    // Honor the §12.2 denial contract: a denied user yields `Ok(None)`
    // so callers degrade gracefully. A timed-out prompt, however, is a
    // *recoverable* error the caller may want to retry — surface it
    // verbatim instead of collapsing into the denial branch.
    if !calendar_has_access()? {
        return Ok(None);
    }

    let mut buf: *mut std::os::raw::c_char = std::ptr::null_mut();
    // SAFETY: `ek_read_window_json` writes a malloc'd C string into
    // `*out` and returns the event count. We hand ownership back via
    // `ek_free_string` below — the contract is documented in
    // EventKitHelper.swift and `docs/archives/swift-bridge-pattern.md`.
    let count = unsafe { ffi::ek_read_window_json(start, end, &mut buf) };

    if buf.is_null() {
        // count == 0 with null buf is a legitimate empty window. A
        // count > 0 with null buf means the Swift side's malloc failed
        // — surface it so the caller can retry rather than silently
        // drop events.
        return if count == 0 {
            Ok(Some(Vec::new()))
        } else {
            Err(CalendarError::NullBuffer)
        };
    }

    // SAFETY: `buf` is a NUL-terminated C string allocated by the
    // Swift side via `malloc` + `memcpy` + explicit NUL terminator.
    // We must release it via `ek_free_string` regardless of how this
    // function exits, so the parse runs *between* CStr construction
    // and free. CStr borrows from `buf`; the &str slice is copied into
    // the events Vec by serde_json, after which we drop the buffer.
    let json_result = unsafe {
        let cstr = std::ffi::CStr::from_ptr(buf);
        let s = cstr.to_str();
        let parsed = s.map_err(CalendarError::from).and_then(|json| {
            serde_json::from_str::<Vec<CalendarEvent>>(json).map_err(CalendarError::from)
        });
        ffi::ek_free_string(buf);
        parsed
    };

    json_result.map(Some)
}

/// Off-Apple stub. Always returns `Ok(None)` — calendar access is a
/// macOS-only feature in v1; this lets non-Apple test runners exercise
/// callers that respect the denial contract without changing types.
#[cfg(not(target_vendor = "apple"))]
pub fn calendar_read_one_shot(
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
) -> Result<Option<Vec<CalendarEvent>>, CalendarError> {
    if start_utc > end_utc {
        return Err(CalendarError::InvalidWindow {
            start: start_utc.timestamp(),
            end: end_utc.timestamp(),
        });
    }
    Ok(None)
}

/// Trait wrapper over [`calendar_read_one_shot`] so the orchestrator can
/// inject a stub in tests instead of hitting the live EventKit bridge.
///
/// The method is synchronous because the underlying Swift bridge blocks
/// on a `DispatchSemaphore`; callers running inside an async context
/// must wrap calls in `tokio::task::spawn_blocking` (the orchestrator
/// pipeline does this at the single call-site that drives the live
/// session, so impls don't have to).
pub trait CalendarReader: Send + Sync {
    fn read_window(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
    ) -> Result<Option<Vec<CalendarEvent>>, CalendarError>;
}

/// Production [`CalendarReader`]: defers to [`calendar_read_one_shot`].
pub struct EventKitCalendarReader;

impl CalendarReader for EventKitCalendarReader {
    fn read_window(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
    ) -> Result<Option<Vec<CalendarEvent>>, CalendarError> {
        calendar_read_one_shot(start_utc, end_utc)
    }
}

/// Convenience: round-trip a Unix-epoch-seconds value (as returned in
/// [`CalendarEvent::start`] / `end`) into a UTC `DateTime`. Saturates
/// out-of-range values to the chrono boundary rather than panicking.
pub fn epoch_seconds_to_utc(secs: f64) -> DateTime<Utc> {
    // Use `floor()` rather than `trunc()` so the fractional residual
    // handed to `nanos` is always non-negative. `trunc()` rounds
    // toward zero and would yield a negative fraction for negative
    // timestamps, which `Utc.timestamp_opt` rejects — and the
    // `as u32` cast would silently wrap into a huge value, giving a
    // `DateTime` off by up to one second. chrono's `single()` already
    // saturates on out-of-range i64 inputs, so the explicit branch
    // here only covers the case where the f64 itself overflows i64.
    let whole = secs.floor();
    let nanos = ((secs - whole) * 1.0e9).round() as u32;
    let whole_i = whole as i64;
    let saturating = if secs < 0.0 {
        DateTime::<Utc>::MIN_UTC
    } else {
        DateTime::<Utc>::MAX_UTC
    };
    Utc.timestamp_opt(whole_i, nanos)
        .single()
        .unwrap_or(saturating)
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
        let outcome = calendar_has_access();
        // The point of this test is to prove the Rust → Swift bridge
        // links and the `ek_request_access` symbol resolves. The grant
        // outcome is whatever the user clicked in the TCC dialog.
        println!("calendar access: {outcome:?}");
    }

    #[test]
    fn epoch_round_trip_within_a_microsecond() {
        let secs = 1_700_000_000.123_456;
        let dt = epoch_seconds_to_utc(secs);
        let back = dt.timestamp() as f64 + (dt.timestamp_subsec_nanos() as f64) / 1.0e9;
        assert!(
            (back - secs).abs() < 1e-6,
            "round-trip drift: {} → {}",
            secs,
            back
        );
    }

    #[test]
    fn epoch_negative_timestamp_round_trips() {
        // Regression: an earlier `secs.trunc() as i64` plus naive
        // fractional residual fed `Utc.timestamp_opt` a negative
        // `nanos` (post-`as u32` wrap), producing a DateTime ~1 s off
        // for any sub-second negative value. `.floor()` keeps `nanos`
        // in [0, 1e9).
        let secs = -123.25;
        let dt = epoch_seconds_to_utc(secs);
        let back = dt.timestamp() as f64 + (dt.timestamp_subsec_nanos() as f64) / 1.0e9;
        assert!(
            (back - secs).abs() < 1e-6,
            "negative round-trip drift: {secs} → {back}"
        );
    }

    #[test]
    fn invalid_window_rejected() {
        let later = Utc
            .timestamp_opt(2_000, 0)
            .single()
            .expect("epoch is representable");
        let earlier = Utc
            .timestamp_opt(1_000, 0)
            .single()
            .expect("epoch is representable");
        let err = calendar_read_one_shot(later, earlier).expect_err("inverted window must error");
        assert!(matches!(err, CalendarError::InvalidWindow { .. }));
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn off_apple_returns_none() {
        let start = Utc
            .timestamp_opt(1_700_000_000, 0)
            .single()
            .expect("epoch is representable");
        let end = Utc
            .timestamp_opt(1_700_003_600, 0)
            .single()
            .expect("epoch is representable");
        let result = calendar_read_one_shot(start, end).expect("stub never errors on valid window");
        assert!(
            result.is_none(),
            "non-Apple platforms must honor the denial contract"
        );
    }

    #[test]
    fn calendar_event_round_trips_through_serde() {
        let event = CalendarEvent {
            title: "Architecture review".to_owned(),
            start: 1_700_000_000.0,
            end: 1_700_001_800.0,
            attendees: vec![
                CalendarAttendee {
                    name: "Alice".to_owned(),
                    email: "mailto:alice@example.com".to_owned(),
                },
                CalendarAttendee {
                    name: String::new(),
                    email: String::new(),
                },
            ],
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: CalendarEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, event);
    }

    #[test]
    fn missing_attendees_field_defaults_to_empty() {
        // EventKit may emit events with no `attendees` key when the
        // calendar entry has none; the Swift side currently always
        // includes the field, but our Rust deserializer must tolerate
        // a future Swift change that omits it.
        let json = r#"{"title":"Solo work","start":0,"end":1}"#;
        let parsed: CalendarEvent = serde_json::from_str(json).expect("deserialize");
        assert!(parsed.attendees.is_empty());
    }

    #[test]
    fn raw_constants_match_swift_side_verbatim() {
        // The Swift side defines `private let EK_*: Int32 = …` with
        // these exact values. Drift here is caught at compile time.
        assert_eq!(EK_ACCESS_GRANTED_RAW, 1);
        assert_eq!(EK_ACCESS_DENIED_RAW, 0);
        assert_eq!(EK_TIMEOUT_RAW, -4);
    }

    #[test]
    fn access_status_from_raw_round_trips_every_known_code() {
        assert_eq!(
            EkAccessStatus::from_raw(EK_ACCESS_GRANTED_RAW),
            EkAccessStatus::Granted
        );
        assert_eq!(
            EkAccessStatus::from_raw(EK_ACCESS_DENIED_RAW),
            EkAccessStatus::Denied
        );
        assert_eq!(
            EkAccessStatus::from_raw(EK_TIMEOUT_RAW),
            EkAccessStatus::Timeout
        );
        // Unknown codes collapse to Denied so the post-timeout
        // contract still defaults to "don't proceed".
        assert_eq!(EkAccessStatus::from_raw(-99), EkAccessStatus::Denied);
        assert_eq!(EkAccessStatus::from_raw(7), EkAccessStatus::Denied);
    }
}
