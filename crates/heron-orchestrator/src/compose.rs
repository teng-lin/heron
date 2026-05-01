//! v2 live-session composition helpers.
//!
//! Translate the orchestrator's per-capture inputs (a `MeetingId`, the
//! caller's `Platform`, a `Meeting` snapshot, an optional staged
//! `PreMeetingContext`) into the [`LiveSessionStartArgs`] the
//! [`crate::live_session::LiveSessionFactory`] consumes, and produce
//! the v1 LLM prompt preamble from the same staged context so v1 and
//! v2 agents see the same briefing copy. The translation is field-by-
//! field rather than a serde round trip so a missing field on the
//! upstream [`PreMeetingContext`] is a compile error here, not a
//! silent drop.

use heron_bot::{
    AttendeeContext as BotAttendeeContext, BotCreateArgs, DisclosureProfile, PersonaId,
    PreMeetingContext as BotPreMeetingContext,
};
use heron_policy::{EscalationMode, PolicyProfile};
use heron_realtime::{SessionConfig as RealtimeSessionConfig, TurnDetection};
use heron_session::{Meeting, MeetingId, Platform, PreMeetingContext};
use uuid::Uuid;

use crate::live_session::LiveSessionStartArgs;

/// Default disclosure template used when the orchestrator composes
/// the v2 stack itself. The `{user_name}` and `{meeting_title}`
/// placeholders that `render_disclosure` understands are deliberately
/// omitted: the bot driver currently substitutes a literal
/// `"the user"` for `{user_name}` (see
/// `crates/heron-bot/src/recall/mod.rs:391`), so referencing the
/// placeholder would imply a real name will appear when it does
/// not. A persona-authored template lands alongside the persona
/// settings UI; for alpha this is enough to satisfy `bot_create`'s
/// no-empty-disclosure invariant (Spec §4 Invariant 6).
const DEFAULT_DISCLOSURE_TEMPLATE: &str = "Heron is recording and assisting in this meeting.";

/// Default OpenAI Realtime voice. `alloy` is the documented sane
/// default; the orchestrator will surface this as a settings field
/// when persona authoring lands.
const DEFAULT_REALTIME_VOICE: &str = "alloy";

/// Default persona prompt used as the system-prompt prefix when no
/// persona is configured. The persona authoring UI will replace
/// this; for alpha it ensures the realtime backend has a non-empty
/// `system_prompt` (validated by `heron_realtime::validate`).
const DEFAULT_PERSONA_PROMPT: &str = "You are a concise meeting assistant.";

/// Translate the orchestrator's per-capture inputs into the
/// [`LiveSessionStartArgs`] the
/// [`crate::live_session::LiveSessionFactory`] consumes.
///
/// This is the consumer hand-off for the pre-meeting-context gap:
/// when `applied_context` is `Some`, its agenda / attendees /
/// briefing are rendered into the realtime session's system prompt
/// AND threaded through to the bot driver so persona-aware behaviour
/// is available from turn one.
pub(crate) fn build_live_session_start_args(
    meeting_id: MeetingId,
    platform: Platform,
    meeting: &Meeting,
    applied_context: Option<&PreMeetingContext>,
) -> LiveSessionStartArgs {
    let mut bot_context = applied_context
        .map(translate_to_bot_context)
        .unwrap_or_default();

    // Render the system prompt as `<persona>\n\n<context>` when a
    // context is present, else fall back to the persona prompt
    // alone. `heron_bot::render_context` enforces the 48 KiB cap
    // from spec Invariant 10 — on overflow we drop the rendered
    // context (the persona prompt by itself is still a valid
    // session config) and log so an operator can correlate with the
    // attach-context call that staged a too-large payload.
    //
    // Issue #215 finding 4 — when render fails we ALSO reset
    // `bot_context` to default so the oversized payload doesn't
    // get smuggled into `BotCreateArgs.context` and out to the bot
    // driver / Recall on the wire. Without this, the realtime
    // prompt was sanitized but the bot side could still trip the
    // same 48 KiB invariant downstream and fail the live session.
    let rendered_context = match heron_bot::render_context(&bot_context) {
        Ok(rendered) => rendered,
        Err(err) => {
            tracing::warn!(
                meeting_id = %meeting_id,
                error = %err,
                "rendered context exceeds spec budget; dropping context from system prompt and bot args",
            );
            bot_context = BotPreMeetingContext::default();
            String::new()
        }
    };
    let system_prompt = if rendered_context.is_empty() {
        DEFAULT_PERSONA_PROMPT.to_owned()
    } else {
        format!("{DEFAULT_PERSONA_PROMPT}\n\n{rendered_context}")
    };

    // The hint is the closest thing to a meeting URL the
    // orchestrator currently has. EventKit-sourced meetings carry a
    // real URL on `CalendarEvent::meeting_url`; until that flows
    // through `StartCaptureArgs`, we forward the hint and let the
    // bot driver reject malformed inputs at `bot_create` time.
    let meeting_url = meeting.title.clone().unwrap_or_default();

    LiveSessionStartArgs {
        meeting_id,
        bot: BotCreateArgs {
            meeting_url,
            // PersonaId::nil() would be rejected by RecallDriver
            // (Spec §4 Invariant 8). Mint a fresh per-capture id
            // until persona authoring lands; identical to how the
            // existing `live_session::tests::start_args` builds one.
            persona_id: PersonaId::now_v7(),
            disclosure: DisclosureProfile {
                text_template: DEFAULT_DISCLOSURE_TEMPLATE.to_owned(),
                objection_patterns: Vec::new(),
                objection_timeout_secs: 30,
                re_announce_on_join: false,
            },
            context: bot_context,
            metadata: serde_json::json!({
                "meeting_id": meeting_id.to_string(),
                "platform": format!("{platform:?}"),
            }),
            // Minted fresh per `start_capture` because the
            // orchestrator does not retry. If retry is added later,
            // this MUST become a stable value derived from
            // `meeting_id` so the bot driver's vendor-side
            // idempotency holds (Spec §11 Invariant 14).
            idempotency_key: Uuid::now_v7(),
        },
        realtime: RealtimeSessionConfig {
            system_prompt,
            tools: Vec::new(),
            turn_detection: TurnDetection {
                vad_threshold: 0.5,
                prefix_padding_ms: 300,
                silence_duration_ms: 500,
                interrupt_response: true,
                // OpenAiRealtime requires this to be `false` so the
                // controller mints response IDs explicitly. See
                // `crates/heron-realtime/src/openai.rs:117-122`.
                auto_create_response: false,
            },
            voice: DEFAULT_REALTIME_VOICE.to_owned(),
        },
        policy: PolicyProfile {
            allow_topics: Vec::new(),
            // Conservative defaults until a settings surface lands.
            // `mute: false` keeps the agent able to speak; deny
            // list is empty (tighter rules belong in user-facing
            // settings, not the orchestrator default). Escalation
            // is `None` because there's no destination configured.
            deny_topics: Vec::new(),
            mute: false,
            escalation: EscalationMode::None,
        },
    }
}

/// The bot driver carries its own typed `PreMeetingContext` shape
/// (subset of the orchestrator-side one — no `prior_decisions`).
/// Translate field-by-field rather than re-using a serde round trip
/// so a missing field is a compile error, not a silent drop.
fn translate_to_bot_context(ctx: &PreMeetingContext) -> BotPreMeetingContext {
    BotPreMeetingContext {
        agenda: ctx.agenda.clone(),
        attendees_known: ctx
            .attendees_known
            .iter()
            .map(|a| BotAttendeeContext {
                name: a.name.clone(),
                email: a.email.clone(),
                last_seen_in: a.last_seen_in,
                relationship: a.relationship.clone(),
                notes: a.notes.clone(),
            })
            .collect(),
        related_notes: ctx.related_notes.clone(),
        user_briefing: ctx.user_briefing.clone(),
    }
}

/// Render the staged pre-meeting context into a markdown preamble for
/// the v1 LLM summarizer prompt. Mirrors what
/// [`build_live_session_start_args`] does for the v2 system prompt
/// (`heron_bot::render_context` with its 48 KiB cap from spec
/// Invariant 10) so v1 and v2 agents see the same briefing copy when
/// both paths run.
///
/// Returns `None` when no context is staged, when render fails (e.g.
/// the rendered text would exceed the cap), or when the rendered
/// output is empty whitespace. The summarizer prompt template
/// suppresses its `## Pre-meeting context` block on `None`, so capture
/// continues normally either way — context is a hint, not a
/// precondition.
pub(crate) fn pre_meeting_briefing_for_v1(
    applied_context: Option<&PreMeetingContext>,
    meeting_id: MeetingId,
) -> Option<String> {
    let bot_context = translate_to_bot_context(applied_context?);
    match heron_bot::render_context(&bot_context) {
        Ok(rendered) if !rendered.trim().is_empty() => Some(rendered),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(
                meeting_id = %meeting_id,
                error = %err,
                "v1 pre-meeting briefing exceeds spec budget; v1 summary will run without preamble",
            );
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Issue #215 finding 4 — when `heron_bot::render_context`
    //! rejects an oversized [`PreMeetingContext`], the realtime
    //! system prompt was already sanitized but `bot_context` (which
    //! flows into `BotCreateArgs.context` and out to Recall on the
    //! wire) was not. Confirm both sides drop the oversized payload.
    use super::*;
    use heron_bot::MAX_CONTEXT_BYTES;
    use heron_session::{Meeting, MeetingStatus, SummaryLifecycle, TranscriptLifecycle};

    fn empty_meeting(id: MeetingId) -> Meeting {
        Meeting {
            id,
            status: MeetingStatus::Done,
            platform: Platform::Zoom,
            title: Some("Standup".into()),
            calendar_event_id: None,
            started_at: chrono::Utc::now(),
            ended_at: None,
            duration_secs: None,
            participants: Vec::new(),
            transcript_status: TranscriptLifecycle::Pending,
            summary_status: SummaryLifecycle::Pending,
            tags: Vec::new(),
            processing: None,
            action_items: Vec::new(),
        }
    }

    #[test]
    fn build_live_session_drops_oversized_context_from_bot_args() {
        let meeting_id = MeetingId::now_v7();
        let meeting = empty_meeting(meeting_id);
        // Stuff the agenda past the 48 KiB cap so `render_context`
        // returns `ContextError::TooLarge`.
        let oversized = PreMeetingContext {
            agenda: Some("a".repeat(MAX_CONTEXT_BYTES + 4096)),
            attendees_known: Vec::new(),
            related_notes: Vec::new(),
            prior_decisions: Vec::new(),
            user_briefing: None,
        };

        let args =
            build_live_session_start_args(meeting_id, Platform::Zoom, &meeting, Some(&oversized));

        // Realtime prompt must not contain the oversized agenda.
        assert!(
            !args.realtime.system_prompt.contains("aaaaaaa"),
            "realtime system_prompt leaked oversized agenda",
        );
        // Pre-fix: `args.bot.context` would still carry the
        // oversized agenda. The fix resets it to default on render
        // failure, so all fields must be empty.
        assert!(
            args.bot.context.agenda.is_none(),
            "bot.context.agenda leaked through after render failure: {:?}",
            args.bot.context.agenda.as_ref().map(|s| s.len()),
        );
        assert!(args.bot.context.attendees_known.is_empty());
        assert!(args.bot.context.related_notes.is_empty());
        assert!(args.bot.context.user_briefing.is_none());
    }

    #[test]
    fn build_live_session_keeps_in_budget_context() {
        // Sanity: a normal-sized context still flows through to
        // both the realtime prompt and the bot args. (Guards against
        // the fix over-correcting and resetting on success.)
        let meeting_id = MeetingId::now_v7();
        let meeting = empty_meeting(meeting_id);
        let ctx = PreMeetingContext {
            agenda: Some("ship the alpha".to_owned()),
            attendees_known: Vec::new(),
            related_notes: Vec::new(),
            prior_decisions: Vec::new(),
            user_briefing: None,
        };

        let args = build_live_session_start_args(meeting_id, Platform::Zoom, &meeting, Some(&ctx));

        assert!(args.realtime.system_prompt.contains("ship the alpha"));
        assert_eq!(args.bot.context.agenda.as_deref(), Some("ship the alpha"));
    }
}
