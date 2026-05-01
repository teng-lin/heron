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

use crate::metrics_names::{AUDIO_BACKPRESSURE_EPISODES_TOTAL, AUDIO_FRAMES_DROPPED_TOTAL};

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
    /// Snapshot of the cumulative `dropped_frames` count from the
    /// previous call to [`Self::observe`]. Used to compute the
    /// per-tick delta we feed into the
    /// `audio_frames_dropped_total` counter — the counter is
    /// monotonic per the Prometheus contract, so we increment by
    /// `current - previous` rather than re-recording the absolute
    /// value (which `metrics::counter!::increment` doesn't support
    /// anyway). Initial value is 0; the first observe with a
    /// non-zero `dropped_frames` bumps the counter by exactly that
    /// amount.
    last_dropped_frames: u32,
}

impl BackpressureMonitor {
    pub fn new(session_id: SessionId, capacity: usize, events: broadcast::Sender<Event>) -> Self {
        Self {
            session_id,
            capacity: capacity.max(1),
            saturated: false,
            events,
            last_dropped_frames: 0,
        }
    }

    /// Observe a `(in_flight, dropped_frames, at)` tuple. Emits a
    /// `CaptureDegraded` event the first time saturation is crossed,
    /// and clears the latch once the queue drops below threshold so
    /// a later saturation can re-fire.
    ///
    /// Returns `true` if an event was emitted on this call.
    ///
    /// **Metrics co-emission.** Per `docs/observability.md` and the
    /// foundation rule "metrics for dashboards, tracing/events for
    /// human-readable logs", this method also bumps:
    ///
    /// - [`AUDIO_FRAMES_DROPPED_TOTAL`] by the *delta* between the
    ///   incoming `dropped_frames` and the value seen on the previous
    ///   call. The counter is process-global (no session label) so a
    ///   long-running daemon's running total is the sum across
    ///   sessions, mirroring how Prometheus consumes counters.
    /// - [`AUDIO_BACKPRESSURE_EPISODES_TOTAL`] exactly once per
    ///   saturation transition (false → true), aligned with the
    ///   `CaptureDegraded` event emission. The recovery edge does NOT
    ///   bump the counter — repeated saturate/recover cycles each
    ///   count once on the saturate edge.
    pub fn observe(&mut self, in_flight: usize, dropped_frames: u32, at: Duration) -> bool {
        // Drop-counter delta — the realtime callback's atomic is
        // monotonic per session and the consumer tasks pass it in
        // verbatim. `saturating_sub` defends against the case where
        // the caller passes a smaller value on a subsequent call
        // (a session restart re-using the same monitor — today the
        // consumer tasks never recycle a monitor, but the
        // contract should be robust to that). We update
        // `last_dropped_frames` on **every** observation, not just
        // delta-positive ones — without that, a counter reset
        // (current < last) would leave `last_dropped_frames` pinned
        // at the prior session's high-water mark and silently swallow
        // every observation below it forever.
        let delta = dropped_frames.saturating_sub(self.last_dropped_frames);
        if delta > 0 {
            metrics::counter!(AUDIO_FRAMES_DROPPED_TOTAL).increment(u64::from(delta));
        }
        self.last_dropped_frames = dropped_frames;

        let ratio = in_flight as f32 / self.capacity as f32;
        if !self.saturated && ratio >= SATURATION_THRESHOLD {
            self.saturated = true;
            metrics::counter!(AUDIO_BACKPRESSURE_EPISODES_TOTAL).increment(1);
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

    /// Metric co-emission: a saturation episode bumps
    /// `audio_backpressure_episodes_total` and a non-zero
    /// `dropped_frames` delta bumps `audio_frames_dropped_total`. We
    /// install the process-global Prometheus recorder (idempotent
    /// across the test suite per `heron_metrics::recorder`) and grep
    /// the rendered exposition for both names + values. Mirrors the
    /// shape of `metrics_endpoint_returns_prometheus_exposition_with_bearer`
    /// from `crates/herond/tests/api.rs`.
    ///
    /// **Parallel-test note.** Both metrics are unlabeled
    /// counters, so other tests in this module emit into the same
    /// time series concurrently. We assert `>= N` rather than `== N`
    /// for that reason — exact-equality assertions would flake under
    /// `cargo test`'s default thread-per-test scheduling.
    #[test]
    fn observe_emits_metrics_on_saturation_and_drops() {
        let handle =
            heron_metrics::init_prometheus_recorder().expect("install recorder for metric test");
        let (tx, _rx) = channel();
        let mut mon = BackpressureMonitor::new(SessionId::nil(), 10, tx);

        // Saturate AND record a delta of 7 dropped frames in one call.
        let before = handle.render();
        let before_drops = scrape_total(&before, AUDIO_FRAMES_DROPPED_TOTAL);
        let before_episodes = scrape_total(&before, AUDIO_BACKPRESSURE_EPISODES_TOTAL);

        mon.observe(10, 7, Duration::from_secs(1));

        let after = handle.render();
        let after_drops = scrape_total(&after, AUDIO_FRAMES_DROPPED_TOTAL);
        let after_episodes = scrape_total(&after, AUDIO_BACKPRESSURE_EPISODES_TOTAL);

        // `>= 7` (not `== 7`): another test running in parallel may
        // have bumped this same unlabeled counter between the two
        // snapshots. The assertion that matters is "the
        // delta-positive observation moved the counter by AT LEAST
        // the observed delta" — drift upward from concurrent tests
        // is fine; drift downward / no-movement is the bug we're
        // catching.
        assert!(
            after_drops - before_drops >= 7,
            "saturation observation must bump frames_dropped by at least 7; \
             got delta {} from rendered exposition:\n{after}",
            after_drops - before_drops,
        );
        assert!(
            after_episodes - before_episodes >= 1,
            "saturation edge must bump backpressure_episodes by at least 1; \
             got delta {} from rendered exposition:\n{after}",
            after_episodes - before_episodes,
        );

        // Recovery edge does NOT bump episodes; subsequent saturate
        // does. (Mirrors `re_fires_after_recovery`.)
        mon.observe(3, 7, Duration::from_secs(2)); // recover, no delta
        mon.observe(10, 7, Duration::from_secs(3)); // saturate again
        let after2 = handle.render();
        let after2_episodes = scrape_total(&after2, AUDIO_BACKPRESSURE_EPISODES_TOTAL);
        assert!(
            after2_episodes - before_episodes >= 2,
            "two saturation edges must bump backpressure_episodes by at least 2; \
             got delta {} from rendered exposition:\n{after2}",
            after2_episodes - before_episodes,
        );
    }

    /// Helper: parse the unlabelled `<name> <value>` line out of the
    /// Prometheus exposition body. Returns 0 when the metric isn't
    /// present (the recorder hasn't seen the name yet) so before/after
    /// deltas are well-defined on a cold cache.
    ///
    /// Both `audio_frames_dropped_total` and
    /// `audio_backpressure_episodes_total` are emitted without
    /// labels in this crate, so the line shape is exactly
    /// `<name> <value>` followed by either a newline or a `{` on the
    /// labelled-variant case.
    fn scrape_total(body: &str, name: &str) -> u64 {
        for line in body.lines() {
            // Skip `# TYPE` / `# HELP` annotations which start with
            // `#` and are not value lines.
            if line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix(name) {
                // The unlabelled line shape is "<name> <value>"; a
                // labelled line would be "<name>{...} <value>". We
                // only emit the unlabelled form for these two
                // counters today, so anything starting with `{` is
                // ignored as a future-labelled variant we don't
                // handle here.
                if let Some(val) = rest.strip_prefix(' ')
                    && let Ok(n) = val.trim().parse::<u64>()
                {
                    return n;
                }
            }
        }
        0
    }
}
