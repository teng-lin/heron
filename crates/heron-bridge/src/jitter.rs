//! Pure reorder / jitter buffer for [`crate::PcmFrame`].
//!
//! Driver-side capture timestamps are not monotonic in practice:
//! Recall WebSocket frames arrive over the network, Attendee live
//! PCM crosses an OS boundary, and native callbacks can interleave
//! across threads. The realtime backend wants frames in
//! capture-time order so the LLM doesn't see scrambled speech.
//! [`JitterBuffer`] is the shape that buys us that ordering — a
//! capture-time-keyed map with explicit late-drop and overflow
//! policy, no clock, no tokio.
//!
//! ## Why pure
//!
//! The bridge's async machinery (mpsc channels, tokio tasks) lives
//! one layer up. This module is the *model*: caller drives it with
//! [`JitterBuffer::insert`] / [`JitterBuffer::pop_oldest`] and an
//! explicit `captured_at_micros` on every frame. Pure code is
//! testable without a runtime and reusable in offline tools (vault
//! replay, fixture generation).
//!
//! ## Eviction policy
//!
//! On overflow we drop the **newest of all** `max_frames + 1`
//! candidates (existing frames + the incoming). If the incoming is
//! itself the newest — a future-outlier the consumer hasn't reached
//! yet — we drop it instead of mutating the map. This biases toward
//! retaining older audio that the consumer is about to pop, and
//! refuses to evict a soon-to-be-popped frame in favor of an outlier
//! that may not even be in the right time window. The cost is
//! dropping audio further out in the future, which the upstream
//! pipeline will retry-send or reconstruct from the backend's own
//! jitter handling.
//!
//! `max_frames == 0` is a degenerate config; every insert returns
//! [`InsertOutcome::Overflow`] without buffering, rather than
//! panicking.
//!
//! ## Late-drop policy
//!
//! [`JitterBuffer`] tracks a high-watermark in capture-time micros.
//! On insert, frames with `captured_at_micros < watermark` are
//! dropped as too late. After a successful insert the watermark
//! advances to `max(watermark, captured_at_micros - late_drop_micros)`.
//! `late_drop_micros == 0` disables the policy (watermark stays at
//! 0 → nothing is ever late) — useful for offline replay where
//! ordering matters but lateness doesn't.
//!
//! ## Duplicate handling
//!
//! Two frames with the same `captured_at_micros` collide on the
//! map key. First-write-wins: the second insert returns
//! [`InsertOutcome::DroppedDuplicate`]. The driver shouldn't
//! produce duplicates in normal operation; if it does, retaining
//! the earlier one preserves whatever upstream invariant produced
//! that timestamp first.
//!
//! ```
//! use heron_bridge::{AudioChannel, PcmFrame};
//! use heron_bridge::jitter::{JitterBuffer, JitterConfig};
//!
//! let mut jb = JitterBuffer::new(JitterConfig::default());
//! let frame = |t| PcmFrame {
//!     samples: vec![],
//!     captured_at_micros: t,
//!     channel: AudioChannel::MeetingIn,
//! };
//!
//! // Frames arrive out of order…
//! jb.insert(frame(30));
//! jb.insert(frame(10));
//! jb.insert(frame(20));
//!
//! // …but pop in capture-time order.
//! assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(10));
//! assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(20));
//! assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(30));
//! assert!(jb.pop_oldest().is_none());
//! ```

use std::collections::BTreeMap;

use crate::PcmFrame;

/// Tunables for [`JitterBuffer`]. `Copy` so the bridge can stash a
/// snapshot in its config struct without lifetime juggling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterConfig {
    /// Max number of frames buffered. Insert beyond this evicts the
    /// newest frame and returns [`InsertOutcome::Overflow`].
    pub max_frames: usize,
    /// Frames with `captured_at_micros < watermark` on insert are
    /// dropped as too late. `0` disables the late-drop policy.
    pub late_drop_micros: u64,
}

impl Default for JitterConfig {
    fn default() -> Self {
        // 64 frames at 20 ms each = ~1.28 s of audio, comfortably
        // beyond the 80 ms / 200 ms jitter thresholds in
        // `crate::health` without unbounded memory. Late-drop off
        // by default so callers opt in once they've measured their
        // own latency budget.
        Self {
            max_frames: 64,
            late_drop_micros: 0,
        }
    }
}

/// What [`JitterBuffer::insert`] did with the supplied frame. The
/// bridge logs / counters branch on this so an operator can see why
/// a frame disappeared.
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Frame stored; will surface from a future
    /// [`JitterBuffer::pop_oldest`].
    Buffered,
    /// Frame's `captured_at_micros` was below the watermark; not
    /// stored. The realtime backend has already moved past this
    /// point in capture time.
    DroppedLate,
    /// A frame with the same `captured_at_micros` is already
    /// buffered; first-write-wins. Not stored.
    DroppedDuplicate,
    /// Buffer was at capacity (or `max_frames == 0`). Either the
    /// newest existing frame was evicted to make room for an older
    /// incoming, or the incoming itself was the newest of all
    /// candidates and was dropped. Both branches return `Overflow` so
    /// the bridge can log + count the pressure event without
    /// branching on the eviction direction.
    Overflow,
}

/// Capture-time-keyed reorder buffer. Pure model, no clock.
///
/// Keyed by `captured_at_micros` with [`BTreeMap`] for `O(log n)`
/// insert and `O(log n)` pop-min via [`BTreeMap::pop_first`]. A
/// `BinaryHeap<Reverse<…>>` would give `O(log n)` insert + `O(1)`
/// peek but doesn't give us the duplicate-key detection we need
/// without a separate set. The map's first-write-wins semantics
/// fall out for free.
#[derive(Debug)]
pub struct JitterBuffer {
    config: JitterConfig,
    /// Capture-time micros → frame at that timestamp. Sorted by
    /// key; `pop_first` gives the oldest.
    frames: BTreeMap<u64, PcmFrame>,
    /// Monotone "everything before this is too late" line in
    /// capture-time micros. Advances on insert; never regresses.
    watermark_micros: u64,
}

impl JitterBuffer {
    pub fn new(config: JitterConfig) -> Self {
        Self {
            config,
            frames: BTreeMap::new(),
            watermark_micros: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Current high-watermark used for late-drop. Exposed for tests
    /// and for diagnostics surfaces that want to show how far past
    /// the buffer's late-drop line a frame fell.
    pub fn watermark_micros(&self) -> u64 {
        self.watermark_micros
    }

    /// Insert a frame. Returns the outcome; the buffer advances its
    /// watermark to `max(watermark, captured_at_micros - late_drop_micros)`
    /// on a successful insert.
    pub fn insert(&mut self, frame: PcmFrame) -> InsertOutcome {
        // `max_frames == 0` is a degenerate config — there's no slot
        // for the frame to live in. Drop on insert and report
        // Overflow so the caller sees that the configured cap is
        // unusable. Without this short-circuit the rest of the body
        // would briefly violate the cap (insert then evict-someday).
        if self.config.max_frames == 0 {
            return InsertOutcome::Overflow;
        }

        // Late-drop check: only consult the watermark when the
        // policy is on. With `late_drop_micros == 0` the watermark
        // stays at 0 and ancient timestamps are never rejected.
        if self.config.late_drop_micros > 0 && frame.captured_at_micros < self.watermark_micros {
            return InsertOutcome::DroppedLate;
        }

        // Duplicate check via the map's API. Manual lookup before
        // insert keeps us from clobbering the earlier frame
        // (first-write-wins).
        if self.frames.contains_key(&frame.captured_at_micros) {
            return InsertOutcome::DroppedDuplicate;
        }

        // Overflow branch: evict the newest of all `max_frames + 1`
        // candidates (existing frames + the incoming). Bias is to
        // keep older audio that the consumer is about to drain. If
        // the incoming is itself the newest, drop it instead of
        // mutating the map — that preserves the soon-to-be-popped
        // existing-newest frame which would otherwise be lost.
        // Watermark advance happens AFTER this branch so a dropped
        // future-outlier doesn't poison the late-drop window.
        if self.frames.len() >= self.config.max_frames {
            let existing_newest = self.frames.last_key_value().map(|(k, _)| *k).unwrap_or(0);
            if frame.captured_at_micros >= existing_newest {
                // Incoming is the newest of all candidates — drop it.
                return InsertOutcome::Overflow;
            }
            // Incoming is older than the newest existing; evict the
            // newest existing and store the incoming.
            self.frames.pop_last();
            self.frames.insert(frame.captured_at_micros, frame.clone());
            self.advance_watermark(frame.captured_at_micros);
            return InsertOutcome::Overflow;
        }

        let captured_at = frame.captured_at_micros;
        self.frames.insert(captured_at, frame);
        self.advance_watermark(captured_at);
        InsertOutcome::Buffered
    }

    fn advance_watermark(&mut self, captured_at_micros: u64) {
        // Skip entirely when the policy is off — leaving the
        // watermark at its initial 0 is what makes ancient
        // timestamps insertable. Saturating_sub so a small
        // `captured_at_micros` doesn't underflow when
        // `late_drop_micros` is large.
        if self.config.late_drop_micros == 0 {
            return;
        }
        let candidate = captured_at_micros.saturating_sub(self.config.late_drop_micros);
        if candidate > self.watermark_micros {
            self.watermark_micros = candidate;
        }
    }

    /// Pop the frame with the smallest `captured_at_micros`, if any.
    pub fn pop_oldest(&mut self) -> Option<PcmFrame> {
        self.frames.pop_first().map(|(_key, frame)| frame)
    }

    /// Drain all frames in capture-time order. Useful for shutdown,
    /// where we want to flush whatever's queued before tearing the
    /// bridge down.
    pub fn drain(&mut self) -> Vec<PcmFrame> {
        // `std::mem::take` swaps in an empty map; cheaper than
        // collecting via `pop_first` in a loop because we don't
        // re-balance the tree on every pop.
        let taken = std::mem::take(&mut self.frames);
        taken.into_values().collect()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::AudioChannel;

    fn frame(captured_at_micros: u64) -> PcmFrame {
        PcmFrame {
            samples: vec![],
            captured_at_micros,
            channel: AudioChannel::MeetingIn,
        }
    }

    fn frame_with_samples(captured_at_micros: u64, samples: Vec<i16>) -> PcmFrame {
        PcmFrame {
            samples,
            captured_at_micros,
            channel: AudioChannel::MeetingIn,
        }
    }

    #[test]
    fn empty_buffer_pop_returns_none() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert!(jb.pop_oldest().is_none());
    }

    #[test]
    fn in_order_inserts_pop_in_order() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.insert(frame(10)), InsertOutcome::Buffered);
        assert_eq!(jb.insert(frame(20)), InsertOutcome::Buffered);
        assert_eq!(jb.insert(frame(30)), InsertOutcome::Buffered);
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(10));
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(20));
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(30));
    }

    #[test]
    fn out_of_order_inserts_pop_in_capture_time_order() {
        // The whole reason the buffer exists. Insert 30, 10, 20 →
        // pop 10, 20, 30.
        let mut jb = JitterBuffer::new(JitterConfig::default());
        jb.insert(frame(30));
        jb.insert(frame(10));
        jb.insert(frame(20));
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(10));
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(20));
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(30));
    }

    #[test]
    fn duplicate_timestamp_drops_second() {
        // First-write-wins: the original samples must survive even
        // though the second insert carried different audio.
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(
            jb.insert(frame_with_samples(20, vec![1, 2, 3])),
            InsertOutcome::Buffered
        );
        assert_eq!(
            jb.insert(frame_with_samples(20, vec![9, 9, 9])),
            InsertOutcome::DroppedDuplicate
        );
        let popped = jb.pop_oldest().expect("first frame should still be there");
        assert_eq!(popped.samples, vec![1, 2, 3]);
    }

    #[test]
    fn late_arrival_dropped_when_watermark_advanced() {
        // late_drop_micros=100 means "stay within 100 µs of newest."
        // Insert at t=1000 → watermark advances to 900. A frame at
        // t=500 is 400 µs before the line and must be dropped.
        let mut jb = JitterBuffer::new(JitterConfig {
            max_frames: 64,
            late_drop_micros: 100,
        });
        assert_eq!(jb.insert(frame(1_000)), InsertOutcome::Buffered);
        assert_eq!(jb.watermark_micros(), 900);
        assert_eq!(jb.insert(frame(500)), InsertOutcome::DroppedLate);
        // Late frame must not have been stored.
        assert_eq!(jb.len(), 1);
    }

    #[test]
    fn late_drop_zero_disables_policy() {
        // With the policy off the watermark stays at 0, so any
        // timestamp — even one arriving after a far-future frame —
        // is buffered. Pin so a future "always advance the
        // watermark" change surfaces here.
        let mut jb = JitterBuffer::new(JitterConfig {
            max_frames: 64,
            late_drop_micros: 0,
        });
        assert_eq!(jb.insert(frame(1_000_000)), InsertOutcome::Buffered);
        assert_eq!(jb.watermark_micros(), 0);
        assert_eq!(jb.insert(frame(1)), InsertOutcome::Buffered);
        assert_eq!(jb.len(), 2);
    }

    #[test]
    fn overflow_evicts_newest_keeps_oldest() {
        // max_frames=2. Insert 10, 30, then 20: third insert is
        // Overflow; eviction policy drops the newest existing
        // entry (30), keeps oldest (10), and stores the new (20).
        // Pop order: 10, then 20.
        let mut jb = JitterBuffer::new(JitterConfig {
            max_frames: 2,
            late_drop_micros: 0,
        });
        assert_eq!(jb.insert(frame(10)), InsertOutcome::Buffered);
        assert_eq!(jb.insert(frame(30)), InsertOutcome::Buffered);
        assert_eq!(jb.insert(frame(20)), InsertOutcome::Overflow);
        assert_eq!(jb.len(), 2);
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(10));
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(20));
        assert!(jb.pop_oldest().is_none());
    }

    #[test]
    fn overflow_drops_incoming_when_it_is_the_newest() {
        // max_frames=2. Insert 10, 20, then 1000: the incoming 1000
        // is the newest of all three candidates. Eviction policy
        // keeps older frames the consumer is about to drain, so we
        // drop the incoming and report Overflow. Pop order remains
        // 10, 20 — the future-outlier is rejected, not stored.
        let mut jb = JitterBuffer::new(JitterConfig {
            max_frames: 2,
            late_drop_micros: 0,
        });
        assert_eq!(jb.insert(frame(10)), InsertOutcome::Buffered);
        assert_eq!(jb.insert(frame(20)), InsertOutcome::Buffered);
        assert_eq!(jb.insert(frame(1_000)), InsertOutcome::Overflow);
        assert_eq!(jb.len(), 2);
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(10));
        assert_eq!(jb.pop_oldest().map(|f| f.captured_at_micros), Some(20));
    }

    #[test]
    fn overflow_drops_incoming_does_not_advance_watermark() {
        // The watermark must not be poisoned by a future-outlier
        // we just rejected via Overflow. Pin so a future change
        // that moves the advance call up surfaces here.
        let mut jb = JitterBuffer::new(JitterConfig {
            max_frames: 1,
            late_drop_micros: 100,
        });
        // Insert 500 → watermark = 400.
        assert_eq!(jb.insert(frame(500)), InsertOutcome::Buffered);
        assert_eq!(jb.watermark_micros(), 400);
        // Insert 1_000_000 → buffer full and incoming is newest →
        // dropped via Overflow; watermark must NOT advance to ~999900.
        assert_eq!(jb.insert(frame(1_000_000)), InsertOutcome::Overflow);
        assert_eq!(jb.watermark_micros(), 400);
    }

    #[test]
    fn max_frames_zero_drops_every_insert() {
        // Degenerate config but we shouldn't panic. Every insert
        // returns Overflow and len stays 0. Pin so a future
        // refactor that moves the cap check after the insert
        // surfaces here.
        let mut jb = JitterBuffer::new(JitterConfig {
            max_frames: 0,
            late_drop_micros: 0,
        });
        assert_eq!(jb.insert(frame(10)), InsertOutcome::Overflow);
        assert_eq!(jb.insert(frame(20)), InsertOutcome::Overflow);
        assert_eq!(jb.len(), 0);
        assert!(jb.is_empty());
    }

    #[test]
    fn len_and_is_empty_track_state() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.len(), 0);
        assert!(jb.is_empty());
        jb.insert(frame(10));
        assert_eq!(jb.len(), 1);
        assert!(!jb.is_empty());
        jb.insert(frame(20));
        assert_eq!(jb.len(), 2);
        jb.pop_oldest();
        assert_eq!(jb.len(), 1);
        jb.pop_oldest();
        assert!(jb.is_empty());
    }

    #[test]
    fn drain_returns_in_capture_order_and_empties() {
        // Shutdown path: flush everything, sorted, leaving the
        // buffer empty.
        let mut jb = JitterBuffer::new(JitterConfig::default());
        jb.insert(frame(30));
        jb.insert(frame(10));
        jb.insert(frame(20));
        let drained = jb.drain();
        let timestamps: Vec<u64> = drained.iter().map(|f| f.captured_at_micros).collect();
        assert_eq!(timestamps, vec![10, 20, 30]);
        assert_eq!(jb.len(), 0);
        assert!(jb.is_empty());
    }

    #[test]
    fn watermark_does_not_regress() {
        // Insert 1000 → watermark = 900. Inserting an earlier
        // timestamp must NOT pull the watermark backwards even when
        // the earlier frame is itself buffered (it's above 900).
        let mut jb = JitterBuffer::new(JitterConfig {
            max_frames: 64,
            late_drop_micros: 100,
        });
        jb.insert(frame(1_000));
        assert_eq!(jb.watermark_micros(), 900);
        // 950 is above the watermark so it's accepted; the
        // resulting candidate (950 - 100 = 850) is below 900 and
        // must not regress the watermark.
        assert_eq!(jb.insert(frame(950)), InsertOutcome::Buffered);
        assert_eq!(jb.watermark_micros(), 900);
    }

    #[test]
    fn default_config_has_sane_values() {
        // Pin the documented defaults so a future tweak surfaces
        // here instead of silently shifting buffer behavior.
        let cfg = JitterConfig::default();
        assert_eq!(cfg.max_frames, 64);
        assert_eq!(cfg.late_drop_micros, 0);
    }
}
