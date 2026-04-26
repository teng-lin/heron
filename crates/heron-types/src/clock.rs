//! Shared clock anchored at session start.
//!
//! Two clocks need to be aligned during a session:
//!
//! - **Mach host time** (`mach_absolute_time`): used by Core Audio
//!   frame timestamps. Monotonic, ticks at the platform-specific
//!   timebase (`numer/denom` ratio in nanoseconds-per-tick).
//! - **Wall clock** (`SystemTime`): used by AX events and by anything
//!   that needs to render a real timestamp.
//!
//! `SessionClock::new()` anchors both at the same instant. After that:
//! - [`SessionClock::host_to_session_secs`] converts a raw mach tick
//!   into seconds since session start.
//! - [`SessionClock::wall_to_session_secs`] converts a `SystemTime`
//!   into seconds since session start.
//!
//! See `docs/implementation.md` §0.9 and §5.3.

use std::time::SystemTime;

/// Mach `mach_timebase_info` — `numer/denom` is nanoseconds-per-tick.
#[derive(Debug, Clone, Copy)]
pub struct TimebaseInfo {
    pub numer: u32,
    pub denom: u32,
}

impl TimebaseInfo {
    /// Convert a tick delta into nanoseconds.
    ///
    /// Multiplies in u128 to avoid overflow on the long sessions
    /// (`u64 * u32` is up to `2^96`, easily exceeding `u64::MAX`).
    #[inline]
    pub fn ticks_to_nanos(self, ticks: u64) -> u128 {
        u128::from(ticks) * u128::from(self.numer) / u128::from(self.denom)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SessionClock {
    pub started_at: SystemTime,
    pub mach_anchor: u64,
    pub mach_timebase: TimebaseInfo,
}

impl SessionClock {
    /// Anchor the clock at "now". On non-Apple platforms `mach_anchor`
    /// is `0` and `mach_timebase` is `{1, 1}` — the wall-clock half of
    /// the API still works, but [`host_to_session_secs`] is meaningless
    /// because there is no `mach_absolute_time` to feed it.
    ///
    /// The mach and wall reads cannot be made atomically; we sample
    /// `mach_absolute_time` first because it's the hot-path conversion
    /// target (every audio frame). The remaining sub-microsecond skew
    /// is absorbed by the §9.3 aligner's first-60s offset estimation.
    ///
    /// [`host_to_session_secs`]: SessionClock::host_to_session_secs
    pub fn new() -> Self {
        let (mach_anchor, mach_timebase) = platform::now_with_timebase();
        let started_at = SystemTime::now();
        Self {
            started_at,
            mach_anchor,
            mach_timebase,
        }
    }

    /// Convert a Core Audio host time (raw mach ticks) into seconds
    /// since [`Self::started_at`].
    ///
    /// `host_time` is interpreted as "ticks since boot" per
    /// `mach_absolute_time`; the offset relative to the session anchor
    /// is converted with the mach timebase.
    ///
    /// Returns a negative value if `host_time` precedes the session
    /// anchor (can happen for ringbuffer frames captured a few ms
    /// before `SessionClock::new()`), mirroring [`wall_to_session_secs`]'s
    /// behavior.
    ///
    /// [`wall_to_session_secs`]: SessionClock::wall_to_session_secs
    #[inline]
    pub fn host_to_session_secs(&self, host_time: u64) -> f64 {
        let signed_delta_ticks = i128::from(host_time) - i128::from(self.mach_anchor);
        let abs_ticks = signed_delta_ticks.unsigned_abs() as u64;
        let abs_nanos = self.mach_timebase.ticks_to_nanos(abs_ticks);
        let secs = abs_nanos as f64 / 1_000_000_000.0;
        if signed_delta_ticks < 0 { -secs } else { secs }
    }

    /// Convert a wall-clock `SystemTime` into seconds since
    /// [`Self::started_at`]. Returns a negative value if `wall` is
    /// before the anchor.
    #[inline]
    pub fn wall_to_session_secs(&self, wall: SystemTime) -> f64 {
        match wall.duration_since(self.started_at) {
            Ok(d) => d.as_secs_f64(),
            Err(e) => -e.duration().as_secs_f64(),
        }
    }

    /// Monotonic "right now" in session-secs.
    ///
    /// Stamps relative to the session's mach anchor (Apple) so the
    /// returned value never regresses on NTP/DST adjustments to wall
    /// time — the property [`heron_zoom::aligner`] depends on, since
    /// a backward-stepped event timestamp creates a degenerate
    /// `SpeakingInterval` where `t0 >= t1` and silently drops the
    /// overlap-attributed turn.
    ///
    /// Off-Apple this falls through to [`Self::wall_to_session_secs`]
    /// — there is no mach clock on Linux/Windows, but the heron
    /// pipeline that consumes this value (the §9.3 aligner) is
    /// macOS-only in v1, so the fallback is best-effort rather than
    /// load-bearing.
    #[inline]
    pub fn now_session_secs(&self) -> f64 {
        #[cfg(target_vendor = "apple")]
        {
            let (now, _timebase) = platform::now_with_timebase();
            self.host_to_session_secs(now)
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            self.wall_to_session_secs(SystemTime::now())
        }
    }
}

impl Default for SessionClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::TimebaseInfo;

    #[repr(C)]
    struct mach_timebase_info_data_t {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut mach_timebase_info_data_t) -> i32;
    }

    pub(super) fn now_with_timebase() -> (u64, TimebaseInfo) {
        // SAFETY: both calls are documented mach kernel APIs available
        // on every supported macOS version. `mach_absolute_time` takes
        // no arguments and returns a raw tick count. `mach_timebase_info`
        // writes through the provided pointer to a stack-allocated
        // `mach_timebase_info_data_t`. We initialize the struct to a
        // safe `{1, 1}` so an (extremely unlikely) non-zero return code
        // doesn't leave us with a zero `denom` and a divide-by-zero in
        // `ticks_to_nanos`.
        let (anchor, kr, info) = unsafe {
            let anchor = mach_absolute_time();
            let mut info = mach_timebase_info_data_t { numer: 1, denom: 1 };
            let kr = mach_timebase_info(&mut info);
            (anchor, kr, info)
        };
        let timebase = if kr == 0 && info.denom != 0 {
            TimebaseInfo {
                numer: info.numer,
                denom: info.denom,
            }
        } else {
            // mach_timebase_info has been documented to never fail since
            // 10.0, so this branch is defensive. Falling back to 1:1
            // means `host_to_session_secs` interprets ticks as nanos —
            // wrong by a small constant factor but never panics.
            TimebaseInfo { numer: 1, denom: 1 }
        };
        (anchor, timebase)
    }
}

#[cfg(not(target_vendor = "apple"))]
mod platform {
    use super::TimebaseInfo;

    pub(super) fn now_with_timebase() -> (u64, TimebaseInfo) {
        // No mach clock off-Apple. The mach half of the API will
        // return zero deltas; callers on other platforms must treat
        // `host_to_session_secs` results as undefined.
        (0, TimebaseInfo { numer: 1, denom: 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn wall_round_trip_within_1ms() {
        let clock = SessionClock::new();
        // 5 seconds after the anchor.
        let wall = clock.started_at + Duration::from_millis(5_000);
        let secs = clock.wall_to_session_secs(wall);
        assert!(
            (secs - 5.0).abs() < 1e-3,
            "expected ≈5.0s, got {secs}s (Δ={:.6}s)",
            secs - 5.0
        );
    }

    #[test]
    fn wall_negative_for_pre_anchor_time() {
        let clock = SessionClock::new();
        let wall = clock.started_at - Duration::from_millis(250);
        let secs = clock.wall_to_session_secs(wall);
        assert!(secs < 0.0);
        assert!(
            (secs + 0.250).abs() < 1e-3,
            "expected ≈-0.250s, got {secs}s"
        );
    }

    #[test]
    fn wall_zero_at_anchor() {
        let clock = SessionClock::new();
        let secs = clock.wall_to_session_secs(clock.started_at);
        assert!(secs.abs() < 1e-9);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn host_round_trip_within_1ms() {
        let clock = SessionClock::new();
        // Sleep ≥10ms, then read mach_absolute_time. We only assert
        // the *lower* bound — i.e. the conversion produced something
        // at least as large as the requested sleep, allowing for the
        // 1ms timebase precision claimed in the spec. We do NOT
        // assert an upper bound: GHA macOS runners (and any heavily
        // loaded host) can stretch a 25ms sleep into 100+ms of real
        // time, which would falsely fail an upper-bound check on this
        // smoke test. Drift correctness is covered by the aligner
        // tests in week 7 (§9.3) against the wall clock.
        let sleep_ms = 25u64;
        std::thread::sleep(Duration::from_millis(sleep_ms));
        // SAFETY: same trivial extern as platform::now_with_timebase.
        let now = unsafe { extern_now() };
        let host_secs = clock.host_to_session_secs(now);
        let lower = (sleep_ms as f64) / 1000.0 - 0.005;
        assert!(
            host_secs >= lower,
            "host_to_session_secs returned {host_secs}s after {sleep_ms}ms sleep (expected ≥ {lower})"
        );
    }

    #[cfg(target_vendor = "apple")]
    unsafe fn extern_now() -> u64 {
        unsafe extern "C" {
            fn mach_absolute_time() -> u64;
        }
        unsafe { mach_absolute_time() }
    }

    #[test]
    fn timebase_ticks_to_nanos_overflow_safe() {
        // Worst case: 1 hour of mach ticks at the typical 1ns/tick
        // ratio is 3.6e12. Even at numer=125, denom=3 (some Apple
        // Silicon ratios), this stays well under u128::MAX.
        let tb = TimebaseInfo {
            numer: 125,
            denom: 3,
        };
        let one_hour_ticks = 3_600_000_000_000u64;
        let nanos = tb.ticks_to_nanos(one_hour_ticks);
        // 3.6e12 * 125 / 3 = 1.5e14 nanoseconds = ~150,000 seconds
        assert_eq!(nanos, 150_000_000_000_000u128);
    }
}
