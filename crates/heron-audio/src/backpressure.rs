//! Back-pressure detection for the realtime → APM → STT pipeline.
//!
//! Per [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §7.4
//! the §7.6 done-when bar requires that a 60-min run with a fake-STT
//! that lags 3s emits exactly one [`Event::CaptureDegraded`] when the
//! STT queue saturates, while the disk ringbuffer keeps ≥ 99 % of
//! captured frames.
//!
//! [`Event::CaptureDegraded`]: heron_types::Event::CaptureDegraded
//!
//! This module ships the v0 detection logic. Real
//! `tokio::sync::mpsc::Sender::capacity()` polling integrates in
//! week 3 once the full STT consumer task lands.

use std::time::Duration;

use heron_types::{Event, SessionId};
use tokio::sync::broadcast;

/// Ratio of `(in_flight / capacity)` above which the queue is
/// considered "saturated" and a [`Event::CaptureDegraded`] is emitted.
///
/// Picked empirically: at 90 % full a healthy STT can typically
/// drain back below threshold within 1–2 s of grace period; lower
/// thresholds cause spurious degraded events on a busy laptop.
pub const SATURATION_THRESHOLD: f32 = 0.90;

/// Watches a queue's depth + drop count and emits exactly one
/// `CaptureDegraded` per saturation episode. Resets the latch when
/// the queue drops back below threshold so a subsequent saturation
/// can re-fire.
pub struct BackpressureMonitor {
    session_id: SessionId,
    capacity: usize,
    saturated: bool,
    events: broadcast::Sender<Event>,
}

impl BackpressureMonitor {
    pub fn new(session_id: SessionId, capacity: usize, events: broadcast::Sender<Event>) -> Self {
        Self {
            session_id,
            capacity: capacity.max(1),
            saturated: false,
            events,
        }
    }

    /// Observe a `(in_flight, dropped_frames, at)` tuple. Emits a
    /// `CaptureDegraded` event the first time saturation is crossed,
    /// and clears the latch once the queue drops below threshold so
    /// a later saturation can re-fire.
    ///
    /// Returns `true` if an event was emitted on this call.
    pub fn observe(&mut self, in_flight: usize, dropped_frames: u32, at: Duration) -> bool {
        let ratio = in_flight as f32 / self.capacity as f32;
        if !self.saturated && ratio >= SATURATION_THRESHOLD {
            self.saturated = true;
            let _ = self.events.send(Event::CaptureDegraded {
                id: self.session_id,
                at,
                dropped_frames,
                reason: format!(
                    "STT queue saturated: {in_flight}/{cap} ({pct:.0}% — threshold {thresh:.0}%)",
                    cap = self.capacity,
                    pct = ratio * 100.0,
                    thresh = SATURATION_THRESHOLD * 100.0
                ),
            });
            return true;
        }
        if self.saturated && ratio < SATURATION_THRESHOLD {
            self.saturated = false;
        }
        false
    }

    pub fn is_saturated(&self) -> bool {
        self.saturated
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::Event;

    fn channel() -> (broadcast::Sender<Event>, broadcast::Receiver<Event>) {
        broadcast::channel(8)
    }

    #[test]
    fn fires_once_on_saturation() {
        let (tx, mut rx) = channel();
        let mut mon = BackpressureMonitor::new(SessionId::nil(), 10, tx);

        // 50 % full: no event.
        assert!(!mon.observe(5, 0, Duration::from_secs(1)));
        // 95 % full: event.
        assert!(mon.observe(10, 0, Duration::from_secs(2)));
        // still saturated; no second event.
        assert!(!mon.observe(10, 0, Duration::from_secs(3)));

        // exactly one CaptureDegraded landed.
        let evt = rx.try_recv().expect("captured");
        match evt {
            Event::CaptureDegraded {
                dropped_frames: 0, ..
            } => {}
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn re_fires_after_recovery() {
        let (tx, mut rx) = channel();
        let mut mon = BackpressureMonitor::new(SessionId::nil(), 10, tx);

        assert!(mon.observe(10, 0, Duration::from_secs(1))); // saturate #1
        assert!(!mon.observe(3, 0, Duration::from_secs(2))); // recover
        assert!(!mon.is_saturated());
        assert!(mon.observe(10, 5, Duration::from_secs(3))); // saturate #2

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn capacity_zero_treated_as_one() {
        let (tx, _rx) = channel();
        let mut mon = BackpressureMonitor::new(SessionId::nil(), 0, tx);
        // any in_flight at all == >= 1 / 1 = saturated.
        assert!(mon.observe(1, 0, Duration::from_secs(0)));
    }

    #[test]
    fn dropped_frame_count_reaches_event_payload() {
        let (tx, mut rx) = channel();
        let mut mon = BackpressureMonitor::new(SessionId::nil(), 10, tx);
        mon.observe(10, 42, Duration::from_secs(1));
        let evt = rx.try_recv().expect("event");
        match evt {
            Event::CaptureDegraded { dropped_frames, .. } => assert_eq!(dropped_frames, 42),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
