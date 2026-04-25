//! Anomaly detection over parsed session summaries.
//!
//! Thresholds are **advisory** — they're tighter than the
//! `docs/implementation.md` §18.2 hard ship criteria so the doctor
//! surfaces drift well before it becomes a blocker. The user can
//! override every threshold via [`Thresholds`] or the CLI flags.
//!
//! In particular, §18.2's ship-blocking gate is "cost > $2 on any
//! meeting <60 min." The default `max_cost_usd: 0.50` here flags an
//! order of magnitude earlier; treat hits as a heads-up, not a
//! release blocker.

use serde::Serialize;

use crate::log_reader::SessionSummaryRecord;

/// Tunable thresholds. Defaults are advisory (see module doc); the
/// CLI exposes them as flags so the user can tighten or loosen per
/// session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// USD per session above which we flag cost as high. The §11.4
    /// calibration target is ~ $0.05; default flags at 10x that. The
    /// §18.2 hard ship gate is $2.00 — leave plenty of head-room.
    pub max_cost_usd: f64,
    /// AX hit rate (0.0–1.0) below which speaker attribution is
    /// considered degraded. The §9 target is 70 % on non-dial-in
    /// fixtures; below 50 % is unambiguously bad.
    pub min_ax_hit_pct: f64,
    /// Ratio of low-confidence turns / total turns. Above this the
    /// transcript needs a human pass before sharing. Not in §18.2;
    /// chosen to flag a session that's mostly low-confidence.
    pub max_low_conf_ratio: f64,
    /// Any non-zero count flags. The audio path drops only under
    /// genuine back-pressure (§7.4); >0 in a clean run is suspicious.
    pub flag_dropped_frames: bool,
    /// Same for device changes — the runtime is supposed to recover
    /// transparently, but the §7.3 manual test exists because edge
    /// cases bite.
    pub flag_device_changes: bool,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            max_cost_usd: 0.50,
            min_ax_hit_pct: 0.50,
            max_low_conf_ratio: 0.30,
            flag_dropped_frames: true,
            flag_device_changes: true,
        }
    }
}

/// One flagged signal. Pinning the `kind` tag in the JSON wire
/// format means a future automation hook (the §16 `heron-doctor`
/// integration) can grep for specific kinds without parsing prose.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnomalyKind {
    HighCost {
        observed_usd: f64,
        threshold_usd: f64,
    },
    LowAxHitRate {
        observed_pct: f64,
        threshold_pct: f64,
    },
    HighLowConfRatio {
        observed: f64,
        threshold: f64,
    },
    DroppedFrames {
        count: u32,
    },
    DeviceChanges {
        count: u32,
    },
}

/// One anomaly attached to its session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Anomaly {
    pub session_id: Option<String>,
    pub kind: AnomalyKind,
}

/// Inspect every record against the supplied thresholds. Returns one
/// [`Anomaly`] per `(session, signal)` flagged. Sessions with no
/// flagged signals contribute nothing.
pub fn detect_anomalies(records: &[SessionSummaryRecord], thresholds: &Thresholds) -> Vec<Anomaly> {
    let mut out = Vec::new();
    for record in records {
        let Some(fields) = record.fields.as_ref() else {
            continue;
        };
        // Tiny closure so each check is one expression rather than
        // five copies of the `Anomaly { session_id: ..., kind }` boilerplate.
        let session_id = record.session_id.clone();
        let mut flag = |kind: AnomalyKind| {
            out.push(Anomaly {
                session_id: session_id.clone(),
                kind,
            });
        };
        if let Some(cost) = fields.summarize_cost_usd
            && cost > thresholds.max_cost_usd
        {
            flag(AnomalyKind::HighCost {
                observed_usd: cost,
                threshold_usd: thresholds.max_cost_usd,
            });
        }
        if let Some(ax) = fields.ax_hit_pct
            && ax < thresholds.min_ax_hit_pct
        {
            flag(AnomalyKind::LowAxHitRate {
                observed_pct: ax,
                threshold_pct: thresholds.min_ax_hit_pct,
            });
        }
        if let (Some(low), Some(total)) = (fields.low_conf_turns, fields.turns_total)
            && total > 0
        {
            let ratio = low as f64 / total as f64;
            if ratio > thresholds.max_low_conf_ratio {
                flag(AnomalyKind::HighLowConfRatio {
                    observed: ratio,
                    threshold: thresholds.max_low_conf_ratio,
                });
            }
        }
        if thresholds.flag_dropped_frames
            && let Some(dropped) = fields.audio_dropped_frames
            && dropped > 0
        {
            flag(AnomalyKind::DroppedFrames { count: dropped });
        }
        if thresholds.flag_device_changes
            && let Some(changes) = fields.device_changes
            && changes > 0
        {
            flag(AnomalyKind::DeviceChanges { count: changes });
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::log_reader::SessionSummaryFields;

    fn record(session: &str, fields: SessionSummaryFields) -> SessionSummaryRecord {
        SessionSummaryRecord {
            log_version: 1,
            ts: None,
            level: Some("INFO".to_owned()),
            session_id: Some(session.to_owned()),
            module: Some("heron_session::summary".to_owned()),
            msg: Some("session complete".to_owned()),
            fields: Some(fields),
        }
    }

    fn clean_fields() -> SessionSummaryFields {
        SessionSummaryFields {
            kind: Some("session_summary".to_owned()),
            duration_secs: Some(1800.0),
            ax_hit_pct: Some(0.85),
            turns_total: Some(200),
            low_conf_turns: Some(10),
            audio_dropped_frames: Some(0),
            device_changes: Some(0),
            summarize_cost_usd: Some(0.03),
            ..SessionSummaryFields::default()
        }
    }

    #[test]
    fn clean_session_yields_no_anomalies() {
        let r = record("clean", clean_fields());
        let out = detect_anomalies(&[r], &Thresholds::default());
        assert!(out.is_empty());
    }

    #[test]
    fn high_cost_flagged() {
        let mut f = clean_fields();
        // Default max_cost_usd is 0.50; flag a session at 1.00.
        f.summarize_cost_usd = Some(1.00);
        let out = detect_anomalies(&[record("expensive", f)], &Thresholds::default());
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].kind, AnomalyKind::HighCost { .. }));
    }

    #[test]
    fn cost_at_threshold_is_not_flagged() {
        // Strict `>` boundary check: a session exactly at the
        // threshold should not be flagged.
        let mut f = clean_fields();
        f.summarize_cost_usd = Some(Thresholds::default().max_cost_usd);
        let out = detect_anomalies(&[record("at-edge", f)], &Thresholds::default());
        assert!(out.is_empty());
    }

    #[test]
    fn low_ax_hit_rate_flagged() {
        let mut f = clean_fields();
        f.ax_hit_pct = Some(0.30);
        let out = detect_anomalies(&[record("bad-ax", f)], &Thresholds::default());
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].kind, AnomalyKind::LowAxHitRate { .. }));
    }

    #[test]
    fn high_low_conf_ratio_flagged() {
        let mut f = clean_fields();
        f.low_conf_turns = Some(80);
        f.turns_total = Some(200);
        let out = detect_anomalies(&[record("noisy", f)], &Thresholds::default());
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].kind, AnomalyKind::HighLowConfRatio { .. }));
    }

    #[test]
    fn zero_total_turns_does_not_divide_by_zero() {
        let mut f = clean_fields();
        f.turns_total = Some(0);
        f.low_conf_turns = Some(0);
        let out = detect_anomalies(&[record("empty", f)], &Thresholds::default());
        assert!(out.is_empty());
    }

    #[test]
    fn dropped_frames_flagged() {
        let mut f = clean_fields();
        f.audio_dropped_frames = Some(5);
        let out = detect_anomalies(&[record("drops", f)], &Thresholds::default());
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].kind,
            AnomalyKind::DroppedFrames { count: 5 }
        ));
    }

    #[test]
    fn flag_dropped_frames_false_silences_signal() {
        let mut f = clean_fields();
        f.audio_dropped_frames = Some(5);
        let thresh = Thresholds {
            flag_dropped_frames: false,
            ..Thresholds::default()
        };
        let out = detect_anomalies(&[record("drops", f)], &thresh);
        assert!(out.is_empty());
    }

    #[test]
    fn device_changes_flagged() {
        let mut f = clean_fields();
        f.device_changes = Some(2);
        let out = detect_anomalies(&[record("flap", f)], &Thresholds::default());
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].kind,
            AnomalyKind::DeviceChanges { count: 2 }
        ));
    }

    #[test]
    fn multiple_signals_in_one_session() {
        let mut f = clean_fields();
        f.summarize_cost_usd = Some(1.0);
        f.ax_hit_pct = Some(0.20);
        f.audio_dropped_frames = Some(3);
        let out = detect_anomalies(&[record("kitchen-sink", f)], &Thresholds::default());
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn missing_fields_does_not_crash() {
        let r = record("partial", SessionSummaryFields::default());
        let out = detect_anomalies(&[r], &Thresholds::default());
        assert!(out.is_empty());
    }

    #[test]
    fn anomaly_serializes_with_kind_tag() {
        let a = Anomaly {
            session_id: Some("s".to_owned()),
            kind: AnomalyKind::DroppedFrames { count: 7 },
        };
        let s = serde_json::to_string(&a).expect("ser");
        assert!(s.contains(r#""kind":"dropped_frames""#));
        assert!(s.contains(r#""count":7"#));
    }
}
