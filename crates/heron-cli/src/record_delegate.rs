//! `heron record --daemon` v2 delegation driver.
//!
//! The legacy `heron record` runs the v1 in-process orchestrator. The
//! `--daemon` flag instead reaches the localhost `herond` over the
//! same OpenAPI surface the desktop shell drives, unifying the CLI
//! and GUI session-control surfaces against a single source of truth.
//!
//! Wire shape (mirrors [`crate::daemon`]):
//!
//! - `POST /v1/meetings` — start a manual capture; returns the
//!   freshly-minted meeting envelope.
//! - `GET /v1/events` — SSE projection of the orchestrator event bus;
//!   filtered to the started meeting and printed to stdout one line
//!   per envelope.
//! - `POST /v1/meetings/{id}/end` — sent on user stop (Ctrl-C) or
//!   duration cap; the daemon publishes `meeting.completed` once
//!   finalization drains.
//!
//! The driver lives in the lib (rather than `main.rs`) so the
//! integration suite can hit it directly against a `wiremock`
//! `MockServer`. The `stop` future is injected so tests don't need to
//! send a real SIGINT.

use std::future::Future;
use std::time::Duration;

use heron_session::{
    EventPayload, Meeting, MeetingCompletedData, MeetingId, MeetingOutcome, StartCaptureArgs,
};
use thiserror::Error;

use crate::daemon::{DaemonClient, DaemonError};

/// Why the user-driven stop arm fired. Surfaced to stderr so the
/// operator sees whether the daemon ended on its own (e.g. detector
/// fired `meeting.completed` first) or whether we forced an end.
///
/// Tests that don't want to fire the stop arm pass a future that
/// never resolves (e.g. [`std::future::pending::<StopReason>()`])
/// rather than carrying a "never stop" variant — keeps the public
/// API focused on the two real production paths.
#[derive(Debug, Clone, Copy)]
pub enum StopReason {
    /// User pressed Ctrl-C.
    UserSignal,
    /// `--duration` cap elapsed.
    DurationCap,
}

impl StopReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::UserSignal => "stop signal received",
            Self::DurationCap => "duration cap reached",
        }
    }
}

#[derive(Debug, Error)]
pub enum DelegateError {
    #[error(transparent)]
    Daemon(#[from] DaemonError),
    #[error("event stream closed before meeting.completed (last seen: {last_seen:?})")]
    StreamClosed { last_seen: Option<&'static str> },
    #[error("encoding event envelope: {0}")]
    Encode(String),
    #[error("writing event to stdout: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration the driver needs from `RecordArgs` — kept narrow so
/// the binary's argv shape can evolve without touching this lib.
#[derive(Debug, Clone)]
pub struct DelegateConfig {
    pub start: StartCaptureArgs,
    /// Hard duration cap. `None` means run until ctrl-c or
    /// `meeting.completed` arrives.
    pub duration_cap: Option<Duration>,
}

/// Outcome surfaced to the binary. The CLI exit code stays 0 for
/// `Success` and 1 for the rest (matching anyhow's existing default
/// for `cmd_record`); structured access is exposed so future callers
/// (e.g. a Tauri shell wrapper) can branch programmatically without
/// scraping stderr.
#[derive(Debug, Clone)]
pub struct DelegateOutcome {
    pub meeting_id: MeetingId,
    pub completed: MeetingCompletedData,
}

/// Drive a delegated record session. Opens the events stream first,
/// then `POST /v1/meetings`, then prints filtered envelopes to stdout
/// until either `stop` resolves (in which case `end_meeting` is
/// sent and the loop continues until `meeting.completed`) or the
/// daemon emits `meeting.completed` on its own.
///
/// `stop` is an async resolution from the caller. The production
/// binary passes `wait_for_stop(duration)` (Ctrl-C OR duration cap);
/// integration tests that don't want the stop arm to fire pass
/// `std::future::pending::<StopReason>()` so the daemon's own
/// `meeting.completed` event is what terminates the run.
pub async fn drive_delegated_session<S>(
    client: &DaemonClient,
    config: DelegateConfig,
    stop: S,
) -> Result<DelegateOutcome, DelegateError>
where
    S: Future<Output = StopReason>,
{
    // Subscribing before POST avoids a race where the daemon emits
    // `meeting.detected` / `meeting.armed` before the SSE listener is
    // ready. The connection is established by the time `events`
    // returns; events emitted from this point on land in `stream`.
    let mut stream = client.events(None).await?;
    let meeting: Meeting = client.start_capture(config.start).await?;
    let meeting_id = meeting.id;
    let mid_string = meeting_id.to_string();
    eprintln!(
        "delegated to herond; meeting_id={mid_string}, status={:?}",
        meeting.status
    );

    // Track the last event we observed so a stream-close error
    // surfaces *which* phase the run reached — easier triage than a
    // bare "stream closed". `event_type()` returns `&'static str`,
    // so no allocation per event.
    let mut last_seen_kind: Option<&'static str> = None;
    let mut end_sent = false;

    let stop_fut = stop;
    tokio::pin!(stop_fut);

    loop {
        tokio::select! {
            // `biased` so the stop arm wins ties — if the user hits
            // ctrl-c at the same instant a transcript event lands,
            // we'd rather send `end_meeting` first than spend the
            // tick decoding another envelope.
            biased;
            reason = &mut stop_fut, if !end_sent => {
                eprintln!("\n{}; ending meeting {mid_string}...", reason.as_str());
                if let Err(e) = client.end_meeting(&meeting_id).await {
                    eprintln!("warning: end_meeting failed: {e}");
                }
                // The `end_sent` guard makes this branch unreachable
                // on subsequent iterations — important because
                // tokio::pin'd futures must not be polled past
                // completion. Tests that never want to fire the stop
                // arm pass `std::future::pending::<StopReason>()`,
                // which leaves this branch perpetually pending.
                end_sent = true;
            }
            next = stream.next() => {
                match next {
                    Some(Ok(env)) => {
                        // Other meetings may share the bus (e.g. an
                        // ambient detector firing concurrently). Skip
                        // any envelope not scoped to ours; the
                        // `meeting_id` field on the framing carries
                        // the same id the OpenAPI defines.
                        if env.meeting_id.as_deref() != Some(mid_string.as_str()) {
                            continue;
                        }
                        last_seen_kind = Some(env.payload.event_type());
                        let line = serde_json::to_string(&env)
                            .map_err(|e| DelegateError::Encode(e.to_string()))?;
                        println!("{line}");
                        if let EventPayload::MeetingCompleted(data) = env.payload {
                            print_outcome_summary(&data);
                            return Ok(DelegateOutcome {
                                meeting_id,
                                completed: data,
                            });
                        }
                    }
                    Some(Err(e)) => return Err(DelegateError::Daemon(e)),
                    None => {
                        return Err(DelegateError::StreamClosed {
                            last_seen: last_seen_kind,
                        });
                    }
                }
            }
        }
    }
}

fn print_outcome_summary(data: &MeetingCompletedData) {
    let outcome_str = match data.outcome {
        MeetingOutcome::Success => "success",
        MeetingOutcome::Failed => "failed",
        MeetingOutcome::Aborted => "aborted",
        MeetingOutcome::PermissionRevoked => "permission_revoked",
    };
    match &data.failure_reason {
        Some(reason) => eprintln!("session complete: outcome={outcome_str} reason={reason}"),
        None => eprintln!("session complete: outcome={outcome_str}"),
    }
}

/// Production stop signal: ctrl-c OR (optionally) a duration cap.
/// The CLI's record-via-daemon path threads this in; tests bypass
/// it entirely and pass `std::future::pending::<StopReason>()` so
/// the daemon's own `meeting.completed` is what terminates.
pub async fn wait_for_stop(duration_cap: Option<Duration>) -> StopReason {
    match duration_cap {
        Some(d) => {
            tokio::select! {
                biased;
                r = tokio::signal::ctrl_c() => match r {
                    Ok(()) => StopReason::UserSignal,
                    Err(e) => {
                        tracing::warn!(error = %e, "ctrl_c handler failed; falling through to duration cap");
                        // Fall through to the duration timer rather
                        // than spinning — a wedged ctrl-c handler
                        // shouldn't strand the user.
                        tokio::time::sleep(d).await;
                        StopReason::DurationCap
                    }
                },
                _ = tokio::time::sleep(d) => StopReason::DurationCap,
            }
        }
        None => match tokio::signal::ctrl_c().await {
            Ok(()) => StopReason::UserSignal,
            Err(e) => {
                tracing::warn!(error = %e, "ctrl_c handler failed; parking forever");
                // A wedged ctrl-c handler with no duration cap is
                // genuinely unrecoverable — park rather than busy-
                // looping. The user can SIGKILL.
                std::future::pending::<()>().await;
                unreachable!()
            }
        },
    }
}
