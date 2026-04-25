//! Discrete health verdict for [`crate::BridgeHealth`].
//!
//! The struct exposes three numeric signals — `aec_tracking`,
//! `jitter_ms`, `recent_drops` — but consumers (the §15.4 Tauri
//! diagnostics tab, the `heron-policy` mute decision, the audit
//! log) want a categorical answer: is the bridge healthy enough
//! that the agent should keep speaking, or is it degraded badly
//! enough that we should mute and surface a banner?
//!
//! [`verdict`] is the single oracle. Pure / synchronous so any layer
//! can call it on the hot path of a bridging frame without a
//! channel hop.
//!
//! ## Threshold rationale (per spec §7 Bridge Health)
//!
//! | Signal | Healthy | Degraded | Critical |
//! |---|---|---|---|
//! | `aec_tracking` | `true` | `false` | (any) |
//! | `jitter_ms` | < 80 | 80–200 | > 200 |
//! | `recent_drops` (per second) | 0 | 1–5 | > 5 |
//!
//! - **AEC tracking lost** is `Critical`: without echo cancellation,
//!   the agent's outbound audio feeds back into the meeting input
//!   and the realtime backend may transcribe the agent's own voice
//!   as a participant turn (a self-reply loop).
//! - **80 ms jitter** is the standard speech-quality threshold;
//!   beyond that the listener perceives glitches.
//! - **5 drops/sec** is the threshold above which packet loss
//!   becomes audible (per WebRTC's published thresholds).
//!
//! Worst-of-all-three wins: a bridge with `Healthy` jitter but
//! `Degraded` drops reports `Degraded`.

use crate::BridgeHealth;

/// Categorical verdict the orchestrator + diagnostics tab branch on.
/// Marked `#[non_exhaustive]` so adding a `Recovering` variant in a
/// future minor doesn't break downstream `match`es.
///
/// Doesn't impl `Eq` because the carried `observed_ms` is `f32`,
/// which excludes the trait by design (NaN ≠ NaN). Tests use
/// `matches!(...)` for shape comparison, which avoids the issue.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum HealthVerdict {
    /// Bridge is operating within all thresholds.
    Healthy,
    /// One or more signals have crossed a soft threshold; the
    /// agent can keep speaking but the diagnostics tab should
    /// surface a yellow indicator.
    Degraded { reason: DegradationReason },
    /// AEC has lost tracking; the agent must mute or the realtime
    /// backend will transcribe its own voice as a participant
    /// turn. Diagnostics surfaces a red banner.
    Critical { reason: CriticalReason },
}

/// What pushed the bridge into [`HealthVerdict::Degraded`].
/// Multiple soft thresholds can fire simultaneously; the verdict
/// records the *first* one that crossed (jitter > drops) so audit
/// logs are deterministic.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum DegradationReason {
    /// `jitter_ms` is in the soft band [`JITTER_DEGRADED_MS`,
    /// `JITTER_CRITICAL_MS`].
    Jitter { observed_ms: f32 },
    /// `recent_drops` is in the soft band [1, `DROPS_CRITICAL`].
    PacketLoss { observed_drops: u32 },
}

/// What pushed the bridge into [`HealthVerdict::Critical`]. AEC
/// loss is non-recoverable without external action (driver
/// restart); critical jitter / drops are recoverable when network
/// conditions improve.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum CriticalReason {
    /// `aec_tracking == false`; the agent's outbound audio is
    /// leaking back into MeetingIn.
    AecTrackingLost,
    /// `jitter_ms > JITTER_CRITICAL_MS`, or jitter is `NaN`. NaN
    /// surfaces here rather than getting filtered upstream because
    /// a buggy bridge reporting NaN must mute the agent — falling
    /// through as Healthy would let the agent keep speaking through
    /// broken telemetry.
    JitterCritical { observed_ms: f32 },
    /// `recent_drops > DROPS_CRITICAL`.
    PacketLossCritical { observed_drops: u32 },
}

/// Soft jitter threshold. Beyond this, listeners perceive glitches.
pub const JITTER_DEGRADED_MS: f32 = 80.0;

/// Hard jitter threshold. Beyond this, speech quality is
/// unacceptable and we should mute rather than ship audio.
pub const JITTER_CRITICAL_MS: f32 = 200.0;

/// Soft drops threshold (drops/sec). 1 drop/sec is the floor we
/// flag as "something's happening on the network."
pub const DROPS_DEGRADED: u32 = 1;

/// Hard drops threshold (drops/sec). Beyond this, packet loss is
/// audible per WebRTC's thresholds.
pub const DROPS_CRITICAL: u32 = 5;

/// Single oracle. Worst-of-all-three wins.
///
/// Order of precedence within Critical: AEC loss > jitter > drops.
/// Order within Degraded: jitter > drops. Determinism matters
/// because the diagnostics tab + audit log surface the *reason*,
/// and a reproducible reason is what makes a regression diff-able.
pub fn verdict(health: &BridgeHealth) -> HealthVerdict {
    // AEC loss is the only Critical that doesn't depend on a
    // numeric. Check it first so a bridge with bad jitter AND no
    // AEC reports the AEC loss (which is the actual showstopper).
    if !health.aec_tracking {
        return HealthVerdict::Critical {
            reason: CriticalReason::AecTrackingLost,
        };
    }

    // NaN jitter surfaces as Critical: a bridge reporting NaN means
    // upstream telemetry is broken, and we must mute the agent
    // rather than ship audio through a broken health pipeline.
    // Strict `>` not `>=`: the documented table says > 200 ⇒ Critical,
    // 200 itself stays in the Degraded band.
    if health.jitter_ms.is_nan() || health.jitter_ms > JITTER_CRITICAL_MS {
        return HealthVerdict::Critical {
            reason: CriticalReason::JitterCritical {
                observed_ms: health.jitter_ms,
            },
        };
    }

    if health.recent_drops > DROPS_CRITICAL {
        return HealthVerdict::Critical {
            reason: CriticalReason::PacketLossCritical {
                observed_drops: health.recent_drops,
            },
        };
    }

    if health.jitter_ms >= JITTER_DEGRADED_MS {
        return HealthVerdict::Degraded {
            reason: DegradationReason::Jitter {
                observed_ms: health.jitter_ms,
            },
        };
    }

    if health.recent_drops >= DROPS_DEGRADED {
        return HealthVerdict::Degraded {
            reason: DegradationReason::PacketLoss {
                observed_drops: health.recent_drops,
            },
        };
    }

    HealthVerdict::Healthy
}

impl HealthVerdict {
    /// `true` if the agent should keep speaking. False when AEC
    /// has lost tracking — the realtime backend would transcribe
    /// the agent's own voice as a participant turn (self-reply
    /// loop). Soft Degraded is still safe to speak through.
    pub fn safe_to_speak(&self) -> bool {
        !matches!(self, HealthVerdict::Critical { .. })
    }

    /// `true` when a yellow / red diagnostics indicator is
    /// warranted. False only when fully [`HealthVerdict::Healthy`].
    pub fn needs_attention(&self) -> bool {
        !matches!(self, HealthVerdict::Healthy)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn health(aec: bool, jitter: f32, drops: u32) -> BridgeHealth {
        BridgeHealth {
            aec_tracking: aec,
            jitter_ms: jitter,
            recent_drops: drops,
        }
    }

    #[test]
    fn fully_healthy_signals_report_healthy() {
        assert_eq!(verdict(&health(true, 10.0, 0)), HealthVerdict::Healthy);
    }

    #[test]
    fn aec_loss_is_critical_regardless_of_other_signals() {
        // AEC loss with otherwise-perfect signals → still Critical.
        // Pin the precedence: AEC > jitter > drops.
        let v = verdict(&health(false, 10.0, 0));
        assert!(matches!(
            v,
            HealthVerdict::Critical {
                reason: CriticalReason::AecTrackingLost
            }
        ));
    }

    #[test]
    fn aec_loss_wins_over_jitter_critical() {
        let v = verdict(&health(false, 999.0, 999));
        // Should still be AecTrackingLost, not JitterCritical.
        assert!(matches!(
            v,
            HealthVerdict::Critical {
                reason: CriticalReason::AecTrackingLost
            }
        ));
    }

    #[test]
    fn critical_jitter_is_critical() {
        // Strict `>`: 200 itself is the boundary of the Degraded
        // band; 200.001 trips Critical.
        let v = verdict(&health(true, JITTER_CRITICAL_MS + 0.001, 0));
        match v {
            HealthVerdict::Critical {
                reason: CriticalReason::JitterCritical { observed_ms },
            } => {
                assert!(observed_ms > JITTER_CRITICAL_MS, "got {observed_ms}");
            }
            other => panic!("expected JitterCritical, got {other:?}"),
        }
    }

    #[test]
    fn jitter_at_critical_threshold_is_only_degraded() {
        // Documented table says jitter `> 200` is Critical and
        // `80–200` is Degraded — i.e., 200 is in Degraded. Pin the
        // boundary so a future flip from `>` to `>=` surfaces here.
        let v = verdict(&health(true, JITTER_CRITICAL_MS, 0));
        assert!(matches!(
            v,
            HealthVerdict::Degraded {
                reason: DegradationReason::Jitter { .. }
            }
        ));
    }

    #[test]
    fn nan_jitter_lands_in_critical() {
        // Broken telemetry must not let the agent keep speaking.
        // Verify NaN routes to JitterCritical regardless of other
        // signals.
        let v = verdict(&health(true, f32::NAN, 0));
        assert!(matches!(
            v,
            HealthVerdict::Critical {
                reason: CriticalReason::JitterCritical { .. }
            }
        ));
    }

    #[test]
    fn jitter_just_below_critical_is_degraded() {
        // JITTER_CRITICAL_MS - epsilon must be Degraded, not
        // Critical. Pin so a future "off-by-epsilon" change
        // surfaces here.
        let v = verdict(&health(true, JITTER_CRITICAL_MS - 0.001, 0));
        assert!(matches!(
            v,
            HealthVerdict::Degraded {
                reason: DegradationReason::Jitter { .. }
            }
        ));
    }

    #[test]
    fn critical_drops_are_critical() {
        let v = verdict(&health(true, 0.0, DROPS_CRITICAL + 1));
        match v {
            HealthVerdict::Critical {
                reason: CriticalReason::PacketLossCritical { observed_drops },
            } => {
                assert_eq!(observed_drops, DROPS_CRITICAL + 1);
            }
            other => panic!("expected PacketLossCritical, got {other:?}"),
        }
    }

    #[test]
    fn drops_at_critical_threshold_is_only_degraded() {
        // The threshold is "> CRITICAL", so == CRITICAL stays
        // Degraded. Pin so a future inclusive-vs-exclusive flip
        // surfaces here.
        let v = verdict(&health(true, 0.0, DROPS_CRITICAL));
        assert!(matches!(
            v,
            HealthVerdict::Degraded {
                reason: DegradationReason::PacketLoss { .. }
            }
        ));
    }

    #[test]
    fn jitter_degraded_with_drops_critical_picks_drops() {
        // Both fire critical-or-degraded; precedence within
        // Critical is jitter > drops, so check that drops-Critical
        // wins over jitter-Degraded.
        let v = verdict(&health(true, JITTER_DEGRADED_MS, DROPS_CRITICAL + 5));
        assert!(matches!(
            v,
            HealthVerdict::Critical {
                reason: CriticalReason::PacketLossCritical { .. }
            }
        ));
    }

    #[test]
    fn jitter_critical_with_drops_critical_picks_jitter() {
        // Both Critical; jitter wins per the documented order.
        let v = verdict(&health(true, JITTER_CRITICAL_MS + 0.1, DROPS_CRITICAL + 5));
        assert!(matches!(
            v,
            HealthVerdict::Critical {
                reason: CriticalReason::JitterCritical { .. }
            }
        ));
    }

    #[test]
    fn degraded_jitter_alone() {
        let v = verdict(&health(true, JITTER_DEGRADED_MS + 10.0, 0));
        match v {
            HealthVerdict::Degraded {
                reason: DegradationReason::Jitter { observed_ms },
            } => {
                assert_eq!(observed_ms, JITTER_DEGRADED_MS + 10.0);
            }
            other => panic!("expected Jitter Degraded, got {other:?}"),
        }
    }

    #[test]
    fn degraded_drops_alone() {
        let v = verdict(&health(true, 0.0, 3));
        match v {
            HealthVerdict::Degraded {
                reason: DegradationReason::PacketLoss { observed_drops },
            } => {
                assert_eq!(observed_drops, 3);
            }
            other => panic!("expected PacketLoss Degraded, got {other:?}"),
        }
    }

    #[test]
    fn jitter_degraded_with_drops_degraded_picks_jitter() {
        // Both Degraded; jitter wins per the documented order.
        let v = verdict(&health(true, JITTER_DEGRADED_MS, 3));
        assert!(matches!(
            v,
            HealthVerdict::Degraded {
                reason: DegradationReason::Jitter { .. }
            }
        ));
    }

    #[test]
    fn drops_at_degraded_threshold() {
        // == DROPS_DEGRADED is Degraded.
        let v = verdict(&health(true, 0.0, DROPS_DEGRADED));
        assert!(matches!(
            v,
            HealthVerdict::Degraded {
                reason: DegradationReason::PacketLoss { .. }
            }
        ));
    }

    #[test]
    fn safe_to_speak_predicate() {
        assert!(HealthVerdict::Healthy.safe_to_speak());
        assert!(
            HealthVerdict::Degraded {
                reason: DegradationReason::Jitter { observed_ms: 100.0 }
            }
            .safe_to_speak()
        );
        assert!(
            !HealthVerdict::Critical {
                reason: CriticalReason::AecTrackingLost
            }
            .safe_to_speak()
        );
    }

    #[test]
    fn needs_attention_predicate() {
        assert!(!HealthVerdict::Healthy.needs_attention());
        assert!(
            HealthVerdict::Degraded {
                reason: DegradationReason::Jitter { observed_ms: 100.0 }
            }
            .needs_attention()
        );
        assert!(
            HealthVerdict::Critical {
                reason: CriticalReason::AecTrackingLost
            }
            .needs_attention()
        );
    }

    #[test]
    fn verdict_is_deterministic_for_same_input() {
        // Audit log + diagnostics tab depend on this. Reproducible
        // verdicts let a regression diff against a recorded fixture.
        let h = health(true, 100.0, 2);
        let v1 = verdict(&h);
        let v2 = verdict(&h);
        let v3 = verdict(&h);
        assert_eq!(v1, v2);
        assert_eq!(v2, v3);
    }
}
