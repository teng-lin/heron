//! Recall.ai `status_changes` → heron [`BotState`] projection.
//!
//! Recall's REST `GET /api/v1/bot/{id}/` returns a chronological array
//! of `{ code, sub_code, message, created_at }` entries. The polling
//! task (see [`super::driver`]) diffs new entries against the last-seen
//! count and projects each into either:
//!
//! - a [`BotEvent`] driven through the [`BotFsm`] (preferred — keeps
//!   transition legality centralized); or
//! - a direct terminal-state synthesis when Recall reports a fatal
//!   path that doesn't have a clean FSM event mapping.
//!
//! Per [`docs/archives/spike-findings.md`](../../../../docs/archives/spike-findings.md)
//! §"Major API-shape discovery", the REST surface returns codes
//! WITHOUT the `bot.` prefix the webhook docs use. This module
//! normalizes both forms — a `bot.` prefix is stripped on read so
//! downstream code only sees one shape.

use crate::{BotEvent, BotState, EjectReason};

/// Outcome of projecting one `status_changes` entry. The polling task
/// either drives the FSM (`Event`) or, when Recall reports a fatal
/// state that the FSM can't reach via a single event, synthesizes the
/// terminal state directly (`Terminal`). Returning `None` means
/// "ignore this entry" — Recall publishes some intermediate codes
/// (e.g. `recording_done`) that don't carry an FSM-meaningful
/// transition by themselves.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Projection {
    /// Drive the FSM with this event.
    Event(BotEvent),
    /// Synthesize the terminal state directly. Used when Recall
    /// collapses several FSM hops into one transition (e.g. a
    /// `fatal/bot_kicked_from_call` while the bot was still
    /// `LoadingPersona`).
    Terminal(BotState),
    /// Ignored — log only.
    Ignore,
}

/// Project a single Recall `status_changes` entry. `code` and
/// `sub_code` are matched against the live values observed during the
/// 2026-04-26 spike, plus the [Recall sub-codes
/// reference](https://docs.recall.ai/docs/sub-codes).
///
/// # Code list
///
/// REST surface (no `bot.` prefix; webhook surface adds it — both are
/// accepted, see [`normalize_code`]):
///
/// | Recall code              | Projection                                |
/// |--------------------------|-------------------------------------------|
/// | `joining_call`           | [`BotEvent::Create`] *(idempotent prelude)* / `JoinAccepted` ladder |
/// | `in_waiting_room`        | ignored — `joining_call` already moved us to `Joining` |
/// | `in_call_not_recording`  | drive ladder to `InMeeting`               |
/// | `in_call_recording`      | drive ladder to `InMeeting`               |
/// | `recording_done`         | ignored — `done` is the terminal           |
/// | `call_ended` (`sub: bot_received_leave_call`) | ignored — clean self-leave; `done` follows |
/// | `call_ended` (other / no sub_code) | terminal [`BotState::HostEnded`] |
/// | `done`                   | terminal [`BotState::Completed`]          |
/// | `fatal` + sub_code       | terminal [`BotState::Ejected`] / `Failed` |
///
/// Anything else is logged and ignored — Recall periodically introduces
/// new codes and the spike found that swallowing unknowns is safer
/// than panicking.
pub(crate) fn project_status_change(
    code: &str,
    sub_code: Option<&str>,
    message: &str,
) -> Projection {
    match normalize_code(code) {
        // The REST surface emits `joining_call` once the bot has been
        // dispatched and is on its way; `in_waiting_room` may follow
        // before admission. Neither carries an FSM event distinct from
        // the synthetic `JoinAccepted` we'll fire on the next code.
        // The polling task already moved the FSM to `Joining` at
        // `bot_create` time via the synthetic `Create → PersonaLoaded
        // → TtsReady` ladder, so we ignore both.
        "joining_call" | "in_waiting_room" => Projection::Ignore,

        // Recording started. This is the canonical "we are in the
        // meeting" signal. The FSM needs `JoinAccepted` then
        // `DisclosureAcked` to land in `InMeeting`; the polling task
        // fires those in sequence.
        "in_call_not_recording" | "in_call_recording" => Projection::Event(BotEvent::JoinAccepted),

        // Recall emits `recording_done` before `done`; it's a marker
        // that the recording artifact is available, not a state change.
        "recording_done" => Projection::Ignore,

        // `call_ended` carries the disambiguator between "we left"
        // and "host ended the meeting". Per the spike: `sub_code:
        // bot_received_leave_call` means WE triggered the end (via
        // `bot_leave`); any other sub_code (including `None`) means
        // the host did. Recall follows `call_ended` with `done`, but
        // `done` itself doesn't carry the cause — so we resolve the
        // distinction here. The driver's terminal-state guard
        // ignores the redundant `done` once we've published
        // `HostEnded`.
        "call_ended" => match sub_code {
            Some("bot_received_leave_call") => Projection::Ignore,
            _ => Projection::Terminal(BotState::HostEnded),
        },

        // Terminal — graceful end via our own `leave_call` (the
        // host-ended case is intercepted at `call_ended` above so
        // we never reach `done` without having already published a
        // terminal). Defaulting to `Completed` keeps the dashboard
        // honest: an unattributed `done` came from a clean exit.
        "done" => Projection::Terminal(BotState::Completed),

        // Terminal — error path. Sub_code carries the granular reason.
        "fatal" => Projection::Terminal(project_fatal(sub_code, code, message)),

        // Unknown code — log via the caller and ignore. The spike
        // emphasized that swallowing unknowns is safer than panicking;
        // Recall periodically introduces new codes.
        _ => Projection::Ignore,
    }
}

/// Strip the optional `bot.` prefix the webhook surface uses but the
/// REST surface omits. Pinned by tests in this module so a future
/// mismatch surfaces here rather than at the vendor edge.
pub(crate) fn normalize_code(code: &str) -> &str {
    code.strip_prefix("bot.").unwrap_or(code)
}

/// Map a `fatal` entry to the most precise [`BotState`] we can. The
/// sub_code list is from [Recall's documented
/// sub-codes](https://docs.recall.ai/docs/sub-codes) plus the spike's
/// observations. Unknown sub_codes fall back to [`BotState::Failed`]
/// carrying the raw code/sub_code/message — never silently rewritten
/// to `EjectReason::Unknown`, which would hide a real Recall bug.
fn project_fatal(sub_code: Option<&str>, code: &str, message: &str) -> BotState {
    match sub_code {
        // Host-removed family. Recall's documented codes plus the
        // spike's observed variant. Per `docs/sub-codes`, both
        // `bot_kicked_from_call` and the platform-prefixed
        // `*_bot_removed_*` variants land here.
        Some(
            "bot_kicked_from_call"
            | "zoom_bot_removed_from_meeting"
            | "google_meet_bot_removed_from_meeting"
            | "microsoft_teams_bot_removed_from_meeting",
        ) => BotState::Ejected {
            reason: EjectReason::HostRemoved,
        },
        // Recording permission family. The host (or the meeting's
        // policy) blocked recording before or during admission.
        Some(
            "recording_permission_denied"
            | "zoom_recording_permission_denied"
            | "microsoft_teams_recording_permission_denied",
        ) => BotState::Ejected {
            reason: EjectReason::RecordingPermissionDenied,
        },
        // Waiting-room admission was refused (host dismissed the bot
        // from the lobby). The webhook docs spell this multiple ways;
        // the spike observed `recording_permission_not_allowed`.
        Some(
            "recording_permission_not_allowed"
            | "bot_not_admitted_from_waiting_room"
            | "zoom_bot_denied_from_waiting_room"
            | "google_meet_bot_denied_from_lobby"
            | "microsoft_teams_bot_denied_from_lobby",
        ) => BotState::Ejected {
            reason: EjectReason::AdmissionRefused,
        },
        // Heron-policy / vendor-policy outcomes. The bot was
        // disallowed by a policy gate (e.g. plan limit, tenant
        // restriction) rather than by a person.
        Some("blocked_by_policy" | "tenant_policy_violation") => BotState::Ejected {
            reason: EjectReason::PolicyViolation,
        },
        // Any other fatal carries through as `Failed` so the operator
        // sees the raw vendor diagnostic. Don't synthesize an
        // `EjectReason::Unknown` here: the spec calls that out as a
        // last-resort, and a sub_code we don't recognize is more
        // likely a real Recall failure than a silent eject.
        other => BotState::Failed {
            error: format!("{code}/{}: {message}", other.unwrap_or("(no sub_code)")),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rest_codes_normalize_to_unprefixed() {
        // The REST surface drops the `bot.` prefix that the webhook
        // surface uses. Both must hash to the same logical code so the
        // projector matches once.
        assert_eq!(normalize_code("done"), "done");
        assert_eq!(normalize_code("bot.done"), "done");
        assert_eq!(normalize_code("bot.in_call_recording"), "in_call_recording");
    }

    #[test]
    fn joining_codes_are_ignored() {
        // The polling task already drove the FSM into `Joining` at
        // `bot_create` time via the synthetic
        // `Create → PersonaLoaded → TtsReady` ladder. Recall's
        // `joining_call` and `in_waiting_room` carry no further FSM
        // information, so they're ignored.
        assert_eq!(
            project_status_change("joining_call", None, ""),
            Projection::Ignore,
        );
        assert_eq!(
            project_status_change("in_waiting_room", Some("meeting_not_started"), ""),
            Projection::Ignore,
        );
    }

    #[test]
    fn in_call_codes_drive_join_accepted() {
        for code in ["in_call_not_recording", "in_call_recording"] {
            assert_eq!(
                project_status_change(code, None, ""),
                Projection::Event(BotEvent::JoinAccepted),
                "code {code} should drive JoinAccepted",
            );
        }
    }

    #[test]
    fn webhook_form_in_call_recording_is_accepted() {
        // The webhook surface sends `bot.in_call_recording`. Same
        // projection — driver normalizes at the boundary.
        assert_eq!(
            project_status_change("bot.in_call_recording", None, ""),
            Projection::Event(BotEvent::JoinAccepted),
        );
    }

    #[test]
    fn recording_done_is_ignored() {
        // Per spike: `done` is the terminal marker; `recording_done`
        // is just "artifact ready" and must not exit the watch loop.
        assert_eq!(
            project_status_change("recording_done", None, ""),
            Projection::Ignore,
        );
    }

    #[test]
    fn call_ended_with_leave_sub_code_is_ignored() {
        // Spike finding: `bot_received_leave_call` means WE
        // triggered it via `bot_leave`. Ignore here so `done`
        // becomes the `Completed` terminal.
        assert_eq!(
            project_status_change("call_ended", Some("bot_received_leave_call"), ""),
            Projection::Ignore,
        );
    }

    #[test]
    fn call_ended_without_leave_sub_code_is_host_ended() {
        // Anything other than `bot_received_leave_call` (including
        // a missing sub_code) means the host or vendor ended the
        // meeting on us. Project to `HostEnded` so the dashboard
        // distinguishes the case from a clean self-leave.
        assert_eq!(
            project_status_change("call_ended", None, ""),
            Projection::Terminal(BotState::HostEnded),
        );
        assert_eq!(
            project_status_change("call_ended", Some("zoom_meeting_ended"), ""),
            Projection::Terminal(BotState::HostEnded),
        );
    }

    #[test]
    fn done_is_terminal_completed() {
        assert_eq!(
            project_status_change("done", None, ""),
            Projection::Terminal(BotState::Completed),
        );
        assert_eq!(
            project_status_change("bot.done", None, ""),
            Projection::Terminal(BotState::Completed),
        );
    }

    #[test]
    fn fatal_kicked_maps_to_host_removed() {
        let p = project_status_change("fatal", Some("bot_kicked_from_call"), "you got kicked");
        assert_eq!(
            p,
            Projection::Terminal(BotState::Ejected {
                reason: EjectReason::HostRemoved,
            }),
        );
    }

    #[test]
    fn fatal_recording_permission_denied_maps_to_recording_permission_denied() {
        let p = project_status_change(
            "fatal",
            Some("recording_permission_denied"),
            "host blocked recording",
        );
        assert_eq!(
            p,
            Projection::Terminal(BotState::Ejected {
                reason: EjectReason::RecordingPermissionDenied,
            }),
        );
    }

    #[test]
    fn fatal_admission_refused_maps_for_either_documented_sub_code() {
        for sub in [
            "recording_permission_not_allowed",
            "bot_not_admitted_from_waiting_room",
        ] {
            let p = project_status_change("fatal", Some(sub), "lobby denied");
            assert_eq!(
                p,
                Projection::Terminal(BotState::Ejected {
                    reason: EjectReason::AdmissionRefused,
                }),
                "sub_code {sub} should map to AdmissionRefused",
            );
        }
    }

    #[test]
    fn fatal_platform_prefixed_kick_codes_all_map_to_host_removed() {
        for sub in [
            "zoom_bot_removed_from_meeting",
            "google_meet_bot_removed_from_meeting",
            "microsoft_teams_bot_removed_from_meeting",
        ] {
            let p = project_status_change("fatal", Some(sub), "kicked");
            assert_eq!(
                p,
                Projection::Terminal(BotState::Ejected {
                    reason: EjectReason::HostRemoved,
                }),
                "{sub} should map to HostRemoved",
            );
        }
    }

    #[test]
    fn fatal_policy_codes_map_to_policy_violation() {
        for sub in ["blocked_by_policy", "tenant_policy_violation"] {
            let p = project_status_change("fatal", Some(sub), "blocked");
            assert_eq!(
                p,
                Projection::Terminal(BotState::Ejected {
                    reason: EjectReason::PolicyViolation,
                }),
                "{sub} should map to PolicyViolation",
            );
        }
    }

    #[test]
    fn fatal_unknown_sub_code_falls_through_to_failed_with_raw_diagnostic() {
        // A sub_code we don't recognize is more likely a real Recall
        // bug than a silent eject. Carry the raw diagnostic so the
        // operator can correlate against vendor logs.
        let p = project_status_change("fatal", Some("zoom_internal_3127"), "boom");
        match p {
            Projection::Terminal(BotState::Failed { error }) => {
                assert!(error.contains("zoom_internal_3127"), "got: {error}");
                assert!(error.contains("boom"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn fatal_with_no_sub_code_still_carries_message() {
        let p = project_status_change("fatal", None, "websocket disconnect");
        match p {
            Projection::Terminal(BotState::Failed { error }) => {
                assert!(error.contains("websocket"), "got: {error}");
                assert!(error.contains("(no sub_code)"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn unknown_code_is_ignored_not_panicked() {
        // The spike emphasized: swallowing unknowns is safer than
        // panicking because Recall periodically introduces new codes.
        assert_eq!(
            project_status_change("brand_new_code_recall_invented", None, ""),
            Projection::Ignore,
        );
    }
}
