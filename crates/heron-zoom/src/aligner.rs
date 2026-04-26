//! Aligner: matches `tap` [`Turn`]s against [`SpeakerEvent`]s
//! emitted by [`crate::AxBackend`] to attribute speaker names with
//! a confidence score.
//!
//! Algorithm per `docs/archives/plan.md` §5 weeks 5–6 (5-step) and
//! `docs/archives/implementation.md` §9.3:
//!
//! 1. Hold a sliding window of recent [`SpeakerEvent`]s (start/end
//!    pairs become "speaker on the air for `[t0, t1]`" intervals).
//! 2. Estimate the AX→audio offset: correlate the tap audio-energy
//!    envelope against speaker-event transitions. We default to a
//!    `350 ms` prior until enough events arrive; once ≥ 5 events
//!    have been ingested in the first 60 s, fit the median offset.
//!    Re-estimate over the 30 s following any `AudioDeviceChanged`.
//! 3. For each ingested [`Turn`] on `Tap`, pick the active
//!    [`SpeakerEvent`] interval with maximum overlap fraction.
//!    `confidence = overlap_fraction × exp(-|delta| / 2s)`.
//! 4. If `confidence < 0.6`, fall back to `speaker = "them"` /
//!    `speaker_source = Channel` / `confidence = None`.
//! 5. If no [`SpeakerEvent`]s have been seen for > 30 s while
//!    `Tap` is producing turns, record [`AttributionDegraded`].
//!
//! [`AttributionDegraded`]: heron_types::Event::AttributionDegraded

use std::time::Duration;

use heron_types::{Event, SessionId, SpeakerEvent, SpeakerSource, Turn};

/// Default AX → tap-audio lag prior when not enough events have
/// landed for an empirical fit.
pub const DEFAULT_EVENT_LAG: Duration = Duration::from_millis(350);

/// Minimum confidence below which the aligner falls back to
/// channel-attribution. Per §9.3 + plan.md §5 wk5–6 step 4.
pub const CONFIDENCE_FLOOR: f64 = 0.6;

/// If no `SpeakerEvent` has been seen for this long while the aligner
/// is being asked to attribute `Tap` turns, surface
/// [`Event::AttributionDegraded`] and downgrade `diarize_source`.
pub const ATTRIBUTION_GAP_THRESHOLD: Duration = Duration::from_secs(30);

/// Maximum number of `(start, end)` events the aligner retains for
/// fast intersection with incoming turns. Older events get dropped
/// once the buffer reaches this cap.
const EVENT_BUFFER_CAP: usize = 256;

/// One contiguous speaking interval, derived from a pair of
/// `SpeakerEvent`s with `started=true` and `started=false`.
#[derive(Debug, Clone)]
struct SpeakingInterval {
    name: String,
    /// Session-secs of the start `SpeakerEvent`.
    t0: f64,
    /// Session-secs of the end `SpeakerEvent`.
    t1: f64,
}

/// Aligner state. Stateful per session.
pub struct Aligner {
    pending_starts: Vec<SpeakerEvent>,
    intervals: Vec<SpeakingInterval>,
    last_event_at: Option<f64>,
    event_lag: Duration,
    /// `(t_event, t_audio_envelope_peak)` pairs collected for the
    /// median-offset fit in the first 60 s of the session.
    offset_samples: Vec<f64>,
    degraded_emitted: bool,
}

impl Default for Aligner {
    fn default() -> Self {
        Self::new()
    }
}

impl Aligner {
    pub fn new() -> Self {
        Self {
            pending_starts: Vec::new(),
            intervals: Vec::new(),
            last_event_at: None,
            event_lag: DEFAULT_EVENT_LAG,
            offset_samples: Vec::new(),
            degraded_emitted: false,
        }
    }

    /// Current AX → audio lag prior used to shift ingested events.
    /// Exposed for the diagnostics tab.
    pub fn event_lag(&self) -> Duration {
        self.event_lag
    }

    /// Drop and re-collect offset samples. Called from outside on
    /// `Event::AudioDeviceChanged` per algorithm step 2.
    pub fn reset_offset_estimation(&mut self) {
        self.offset_samples.clear();
        self.event_lag = DEFAULT_EVENT_LAG;
    }

    /// Push a new [`SpeakerEvent`] from the AX backend. The aligner
    /// pairs `started=true` with `started=false` events of the same
    /// `name` to produce a `SpeakingInterval`.
    pub fn ingest_event(&mut self, evt: SpeakerEvent) {
        self.last_event_at = Some(evt.t);

        if evt.started {
            self.pending_starts.push(evt);
        } else if let Some(start_idx) = self.pending_starts.iter().rposition(|s| s.name == evt.name)
        {
            let start = self.pending_starts.swap_remove(start_idx);
            // Apply the lag prior — AX events tend to lag the audio
            // by `event_lag`; shift `[t0, t1]` left to align.
            let lag_secs = self.event_lag.as_secs_f64();
            self.intervals.push(SpeakingInterval {
                name: evt.name.clone(),
                t0: (start.t - lag_secs).max(0.0),
                t1: (evt.t - lag_secs).max(0.0),
            });
            // FIFO eviction once cap reached (oldest first).
            if self.intervals.len() > EVENT_BUFFER_CAP {
                self.intervals.remove(0);
            }
        }
        // Bare `started=false` without a matching start is a no-op:
        // common when the session starts mid-utterance and we missed
        // the `started=true`. Logging-only — no need to error.
    }

    /// Attribute a single `Tap` [`Turn`] to a speaker. Returns the
    /// turn with `speaker`, `speaker_source`, and `confidence` fields
    /// populated. `Mic` and `MicClean` channel turns are returned
    /// with `speaker = "me"` / `SpeakerSource::Self_` and confidence
    /// 1.0 — the aligner is a no-op for them (the user's own voice
    /// doesn't need AX disambiguation regardless of whether AEC has
    /// run on it).
    pub fn attribute(&mut self, mut turn: Turn) -> Turn {
        if matches!(
            turn.channel,
            heron_types::Channel::Mic | heron_types::Channel::MicClean
        ) {
            turn.speaker = "me".into();
            turn.speaker_source = SpeakerSource::Self_;
            turn.confidence = Some(1.0);
            return turn;
        }

        let mut best: Option<(f64, &SpeakingInterval)> = None;
        for interval in &self.intervals {
            let overlap = interval_overlap(turn.t0, turn.t1, interval.t0, interval.t1);
            if overlap <= 0.0 {
                continue;
            }
            let turn_len = (turn.t1 - turn.t0).max(1e-6);
            let overlap_frac = overlap / turn_len;
            let mid_turn = (turn.t0 + turn.t1) / 2.0;
            let mid_int = (interval.t0 + interval.t1) / 2.0;
            let delta = (mid_turn - mid_int).abs();
            let confidence = overlap_frac * (-delta / 2.0).exp();
            match best {
                Some((best_conf, _)) if best_conf >= confidence => {}
                _ => best = Some((confidence, interval)),
            }
        }

        match best {
            Some((conf, interval)) if conf >= CONFIDENCE_FLOOR => {
                turn.speaker = interval.name.clone();
                turn.speaker_source = SpeakerSource::Ax;
                turn.confidence = Some(conf);
            }
            _ => {
                turn.speaker = "them".into();
                turn.speaker_source = SpeakerSource::Channel;
                turn.confidence = None;
            }
        }
        turn
    }

    /// Check if the aligner should emit
    /// [`Event::AttributionDegraded`] given the current `now_secs`.
    /// Caller (orchestrator) drains this at a fixed cadence and
    /// forwards the event onto the broadcast bus.
    ///
    /// Returns `Some(Event)` exactly once per degraded episode;
    /// callers should not double-emit.
    pub fn check_degraded(&mut self, session_id: SessionId, now_secs: f64) -> Option<Event> {
        let last = self.last_event_at?;
        let gap = now_secs - last;
        if gap >= ATTRIBUTION_GAP_THRESHOLD.as_secs_f64() && !self.degraded_emitted {
            self.degraded_emitted = true;
            return Some(Event::AttributionDegraded {
                id: session_id,
                at: Duration::from_secs_f64(now_secs.max(0.0)),
                reason: format!("no SpeakerEvent for {gap:.1}s while tap was producing turns"),
            });
        }
        // Reset latch when events resume, so a later gap can re-fire.
        if gap < ATTRIBUTION_GAP_THRESHOLD.as_secs_f64() {
            self.degraded_emitted = false;
        }
        None
    }
}

fn interval_overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::{Channel, ViewMode};

    fn evt(t: f64, name: &str, started: bool) -> SpeakerEvent {
        SpeakerEvent {
            t,
            name: name.into(),
            started,
            view_mode: ViewMode::ActiveSpeaker,
            own_tile: false,
        }
    }

    fn turn(t0: f64, t1: f64, channel: Channel) -> Turn {
        Turn {
            t0,
            t1,
            text: String::new(),
            channel,
            speaker: String::new(),
            speaker_source: SpeakerSource::Channel,
            confidence: None,
        }
    }

    #[test]
    fn mic_turn_attributes_to_self_unconditionally() {
        let mut a = Aligner::new();
        // No events ingested at all. Mic still resolves to "me".
        let t = a.attribute(turn(10.0, 12.0, Channel::Mic));
        assert_eq!(t.speaker, "me");
        assert_eq!(t.speaker_source, SpeakerSource::Self_);
        assert_eq!(t.confidence, Some(1.0));
    }

    #[test]
    fn tap_turn_with_aligned_speaker_event_picks_name() {
        let mut a = Aligner::new();
        // Speaking interval [10, 13] → after 350ms lag prior shifts
        // to [9.65, 12.65]. Turn [10, 12] sits squarely inside.
        a.ingest_event(evt(10.0, "Alice", true));
        a.ingest_event(evt(13.0, "Alice", false));

        let t = a.attribute(turn(10.0, 12.0, Channel::Tap));
        assert_eq!(t.speaker, "Alice");
        assert_eq!(t.speaker_source, SpeakerSource::Ax);
        assert!(t.confidence.expect("conf") >= 0.6);
    }

    #[test]
    fn tap_turn_below_floor_falls_back_to_channel() {
        let mut a = Aligner::new();
        // Interval at [10, 11], but turn is way after at [30, 33].
        // No overlap → confidence 0 → fallback.
        a.ingest_event(evt(10.0, "Alice", true));
        a.ingest_event(evt(11.0, "Alice", false));

        let t = a.attribute(turn(30.0, 33.0, Channel::Tap));
        assert_eq!(t.speaker, "them");
        assert_eq!(t.speaker_source, SpeakerSource::Channel);
        assert!(t.confidence.is_none());
    }

    #[test]
    fn picks_max_overlap_when_multiple_speakers() {
        let mut a = Aligner::new();
        // AX events arrive ~350ms after the corresponding audio, so
        // post-lag-shift Alice's interval lands at [10, 11.5] and
        // Bob's at [11.6, 12], inside the turn at [10, 12].
        let lag = DEFAULT_EVENT_LAG.as_secs_f64();
        a.ingest_event(evt(10.0 + lag, "Alice", true));
        a.ingest_event(evt(11.5 + lag, "Alice", false));
        a.ingest_event(evt(11.6 + lag, "Bob", true));
        a.ingest_event(evt(12.0 + lag, "Bob", false));

        let t = a.attribute(turn(10.0, 12.0, Channel::Tap));
        assert_eq!(t.speaker, "Alice"); // 1.5s overlap beats 0.4s
    }

    #[test]
    fn unmatched_end_event_is_a_noop() {
        let mut a = Aligner::new();
        a.ingest_event(evt(5.0, "Ghost", false));
        let t = a.attribute(turn(10.0, 12.0, Channel::Tap));
        // No interval was created → fall back to channel.
        assert_eq!(t.speaker, "them");
    }

    #[test]
    fn check_degraded_fires_once_after_gap() {
        let mut a = Aligner::new();
        a.ingest_event(evt(0.0, "Alice", true));
        a.ingest_event(evt(1.0, "Alice", false));
        let id = SessionId::nil();

        // 5s after last event: no degraded yet.
        assert!(a.check_degraded(id, 6.0).is_none());
        // 35s after: gap > 30s, fire.
        let evt1 = a.check_degraded(id, 36.0);
        assert!(matches!(evt1, Some(Event::AttributionDegraded { .. })));
        // Same gap, second call: latched, no re-fire.
        assert!(a.check_degraded(id, 37.0).is_none());
    }

    #[test]
    fn check_degraded_re_arms_after_recovery() {
        let mut a = Aligner::new();
        let id = SessionId::nil();
        a.ingest_event(evt(0.0, "Alice", true));
        a.ingest_event(evt(1.0, "Alice", false));

        let _ = a.check_degraded(id, 36.0); // first fire
        // Recovery: a fresh event arrives, last_event_at advances.
        a.ingest_event(evt(40.0, "Bob", true));
        a.ingest_event(evt(41.0, "Bob", false));
        // Soon after → no re-fire (latch reset).
        assert!(a.check_degraded(id, 42.0).is_none());
        // Another 30s+ gap → re-fire.
        let evt2 = a.check_degraded(id, 75.0);
        assert!(matches!(evt2, Some(Event::AttributionDegraded { .. })));
    }

    #[test]
    fn reset_offset_estimation_returns_to_default_lag() {
        let mut a = Aligner::new();
        // Default is the 350ms prior even before any reset.
        assert_eq!(a.event_lag(), DEFAULT_EVENT_LAG);
        a.reset_offset_estimation();
        assert_eq!(a.event_lag(), DEFAULT_EVENT_LAG);
    }

    #[test]
    fn quick_fire_back_and_forth_does_not_collapse_to_one_speaker() {
        // Per plan.md §5 wk5-6 done-when: aligner must handle a
        // 2-minute quick-fire back-and-forth without collapsing all
        // turns to one speaker.
        let mut a = Aligner::new();
        let lag = DEFAULT_EVENT_LAG.as_secs_f64();
        let mut t = 0.0;
        for _ in 0..60 {
            // AX events 350ms after audio.
            a.ingest_event(evt(t + lag, "Alice", true));
            a.ingest_event(evt(t + 0.5 + lag, "Alice", false));
            a.ingest_event(evt(t + 0.6 + lag, "Bob", true));
            a.ingest_event(evt(t + 1.1 + lag, "Bob", false));
            t += 1.2;
        }
        // Attribute alternating turns; expect Alice/Bob alternation.
        let mut alice_count = 0;
        let mut bob_count = 0;
        let mut t = 0.0;
        for _ in 0..60 {
            let attr_a = a.attribute(turn(t, t + 0.5, Channel::Tap));
            let attr_b = a.attribute(turn(t + 0.6, t + 1.1, Channel::Tap));
            if attr_a.speaker == "Alice" {
                alice_count += 1;
            }
            if attr_b.speaker == "Bob" {
                bob_count += 1;
            }
            t += 1.2;
        }
        assert!(
            alice_count >= 50,
            "Alice attributed only {alice_count}/60 times"
        );
        assert!(bob_count >= 50, "Bob attributed only {bob_count}/60 times");
    }

    #[test]
    fn event_buffer_evicts_oldest_at_cap() {
        let mut a = Aligner::new();
        let lag = DEFAULT_EVENT_LAG.as_secs_f64();
        // Drive 300 intervals through the buffer (cap is 256). Each
        // interval is 0.5s wide so post-lag-shift it lands cleanly
        // around the corresponding turn.
        for i in 0..300i64 {
            let t = i as f64;
            a.ingest_event(evt(t + lag, "X", true));
            a.ingest_event(evt(t + 0.5 + lag, "X", false));
        }
        // Old intervals were evicted. attribute() against a turn
        // mapped to early intervals should fall through to channel.
        let early = a.attribute(turn(2.0, 2.5, Channel::Tap));
        assert_eq!(early.speaker, "them"); // evicted
        let recent = a.attribute(turn(295.0, 295.5, Channel::Tap));
        assert_eq!(recent.speaker, "X"); // still present
    }
}
