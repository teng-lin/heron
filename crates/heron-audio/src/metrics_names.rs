//! Audio-pipeline metric names. Foundation conventions live in
//! `crates/heron-metrics` and the rules are documented in
//! `docs/observability.md` — every name here is asserted by
//! [`tests::all_names_pass_validation`] to flunk the workspace test
//! suite if a future rename drifts the convention.
//!
//! Co-emitted with the existing `tracing::warn!` / event-bus signals
//! in this crate per the foundation rule: metrics for dashboards,
//! tracing for human-readable logs. Removing or replacing the
//! tracing call sites is **not** part of this crate's contract.

/// Counter incremented every time the audio pipeline drops a frame
/// because the SPSC ring is full. Wired off the `dropped` atomic
/// the realtime callbacks in [`crate::mic_capture`] and
/// [`crate::process_tap`] bump, observed by their consumer tasks.
///
/// No labels. Per-channel (mic vs. tap) breakdowns are deferred
/// because today the dashboard answer is "is the pipeline dropping
/// frames at all?" — both channels feeding the same counter is the
/// intended signal. A future `channel` label can be added once we
/// know which side dominates the dropped-frames bucket.
pub const AUDIO_FRAMES_DROPPED_TOTAL: &str = "audio_frames_dropped_total";

/// Counter incremented exactly once per saturation episode — every
/// time [`crate::BackpressureMonitor::observe`] crosses the
/// [`crate::SATURATION_THRESHOLD`] from "draining" to "saturated".
/// The recovery edge (saturated → draining) does NOT bump the
/// counter; that's a separate signal we don't expose today.
///
/// No labels. The matching `Event::CaptureDegraded` carries the
/// human-readable reason on the bus; the counter is purely "how
/// many backpressure episodes has this daemon seen", which is the
/// dashboard question.
pub const AUDIO_BACKPRESSURE_EPISODES_TOTAL: &str = "audio_backpressure_episodes_total";

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_metrics::validate_metric_name;

    #[test]
    fn all_names_pass_validation() {
        for name in [
            AUDIO_FRAMES_DROPPED_TOTAL,
            AUDIO_BACKPRESSURE_EPISODES_TOTAL,
        ] {
            validate_metric_name(name)
                .unwrap_or_else(|e| panic!("metric name '{name}' violates convention: {e}"));
        }
    }
}
