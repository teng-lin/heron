//! Pure-logic state machine for [`crate::BotState`].
//!
//! Mirrors `heron_types::RecordingFsm` (the v1 disclosure-banner FSM)
//! in shape: typed events drive transitions, invalid transitions
//! return [`TransitionError`] without mutating state, and terminal
//! states reject every further event.
//!
//! Why split this out from the [`crate::MeetingBotDriver`] trait:
//! - Vendor drivers (Recall, Attendee, native Zoom) all need to drive
//!   the same FSM. Centralizing the legality rules here means a new
//!   driver can't accidentally invent a "skip disclosure" transition.
//! - The FSM is pure synchronous Rust; the trait surface is async.
//!   Tests of the legality table don't need a tokio runtime.
//! - Spec invariants 5, 7, 8, 11 (no-speech-without-`InMeeting`,
//!   singleton, persona-required, kick-out semantics) are enforced
//!   at this layer rather than scattered across drivers.
//!
//! ## State diagram (per spec §3)
//!
//! ```text
//!                       on_persona_loaded         on_tts_ready
//!  init ── on_create ──► loading_persona ─────► tts_warming ──► joining
//!                                                                 │
//!                                              on_join_accepted   ▼
//!                                              ┌─── disclosing ◄─ │
//!                                              │       │          │
//!                                  on_disclosure_acked│           │
//!                                              ▼       │          │
//!                                          in_meeting  │          │
//!                                            │   ▲     │          │
//!                                  on_conn_  │   │ on_reconnected │
//!                                      lost  │   │                │
//!                                            ▼   │                │
//!                                       reconnecting               │
//!                                            │                    │
//!                                  on_leave  │                    │
//!                                            ▼                    │
//!                                         leaving                 │
//!                                            │                    │
//!                                            ▼                    │
//!                                        completed                │
//!                                                                 │
//!                                       on_disclosure_objected ───┘
//!                                       on_join_rejected
//!                                       on_host_ended
//!                                       on_ejected{reason}
//!                                       on_vendor_error{e}
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{BotState, EjectReason};

/// Events the orchestrator delivers to the FSM. One per real-world
/// transition trigger — the FSM itself doesn't generate events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BotEvent {
    /// `MeetingBotDriver::bot_create` was called and validated. Drives
    /// `init → loading_persona`.
    Create,
    /// Persona TTS voice + system prompt loaded. Drives
    /// `loading_persona → tts_warming`.
    PersonaLoaded,
    /// First TTS sample synthesized; warm-pool ready. Drives
    /// `tts_warming → joining`.
    TtsReady,
    /// Vendor / native client confirmed admission. Drives
    /// `joining → disclosing`.
    JoinAccepted,
    /// Vendor / host refused admission. Drives `joining → ejected`
    /// with [`EjectReason::AdmissionRefused`] (or whatever the caller
    /// supplies).
    JoinRejected { reason: EjectReason },
    /// Disclosure utterance was spoken AND no objection arrived
    /// within the configured window. Drives `disclosing → in_meeting`.
    DisclosureAcked,
    /// A participant matched an objection-pattern regex within the
    /// objection window. Drives `disclosing → leaving`.
    DisclosureObjected,
    /// User pressed "leave"; orchestrator is closing the bot
    /// gracefully. Drives `in_meeting | reconnecting → leaving`.
    LeaveRequested,
    /// Host ended the meeting; bot exits without speaking goodbye.
    /// Drives `* (live) → host_ended`.
    HostEnded,
    /// Platform ejected the bot (host kicked, recording denied,
    /// policy violation). Drives `* (live) → ejected`.
    Ejected { reason: EjectReason },
    /// Connection dropped mid-meeting. Drives
    /// `in_meeting → reconnecting`.
    ConnectionLost,
    /// Connection re-established. Drives
    /// `reconnecting → in_meeting`.
    Reconnected,
    /// `bot_leave` finished cleanly. Drives `leaving → completed`.
    LeaveFinalized,
    /// Vendor/network error that's NOT an explicit eject. Drives
    /// `* (live) → failed`.
    Failed { error: String },
}

/// Returned by [`BotFsm::on_event`] when the event is illegal in the
/// current state. The FSM doesn't mutate on `Err`, so the caller can
/// inspect + retry without a state-rollback dance.
#[derive(Debug, Error, Clone, PartialEq)]
#[error("transition rejected: event {event:?} is illegal from state {current:?}")]
pub struct TransitionError {
    pub current: BotState,
    pub event: BotEvent,
}

/// Pure-logic FSM. Cheap to construct (`Default::default()` lands in
/// [`BotState::Init`]) and cheap to clone for snapshot/diff.
#[derive(Debug, Clone)]
pub struct BotFsm {
    state: BotState,
}

impl Default for BotFsm {
    fn default() -> Self {
        Self {
            state: BotState::Init,
        }
    }
}

impl BotFsm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &BotState {
        &self.state
    }

    /// `true` once the bot has reached a terminal state and will
    /// reject every further event. Spec invariants 5 + 9: no
    /// post-terminal speech, no post-terminal recording.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            BotState::Completed
                | BotState::Failed { .. }
                | BotState::Ejected { .. }
                | BotState::HostEnded
        )
    }

    /// `true` only in [`BotState::InMeeting`]. Spec Invariant 5: speech-
    /// control calls (`speak`, `cancel`, etc.) are accepted only when
    /// this returns `true`. The orchestrator's policy layer should
    /// gate every utterance on this predicate.
    pub fn can_speak(&self) -> bool {
        matches!(self.state, BotState::InMeeting)
    }

    /// Drive a transition. On `Ok`, the new state is in `self`; on
    /// `Err`, `self` is unchanged so the caller can either retry or
    /// fail out without a manual rollback.
    pub fn on_event(&mut self, event: BotEvent) -> Result<&BotState, TransitionError> {
        if self.is_terminal() {
            return Err(TransitionError {
                current: self.state.clone(),
                event,
            });
        }
        let next = self.next_state(&event)?;
        self.state = next;
        Ok(&self.state)
    }

    fn next_state(&self, event: &BotEvent) -> Result<BotState, TransitionError> {
        let next = match (&self.state, event) {
            (BotState::Init, BotEvent::Create) => BotState::LoadingPersona,
            (BotState::LoadingPersona, BotEvent::PersonaLoaded) => BotState::TtsWarming,
            (BotState::TtsWarming, BotEvent::TtsReady) => BotState::Joining,
            (BotState::Joining, BotEvent::JoinAccepted) => BotState::Disclosing,
            (BotState::Joining, BotEvent::JoinRejected { reason }) => BotState::Ejected {
                reason: reason.clone(),
            },
            (BotState::Disclosing, BotEvent::DisclosureAcked) => BotState::InMeeting,
            (BotState::Disclosing, BotEvent::DisclosureObjected) => BotState::Leaving,
            (BotState::InMeeting, BotEvent::LeaveRequested) => BotState::Leaving,
            (BotState::InMeeting, BotEvent::ConnectionLost) => BotState::Reconnecting,
            (BotState::Reconnecting, BotEvent::Reconnected) => BotState::InMeeting,
            (BotState::Reconnecting, BotEvent::LeaveRequested) => BotState::Leaving,
            (BotState::Leaving, BotEvent::LeaveFinalized) => BotState::Completed,

            // Universal "live → terminal" transitions. `is_terminal`
            // already gates the post-terminal case, so these only
            // apply to live states.
            (live, BotEvent::HostEnded) if !is_terminal_state(live) => BotState::HostEnded,
            (live, BotEvent::Ejected { reason }) if !is_terminal_state(live) => BotState::Ejected {
                reason: reason.clone(),
            },
            (live, BotEvent::Failed { error }) if !is_terminal_state(live) => BotState::Failed {
                error: error.clone(),
            },
            _ => {
                return Err(TransitionError {
                    current: self.state.clone(),
                    event: event.clone(),
                });
            }
        };
        Ok(next)
    }
}

fn is_terminal_state(state: &BotState) -> bool {
    matches!(
        state,
        BotState::Completed
            | BotState::Failed { .. }
            | BotState::Ejected { .. }
            | BotState::HostEnded
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn drive(events: &[BotEvent]) -> BotFsm {
        let mut fsm = BotFsm::new();
        for e in events {
            fsm.on_event(e.clone()).expect("drive transition");
        }
        fsm
    }

    #[test]
    fn happy_path_init_to_in_meeting() {
        let fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
        ]);
        assert_eq!(*fsm.state(), BotState::InMeeting);
        assert!(fsm.can_speak(), "InMeeting must permit speech");
        assert!(!fsm.is_terminal());
    }

    #[test]
    fn graceful_leave_from_in_meeting_reaches_completed() {
        let fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
            BotEvent::LeaveRequested,
            BotEvent::LeaveFinalized,
        ]);
        assert_eq!(*fsm.state(), BotState::Completed);
        assert!(fsm.is_terminal());
        assert!(!fsm.can_speak(), "Completed must NOT permit speech");
    }

    #[test]
    fn disclosure_objection_routes_to_leaving_then_completed() {
        let fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureObjected,
            BotEvent::LeaveFinalized,
        ]);
        // Disclosure objected → Leaving → Completed. The bot honored
        // the participant's objection by exiting cleanly.
        assert_eq!(*fsm.state(), BotState::Completed);
    }

    #[test]
    fn connection_loss_then_reconnect_returns_to_in_meeting() {
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
        ]);
        assert!(fsm.can_speak());
        fsm.on_event(BotEvent::ConnectionLost).expect("conn lost");
        assert_eq!(*fsm.state(), BotState::Reconnecting);
        // Spec invariant 5: must NOT permit speech while reconnecting,
        // since the agent's audio wouldn't be heard.
        assert!(!fsm.can_speak(), "Reconnecting must NOT permit speech");
        fsm.on_event(BotEvent::Reconnected).expect("recon");
        assert_eq!(*fsm.state(), BotState::InMeeting);
        assert!(fsm.can_speak());
    }

    #[test]
    fn reconnecting_can_be_short_circuited_by_user_leave() {
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
            BotEvent::ConnectionLost,
        ]);
        fsm.on_event(BotEvent::LeaveRequested)
            .expect("leave from reconnecting");
        assert_eq!(*fsm.state(), BotState::Leaving);
    }

    #[test]
    fn host_ended_during_in_meeting_lands_in_terminal_host_ended() {
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
        ]);
        fsm.on_event(BotEvent::HostEnded).expect("host ended");
        assert_eq!(*fsm.state(), BotState::HostEnded);
        assert!(fsm.is_terminal());
    }

    #[test]
    fn host_ended_during_pre_meeting_states_also_terminal() {
        // Per spec: HostEnded is a "live → terminal" transition. The
        // bot can absorb it from any pre-terminal state including
        // Joining (the host closed the room before we got admitted).
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
        ]);
        fsm.on_event(BotEvent::HostEnded).expect("host ended early");
        assert_eq!(*fsm.state(), BotState::HostEnded);
    }

    #[test]
    fn join_rejected_lands_in_ejected_with_carried_reason() {
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
        ]);
        fsm.on_event(BotEvent::JoinRejected {
            reason: EjectReason::AdmissionRefused,
        })
        .expect("rejected");
        assert!(matches!(
            fsm.state(),
            BotState::Ejected {
                reason: EjectReason::AdmissionRefused
            }
        ));
    }

    #[test]
    fn ejected_during_in_meeting_carries_reason() {
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
        ]);
        fsm.on_event(BotEvent::Ejected {
            reason: EjectReason::HostRemoved,
        })
        .expect("ejected");
        assert!(matches!(
            fsm.state(),
            BotState::Ejected {
                reason: EjectReason::HostRemoved
            }
        ));
    }

    #[test]
    fn vendor_failure_lands_in_failed_with_carried_message() {
        let mut fsm = drive(&[BotEvent::Create]);
        fsm.on_event(BotEvent::Failed {
            error: "websocket disconnected with 1011".into(),
        })
        .expect("vendor fail");
        assert!(matches!(fsm.state(), BotState::Failed { error } if error.contains("1011")));
    }

    #[test]
    fn cannot_speak_until_in_meeting() {
        // Spec Invariant 5: enumerate every pre-meeting state and
        // assert can_speak() is false. Catches a regression where a
        // future variant accidentally pattern-matches as "live."
        let states = [
            (BotEvent::Create, "loading_persona"),
            (BotEvent::PersonaLoaded, "tts_warming"),
            (BotEvent::TtsReady, "joining"),
            (BotEvent::JoinAccepted, "disclosing"),
        ];
        let mut fsm = BotFsm::new();
        assert!(!fsm.can_speak(), "init must NOT permit speech");
        for (ev, name) in states {
            fsm.on_event(ev).expect("transition");
            assert!(!fsm.can_speak(), "{name} must NOT permit speech");
        }
        // Now finally we're allowed.
        fsm.on_event(BotEvent::DisclosureAcked).expect("acked");
        assert!(fsm.can_speak());
    }

    #[test]
    fn illegal_event_returns_err_without_mutating_state() {
        let mut fsm = BotFsm::new();
        let before = fsm.state().clone();
        let err = fsm
            .on_event(BotEvent::DisclosureAcked)
            .expect_err("init→acked is illegal");
        assert_eq!(err.current, before);
        assert_eq!(*fsm.state(), before, "state must not have moved");
    }

    #[test]
    fn terminal_states_reject_every_further_event() {
        for terminal_event in [
            BotEvent::HostEnded,
            BotEvent::Ejected {
                reason: EjectReason::PolicyViolation,
            },
            BotEvent::Failed { error: "x".into() },
        ] {
            let mut fsm = drive(&[BotEvent::Create]);
            fsm.on_event(terminal_event.clone()).expect("transition");
            assert!(fsm.is_terminal());
            // Every further event must error.
            let err = fsm
                .on_event(BotEvent::PersonaLoaded)
                .expect_err("post-terminal must reject");
            assert_eq!(err.event, BotEvent::PersonaLoaded);
        }
    }

    #[test]
    fn terminal_states_reject_their_own_re_entry() {
        // A second `LeaveFinalized` after Completed should NOT be a
        // no-op: the FSM should error so a buggy driver that fires
        // duplicate events doesn't silently mask a real bug.
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
            BotEvent::LeaveRequested,
            BotEvent::LeaveFinalized,
        ]);
        let err = fsm
            .on_event(BotEvent::LeaveFinalized)
            .expect_err("double finalize");
        assert!(matches!(err.current, BotState::Completed));
    }

    #[test]
    fn cannot_skip_persona_load() {
        let mut fsm = BotFsm::new();
        fsm.on_event(BotEvent::Create).expect("create");
        // Trying to jump directly from LoadingPersona to Joining
        // without TtsReady is illegal.
        let err = fsm.on_event(BotEvent::JoinAccepted).expect_err("skip");
        assert!(matches!(err.current, BotState::LoadingPersona));
    }

    #[test]
    fn cannot_skip_disclosure() {
        let mut fsm = drive(&[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
        ]);
        // Spec Invariant 6: no silent bot. Skipping disclosure
        // straight to InMeeting is rejected.
        let err = fsm
            .on_event(BotEvent::Reconnected)
            .expect_err("Reconnected from Disclosing is illegal");
        assert!(matches!(err.current, BotState::Disclosing));
    }

    #[test]
    fn fsm_clone_yields_independent_snapshot() {
        let mut a = drive(&[BotEvent::Create, BotEvent::PersonaLoaded]);
        let b = a.clone();
        a.on_event(BotEvent::TtsReady).expect("advance a");
        assert_eq!(*a.state(), BotState::Joining);
        assert_eq!(
            *b.state(),
            BotState::TtsWarming,
            "snapshot should not have moved with `a`"
        );
    }

    #[test]
    fn bot_event_round_trips_via_serde() {
        // The wire format matters because driver impls (Recall webhook,
        // native callbacks) deserialize remote events into BotEvent.
        // Pin the snake_case + tag scheme so a future rename surfaces
        // here rather than at the vendor edge.
        let cases = [
            BotEvent::Create,
            BotEvent::JoinRejected {
                reason: EjectReason::AdmissionRefused,
            },
            BotEvent::Failed { error: "x".into() },
        ];
        for ev in cases {
            let json = serde_json::to_string(&ev).expect("serialize");
            let back: BotEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(ev, back);
        }
    }
}
