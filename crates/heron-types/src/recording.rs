//! Recording-flow state machine per
//! [`docs/implementation.md`](../../../docs/implementation.md) §14.2.
//!
//! Lives in `heron-types` rather than the Tauri shell so the
//! orchestrator + CLI + future test harnesses can drive the same
//! states. The Tauri week-12 work renders banners off the
//! transitions enumerated here.
//!
//! ```text
//! idle ──(hotkey)──► armed
//! armed ──(yes)──► recording
//! armed ──(remind 30s)──► armed-cooldown ──(30s tick)──► armed
//! armed ──(cancel)──► idle
//! recording ──(hotkey or window close)──► transcribing
//! transcribing ──(done)──► summarizing
//! summarizing ──(done|fail)──► idle
//! ```

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Discrete recording-flow states. Each transition is exposed as a
/// dedicated method on [`RecordingFsm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Idle,
    Armed,
    ArmedCooldown,
    Recording,
    Transcribing,
    Summarizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryOutcome {
    Done,
    Failed,
}

/// Cause of an `idle` transition. Useful for the banner copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleReason {
    /// User cancelled while armed (no recording started).
    Cancelled,
    /// Summarize completed successfully.
    SummaryDone,
    /// Summarize errored; the recording is on disk but no `.md`.
    SummaryFailed,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("invalid transition from {from:?} for event {event}")]
    Invalid { from: RecordingState, event: String },
}

/// Cooldown period after a `remind` while armed before the banner
/// re-prompts. Per §14.2 / week 10 plan: 30 seconds.
pub const ARM_COOLDOWN: Duration = Duration::from_secs(30);

/// Stateful FSM. Owns the current [`RecordingState`] and rejects
/// transitions that aren't legal from it.
#[derive(Debug, Clone)]
pub struct RecordingFsm {
    state: RecordingState,
    last_idle_reason: Option<IdleReason>,
}

impl Default for RecordingFsm {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingFsm {
    pub fn new() -> Self {
        Self {
            state: RecordingState::Idle,
            last_idle_reason: None,
        }
    }

    pub fn state(&self) -> RecordingState {
        self.state
    }

    pub fn last_idle_reason(&self) -> Option<IdleReason> {
        self.last_idle_reason
    }

    /// `idle ──(hotkey)──► armed`
    pub fn on_hotkey(&mut self) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::Idle => {
                // Clear any prior IdleReason on the Idle→Armed edge
                // so the banner doesn't show "summary done" while a
                // new flow is being armed.
                self.last_idle_reason = None;
                self.state = RecordingState::Armed;
                Ok(self.state)
            }
            // Hotkey while recording = stop. Per §14.2:
            // "recording ──(hotkey or window close)──► transcribing".
            RecordingState::Recording => {
                self.state = RecordingState::Transcribing;
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "hotkey".into(),
            }),
        }
    }

    /// `armed ──(yes)──► recording`
    pub fn on_yes(&mut self) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::Armed | RecordingState::ArmedCooldown => {
                self.state = RecordingState::Recording;
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "yes".into(),
            }),
        }
    }

    /// `armed ──(remind 30s)──► armed-cooldown`
    pub fn on_remind(&mut self) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::Armed => {
                self.state = RecordingState::ArmedCooldown;
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "remind".into(),
            }),
        }
    }

    /// `armed-cooldown ──(30s tick)──► armed`
    pub fn on_cooldown_tick(&mut self) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::ArmedCooldown => {
                self.state = RecordingState::Armed;
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "cooldown_tick".into(),
            }),
        }
    }

    /// `armed ──(cancel)──► idle`
    pub fn on_cancel(&mut self) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::Armed | RecordingState::ArmedCooldown => {
                self.state = RecordingState::Idle;
                self.last_idle_reason = Some(IdleReason::Cancelled);
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "cancel".into(),
            }),
        }
    }

    /// `recording ──(window close)──► transcribing` — alias for the
    /// hotkey path so callers can wire onto a window-close event
    /// without dispatching through hotkey handling.
    pub fn on_window_close(&mut self) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::Recording => {
                self.state = RecordingState::Transcribing;
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "window_close".into(),
            }),
        }
    }

    /// `transcribing ──(done)──► summarizing`
    pub fn on_transcribe_done(&mut self) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::Transcribing => {
                self.state = RecordingState::Summarizing;
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "transcribe_done".into(),
            }),
        }
    }

    /// `summarizing ──(done|fail)──► idle`
    pub fn on_summary(
        &mut self,
        outcome: SummaryOutcome,
    ) -> Result<RecordingState, TransitionError> {
        match self.state {
            RecordingState::Summarizing => {
                self.state = RecordingState::Idle;
                self.last_idle_reason = Some(match outcome {
                    SummaryOutcome::Done => IdleReason::SummaryDone,
                    SummaryOutcome::Failed => IdleReason::SummaryFailed,
                });
                Ok(self.state)
            }
            other => Err(TransitionError::Invalid {
                from: other,
                event: "summary".into(),
            }),
        }
    }

    /// Whether the FSM is in a state where the user should see a
    /// "recording" UI affordance. Convenience for the banner.
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            RecordingState::Recording | RecordingState::Transcribing | RecordingState::Summarizing
        )
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_idle_to_summary_done() {
        let mut f = RecordingFsm::new();
        assert_eq!(f.state(), RecordingState::Idle);
        assert_eq!(f.on_hotkey().expect("hotkey"), RecordingState::Armed);
        assert_eq!(f.on_yes().expect("yes"), RecordingState::Recording);
        assert!(f.is_active());
        assert_eq!(f.on_hotkey().expect("stop"), RecordingState::Transcribing);
        assert_eq!(
            f.on_transcribe_done().expect("trans_done"),
            RecordingState::Summarizing
        );
        assert_eq!(
            f.on_summary(SummaryOutcome::Done).expect("done"),
            RecordingState::Idle
        );
        assert_eq!(f.last_idle_reason(), Some(IdleReason::SummaryDone));
    }

    #[test]
    fn cooldown_then_re_arms() {
        let mut f = RecordingFsm::new();
        f.on_hotkey().expect("hotkey");
        assert_eq!(
            f.on_remind().expect("remind"),
            RecordingState::ArmedCooldown
        );
        assert_eq!(f.on_cooldown_tick().expect("tick"), RecordingState::Armed);
    }

    #[test]
    fn yes_works_from_cooldown_too() {
        // The user's "yes, record now" answer should take effect
        // even mid-cooldown — we don't want to throw away their
        // consent because the timer happened to be running.
        let mut f = RecordingFsm::new();
        f.on_hotkey().expect("hotkey");
        f.on_remind().expect("remind");
        assert_eq!(
            f.on_yes().expect("yes from cooldown"),
            RecordingState::Recording
        );
    }

    #[test]
    fn cancel_records_idle_reason() {
        let mut f = RecordingFsm::new();
        f.on_hotkey().expect("hotkey");
        assert_eq!(f.on_cancel().expect("cancel"), RecordingState::Idle);
        assert_eq!(f.last_idle_reason(), Some(IdleReason::Cancelled));
    }

    #[test]
    fn summary_failed_path_records_reason() {
        let mut f = RecordingFsm::new();
        f.on_hotkey().expect("hotkey");
        f.on_yes().expect("yes");
        f.on_hotkey().expect("stop");
        f.on_transcribe_done().expect("trans");
        assert_eq!(
            f.on_summary(SummaryOutcome::Failed).expect("fail"),
            RecordingState::Idle
        );
        assert_eq!(f.last_idle_reason(), Some(IdleReason::SummaryFailed));
    }

    #[test]
    fn rejects_invalid_transitions() {
        let mut f = RecordingFsm::new();
        // Idle → yes is not a legal edge (must arm first).
        let err = f.on_yes().expect_err("must error");
        assert!(matches!(err, TransitionError::Invalid { .. }));
        // Idle → summary is not legal.
        assert!(f.on_summary(SummaryOutcome::Done).is_err());
        // Idle → cooldown_tick is not legal.
        assert!(f.on_cooldown_tick().is_err());
    }

    #[test]
    fn window_close_during_recording_acts_like_stop() {
        let mut f = RecordingFsm::new();
        f.on_hotkey().expect("hotkey");
        f.on_yes().expect("yes");
        assert_eq!(
            f.on_window_close().expect("close"),
            RecordingState::Transcribing
        );
    }

    #[test]
    fn window_close_outside_recording_errors() {
        let mut f = RecordingFsm::new();
        let err = f.on_window_close().expect_err("must error");
        assert!(matches!(err, TransitionError::Invalid { .. }));
    }

    #[test]
    fn is_active_only_during_recording_and_post() {
        let mut f = RecordingFsm::new();
        assert!(!f.is_active()); // idle
        f.on_hotkey().expect("hotkey");
        assert!(!f.is_active()); // armed
        f.on_remind().expect("remind");
        assert!(!f.is_active()); // cooldown
        f.on_cooldown_tick().expect("tick");
        f.on_yes().expect("yes");
        assert!(f.is_active()); // recording
        f.on_hotkey().expect("stop");
        assert!(f.is_active()); // transcribing
        f.on_transcribe_done().expect("trans");
        assert!(f.is_active()); // summarizing
        f.on_summary(SummaryOutcome::Done).expect("done");
        assert!(!f.is_active()); // idle
    }

    #[test]
    fn state_serializes_to_snake_case() {
        let s = serde_json::to_string(&RecordingState::ArmedCooldown).expect("serialize");
        assert_eq!(s, r#""armed_cooldown""#);
    }

    #[test]
    fn idle_reason_serializes_to_snake_case() {
        let s = serde_json::to_string(&IdleReason::SummaryFailed).expect("serialize");
        assert_eq!(s, r#""summary_failed""#);
    }
}
