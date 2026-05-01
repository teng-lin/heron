//! Capture lifecycle methods for [`crate::LocalSessionOrchestrator`].
//!
//! `start_capture`, `end_meeting`, `pause_capture`, and
//! `resume_capture` together drive the per-meeting `RecordingFsm`
//! through `Idle → Armed → Recording → Paused?? → Ended → Done`,
//! publish one `meeting.*` envelope per FSM edge, and (when a vault
//! root is configured) hand the work off to the v1 capture pipeline
//! and the optional v2 live-session stack.
//!
//! Each function corresponds 1:1 to a `SessionOrchestrator` trait
//! method on `LocalSessionOrchestrator`; the trait impl block in
//! `lib.rs` becomes a thin one-line delegation. Capture state
//! (`active_meetings`, the `finalized_meetings` index, capture
//! runtimes, the live-session factory) stays as fields on
//! [`crate::LocalSessionOrchestrator`] because it is also read by
//! read-side methods — bundling those fields into a capture-side
//! struct would force read-side to dereference through it for no
//! clarity gain.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use heron_pipeline::session::{
    Orchestrator as CliSessionOrchestrator, SessionConfig as CliSessionConfig,
    SessionError as CliSessionError,
};
use heron_session::{
    EventPayload, Meeting, MeetingCompletedData, MeetingId, MeetingOutcome, MeetingStatus,
    Platform, SessionError, StartCaptureArgs, SummaryLifecycle, TranscriptLifecycle,
};
use heron_types::{RecordingFsm, SummaryOutcome};
use tokio::sync::oneshot;

use crate::compose::{build_live_session_start_args, pre_meeting_briefing_for_v1};
use crate::live_session::DynLiveSession;
use crate::metrics_names;
use crate::pipeline_glue::{
    complete_pipeline_meeting, insert_finalized_meeting, pipeline_to_session_error,
    publish_meeting_event, push_pruned_finalizer, transition_to_session_error,
};
use crate::platform::platform_target_bundle_id;
use crate::state::{ActiveMeeting, CaptureRuntime, FinalizedMeeting};
use crate::validation::normalize_calendar_event_id;
use crate::{LocalSessionOrchestrator, lock_or_recover};

pub(crate) async fn start_capture(
    orch: &LocalSessionOrchestrator,
    args: StartCaptureArgs,
) -> Result<Meeting, SessionError> {
    // FSM-merge: drive the same `RecordingFsm` `heron-cli`'s
    // session orchestrator uses on the live audio path through
    // `idle → armed → recording`, publishing one bus event per
    // transition. A future PR replaces this synchronous walk with
    // an audio-task-driven path that returns at `Armed` and emits
    // `MeetingStarted` once Core Audio actually starts producing
    // PCM; the trait + bus surface stays the same — only the
    // timing of `MeetingStarted` shifts.
    let normalized_event_id = match args.calendar_event_id.as_deref() {
        Some(raw) => Some(normalize_calendar_event_id(raw)?),
        None => None,
    };
    let id = MeetingId::now_v7();
    let started_at = Utc::now();
    let mut meeting = Meeting {
        id,
        status: MeetingStatus::Detected,
        platform: args.platform,
        // The `hint` is wire-shape free text; surfacing it as the
        // title is the most honest projection until a real source
        // (AX window title, calendar correlation) lands.
        title: args.hint,
        calendar_event_id: normalized_event_id.clone(),
        started_at,
        ended_at: None,
        duration_secs: None,
        participants: Vec::new(),
        transcript_status: TranscriptLifecycle::Pending,
        summary_status: SummaryLifecycle::Pending,
        // Tags are LLM-inferred from the summary; an active capture
        // has no summary yet, so start empty and let
        // `meeting_from_note` fill them in once the note is
        // finalized on disk.
        tags: Vec::new(),
        // No summary has run yet at start-capture time; cost is
        // populated later by `meeting_from_note` when the
        // finalized vault note is read back.
        processing: None,
        // No structured action items yet at start-capture time;
        // populated later by `meeting_from_note` from
        // `Frontmatter.action_items` once the vault note is on
        // disk. Tier 0 #3 — read path only.
        action_items: Vec::new(),
    };
    let mut fsm = RecordingFsm::new();

    // Atomic singleton-check-and-claim. The platform-conflict scan
    // and the placeholder insert have to share one critical section
    // — otherwise two concurrent `start_capture` calls for the same
    // platform could both pass the check before either inserted,
    // producing parallel captures. Everything inside the scope is
    // synchronous: bus broadcasts (`bus.send` is non-blocking),
    // FSM transitions, `tokio::task::spawn_blocking` (returns a
    // JoinHandle immediately; the blocking work runs off-thread),
    // and a brief `pending_contexts` lock taken AFTER
    // `active_meetings` per the lock-ordering rule. The lock is
    // released before the v2 `factory.start(...).await` further
    // down — that `.await` is why the live-session attachment runs
    // in its own short critical section after the await rather
    // than here.
    let applied_context = {
        let mut active = lock_or_recover(&orch.active_meetings);
        if active
            .values()
            .any(|m| m.meeting.platform == args.platform && !m.meeting.status.is_terminal())
        {
            return Err(SessionError::CaptureInProgress {
                platform: args.platform,
            });
        }

        publish_meeting_event(
            &orch.bus,
            EventPayload::MeetingDetected(meeting.clone()),
            id,
        );

        // idle → armed. `on_hotkey` from `Idle` is the FSM's "user
        // armed a capture" edge; `Invalid` here would mean the
        // freshly-built FSM isn't actually `Idle`, which can't
        // happen — map defensively rather than `unwrap` so a future
        // FSM change surfaces as a typed error.
        fsm.on_hotkey().map_err(transition_to_session_error)?;
        meeting.status = MeetingStatus::Armed;
        publish_meeting_event(&orch.bus, EventPayload::MeetingArmed(meeting.clone()), id);

        // armed → recording.
        fsm.on_yes().map_err(transition_to_session_error)?;
        meeting.status = MeetingStatus::Recording;
        publish_meeting_event(&orch.bus, EventPayload::MeetingStarted(meeting.clone()), id);

        // Smoke metric — the canonical example sub-issues #224 /
        // #225 / #226 copy. The label MUST flow through
        // `redacted!` (compile-time literal-only) or
        // `RedactedLabel::from_static`; see
        // `docs/observability.md` for the rule. `Platform` is a
        // closed enum with snake_case discriminants → safe as
        // literals. Any fields with user-content shape
        // (meeting_id, hint, calendar_event_id) are NEVER
        // attached as labels.
        let platform_label: heron_metrics::RedactedLabel = match args.platform {
            Platform::Zoom => heron_metrics::redacted!("zoom"),
            Platform::GoogleMeet => heron_metrics::redacted!("google_meet"),
            Platform::MicrosoftTeams => heron_metrics::redacted!("microsoft_teams"),
            Platform::Webex => heron_metrics::redacted!("webex"),
        };
        metrics::counter!(
            heron_metrics::SMOKE_CAPTURE_STARTED_TOTAL,
            "platform" => platform_label.into_inner(),
        )
        .increment(1);
        // Capture-lifecycle gauge: every successful arm → recording
        // walk bumps `capture_active_count`. The matching
        // decrement lives in `end_meeting` (via `decrement(1.0)`).
        // No labels — the dashboard answer is "how many captures
        // are running right now" and the per-platform breakdown
        // already lives on `capture_started_total{platform}` /
        // `capture_ended_total{reason}`.
        metrics::gauge!(metrics_names::CAPTURE_ACTIVE).increment(1.0);

        // Consume the pending context AFTER the FSM walk commits
        // but BEFORE building `CliSessionConfig`, so the rendered
        // briefing can feed both v1
        // (`CliSessionConfig.pre_meeting_briefing`) and v2
        // (`build_live_session_start_args`). A failed FSM
        // transition above `?`-returns and drops the guard before
        // we touch `pending_contexts`, so a retry still finds the
        // staged entry.
        let applied_context = normalized_event_id
            .as_deref()
            .and_then(|cid| orch.pending_contexts.remove(cid));
        let pre_meeting_briefing = pre_meeting_briefing_for_v1(applied_context.as_ref(), id);

        let pause_flag = Arc::new(AtomicBool::new(false));
        let runtime = if let Some(vault_root) = orch.vault_root.clone() {
            let (stop_tx, stop_rx) = oneshot::channel();
            let config = CliSessionConfig {
                session_id: id.0,
                target_bundle_id: platform_target_bundle_id(args.platform).to_owned(),
                cache_dir: orch.cache_dir.clone(),
                vault_root,
                stt_backend_name: orch.stt_backend_name.clone(),
                // Tier 4 #17: forward the user-configured
                // vocabulary-boost list to the WhisperKit backend.
                // Cloned per `start_capture` so each session
                // captures a *snapshot* of the orchestrator's
                // hotwords at start time. The current orchestrator
                // is `&self` and the field is plain
                // `Vec<String>`, so there's no concurrent-mutation
                // hazard today — but if a future PR adds a
                // `Settings.hotwords` live-reload setter (with
                // interior mutability via `RwLock` / `Mutex`), the
                // snapshot is what keeps in-flight sessions
                // pointing at a stable prompt instead of swapping
                // mid-decode.
                hotwords: orch.hotwords.clone(),
                llm_preference: orch.llm_preference,
                pre_meeting_briefing,
                // Tier 0b #4: bridge `SpeakerEvent` from the AX
                // observer onto the canonical event bus so SSE
                // / Tauri / MCP transports can render a "now
                // speaking" indicator without subscribing to a
                // private channel. Cheap clone — the bus is
                // `Arc`-backed inside.
                event_bus: Some((orch.bus.clone(), id)),
                // Tier 4 #19: forward the orchestrator's slug
                // strategy so `pipeline.rs` picks the right
                // `<vault>/meetings/<filename>.md` shape.
                file_naming_pattern: orch.file_naming_pattern,
                // Tier 4 #18 / #21: the daemon orchestrator does
                // not currently read the desktop's `Settings.persona`
                // / `Settings.strip_names_before_summarization`. The
                // desktop's `resummarize.rs` threads them in for the
                // re-summarize path; live capture inherits the
                // pre-Tier-4 prompt path until the daemon grows a
                // settings reader.
                persona: None,
                strip_names: false,
                // Tier 3 #16: hand the pause flag to the pipeline
                // so WAV writers + AX collector + audio-level
                // collector can drop frames on the floor when
                // paused. The orchestrator owns the canonical flag;
                // this is a cheap `Arc` clone.
                pause_flag: Some(Arc::clone(&pause_flag)),
            };
            let handle = tokio::task::spawn_blocking(move || {
                // CoreAudio/cpal handles in the capture path are
                // not `Send` on macOS. Run the whole shared v1
                // pipeline on one blocking worker with its own
                // current-thread runtime so those handles are
                // never moved between Tokio worker threads.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| CliSessionError::Pipeline(format!("tokio runtime: {e}")))?;
                runtime.block_on(async move {
                    let mut orchestrator = CliSessionOrchestrator::new(config);
                    orchestrator.run(stop_rx).await
                })
            });
            CaptureRuntime::Pipeline { stop_tx, handle }
        } else {
            CaptureRuntime::Synthetic
        };

        // Placeholder insert: claims the platform slot before we
        // release the lock. The v2 live session (if any) is
        // attached below in a second critical section, after
        // `factory.start(..).await` resolves.
        active.insert(
            id,
            ActiveMeeting {
                fsm,
                meeting: meeting.clone(),
                runtime,
                applied_context: applied_context.clone(),
                live_session: None,
                pause_flag,
            },
        );

        applied_context
    };

    // The v2 factory call is the only step that needs the lock
    // released, because it `.await`s on vendor HTTP / WebSocket
    // open. The trade-off: a concurrent `end_meeting(id)` on this
    // same meeting could land in the brief gap between the insert
    // above and the live-session attach below; that race is closed
    // by the post-await scope checking that the entry is still
    // present and tearing the orphan session down if it isn't.
    let context_attached = applied_context.is_some();

    if let Some(factory) = orch.live_session_factory.as_ref() {
        let live_args =
            build_live_session_start_args(id, args.platform, &meeting, applied_context.as_ref());
        match factory.start(live_args).await {
            Ok(session) => {
                let bot_id = session.bot_id();
                let realtime_session = session.realtime_session();
                // Hold the lock only long enough to attach the
                // session, OR (when the entry has vanished) hand
                // the session back to the outer scope as an
                // orphan to tear down. Returning the box out of
                // the lock scope keeps the `MutexGuard` (sync,
                // !Send) off the `.await` that follows.
                let orphan: Option<Box<dyn DynLiveSession>> = {
                    let mut active = lock_or_recover(&orch.active_meetings);
                    match active.get_mut(&id) {
                        Some(entry) => {
                            entry.live_session = Some(session);
                            None
                        }
                        None => Some(session),
                    }
                };
                if let Some(orphan) = orphan {
                    // The capture was ended (or otherwise
                    // removed) while the factory was running.
                    // Best-effort tear the dangling session down
                    // so we don't leak a vendor bot.
                    tracing::warn!(
                        meeting_id = %id,
                        "active meeting disappeared during live session start; tearing down",
                    );
                    if let Err(err) = orphan.shutdown().await {
                        tracing::warn!(
                            meeting_id = %id,
                            error = %err,
                            "best-effort live-session shutdown failed",
                        );
                    }
                } else {
                    tracing::info!(
                        meeting_id = %id,
                        bot_id = %bot_id,
                        realtime_session = %realtime_session,
                        "v2 live session composed",
                    );
                }
            }
            Err(err) => {
                // Falling back to the v1 vault path is documented
                // behaviour. The two most common reasons here on
                // alpha are:
                //   * `OPENAI_API_KEY` missing (parallel work),
                //   * Recall vendor flake on `bot_create`.
                // In either case the daemon should still record
                // and transcribe the meeting; only realtime bot
                // interaction is lost. The error rides into the
                // log so operators can correlate with the
                // vendor-side failure.
                tracing::warn!(
                    meeting_id = %id,
                    error = %err,
                    "v2 live session composition failed; continuing with v1 vault pipeline only",
                );
            }
        }
    }

    tracing::info!(
        meeting_id = %id,
        platform = ?args.platform,
        calendar_event_id = ?normalized_event_id,
        context_attached,
        "capture started",
    );
    Ok(meeting)
}

pub(crate) async fn end_meeting(
    orch: &LocalSessionOrchestrator,
    id: &MeetingId,
) -> Result<(), SessionError> {
    // Drive the FSM through `recording → transcribing →
    // summarizing → idle`, publishing `meeting.ended` on the
    // recording-stop edge and `meeting.completed` on the
    // terminal edge. The intermediate transcribing/summarizing
    // edges are internal to the pipeline — they don't have a
    // public bus event today (transcript / summary deltas ride
    // their own typed payloads, emitted by the future audio +
    // STT + LLM impls).
    let entry = {
        let mut active = lock_or_recover(&orch.active_meetings);
        active.remove(id).ok_or_else(|| SessionError::NotFound {
            what: format!("active meeting {id}"),
        })?
    };
    // Decrement the capture-active gauge as soon as we've claimed
    // the entry for removal. Pairing it with the `remove()` (not
    // with the later `?`-bearing transitions) means a subsequent
    // FSM-rejection error path doesn't leak the gauge upward
    // forever — the matching `start_capture` increment landed
    // when the entry became active, the gauge must mirror the
    // entry's existence in `active_meetings`, and the entry is
    // gone the moment `remove()` returns Some.
    metrics::gauge!(metrics_names::CAPTURE_ACTIVE).decrement(1.0);
    let ActiveMeeting {
        mut fsm,
        mut meeting,
        runtime,
        applied_context: _,
        live_session,
        pause_flag: _,
    } = entry;

    // Tear the v2 stack down BEFORE the v1 finalizer runs so the
    // realtime backend's WebSocket and the vendor bot are
    // released as quickly as possible. We hand the shutdown off
    // to a finalizer task because the request handler should not
    // block on vendor leave HTTP calls.
    if let Some(session) = live_session {
        let bot_id = session.bot_id();
        let realtime_session = session.realtime_session();
        let id_copy = *id;
        let live_finalizer = tokio::spawn(async move {
            if let Err(err) = session.shutdown().await {
                tracing::warn!(
                    meeting_id = %id_copy,
                    bot_id = %bot_id,
                    realtime_session = %realtime_session,
                    error = %err,
                    "live session shutdown reported errors",
                );
            } else {
                tracing::info!(
                    meeting_id = %id_copy,
                    bot_id = %bot_id,
                    realtime_session = %realtime_session,
                    "live session shut down cleanly",
                );
            }
        });
        push_pruned_finalizer(&orch.finalizers, live_finalizer);
    }

    // recording → transcribing. The `on_hotkey` from `Recording`
    // is the FSM's stop edge per `docs/archives/implementation.md` §14.2.
    // The FSM rejects this from any other state via
    // `TransitionError`, which `transition_to_session_error`
    // surfaces as `Validation` — that's the safety net for the
    // (currently impossible) drift where an entry's FSM is not
    // at `Recording`.
    fsm.on_hotkey().map_err(transition_to_session_error)?;
    let ended_at = Utc::now();
    // `num_seconds` is `i64`; saturate at 0 if the system clock
    // ran backwards between `start_capture` and `end_meeting`
    // (NTP slew on a long-running daemon). A negative duration
    // would be both meaningless and a panic-on-cast risk.
    let duration_secs = (ended_at - meeting.started_at).num_seconds().max(0) as u64;
    meeting.status = MeetingStatus::Ended;
    meeting.ended_at = Some(ended_at);
    meeting.duration_secs = Some(duration_secs);
    insert_finalized_meeting(
        &orch.finalized_meetings,
        *id,
        FinalizedMeeting {
            meeting: meeting.clone(),
            note_path: None,
        },
    );
    publish_meeting_event(&orch.bus, EventPayload::MeetingEnded(meeting.clone()), *id);

    match runtime {
        CaptureRuntime::Synthetic => {
            fsm.on_transcribe_done()
                .map_err(transition_to_session_error)?;
            fsm.on_summary(SummaryOutcome::Done)
                .map_err(transition_to_session_error)?;
            meeting.status = MeetingStatus::Done;
            meeting.transcript_status = TranscriptLifecycle::Complete;
            meeting.summary_status = SummaryLifecycle::Ready;
            insert_finalized_meeting(
                &orch.finalized_meetings,
                *id,
                FinalizedMeeting {
                    meeting: meeting.clone(),
                    note_path: None,
                },
            );
            publish_meeting_event(
                &orch.bus,
                EventPayload::MeetingCompleted(MeetingCompletedData {
                    meeting,
                    outcome: MeetingOutcome::Success,
                    failure_reason: None,
                }),
                *id,
            );
            // Synthetic path has no real pipeline to wait on, so
            // the lifecycle disposition is decided here. Emit the
            // single `capture_ended_total` increment for this
            // meeting with `reason="user_stop"` — the test stub
            // path always corresponds to "user invoked
            // end_meeting; no automated outcome to report". This
            // keeps `sum(capture_ended_total)` equal to the number
            // of finished meetings (the pipeline arm emits its
            // own `success` / `error` increment from
            // `complete_pipeline_meeting`, never both arms in one
            // lifecycle).
            let reason_label = heron_metrics::redacted!("user_stop");
            metrics::counter!(
                metrics_names::CAPTURE_ENDED_TOTAL,
                "reason" => reason_label.into_inner(),
            )
            .increment(1);
        }
        CaptureRuntime::Pipeline { stop_tx, handle } => {
            let _ = stop_tx.send(());
            let bus = orch.bus.clone();
            let finalized_meetings = Arc::clone(&orch.finalized_meetings);
            let id = *id;
            let finalizer = tokio::spawn(async move {
                let result = match handle.await {
                    Ok(Ok(outcome)) => Ok(outcome),
                    Ok(Err(err)) => Err(pipeline_to_session_error(err)),
                    Err(err) => Err(SessionError::Validation {
                        detail: format!("capture pipeline task failed: {err}"),
                    }),
                };
                complete_pipeline_meeting(&bus, &finalized_meetings, id, fsm, meeting, result);
            });
            push_pruned_finalizer(&orch.finalizers, finalizer);
            // Pipeline path: `complete_pipeline_meeting` (running
            // on the spawned finalizer) is responsible for the
            // `capture_ended_total{reason="success"|"error"}`
            // increment once the pipeline finishes. NOT emitted
            // here so each lifecycle results in exactly one
            // increment, matching the issue's mutually-exclusive
            // `reason` enum.
        }
    }
    // The gauge decrement happens earlier (right after
    // `active.remove(id)`) to avoid a leak on FSM rejection
    // between here and the remove. The `capture_ended_total`
    // increment is emitted per-arm above (synthetic vs pipeline)
    // so each meeting maps to exactly one counter bump.
    tracing::info!(
        meeting_id = %id,
        duration_secs,
        "capture ended",
    );
    Ok(())
}

pub(crate) async fn pause_capture(
    orch: &LocalSessionOrchestrator,
    id: &MeetingId,
) -> Result<(), SessionError> {
    // Tier 3 #16: drive the FSM through `Recording → Paused` and
    // flip the shared atomic flag the capture pipeline reads. Both
    // sides happen under the active-meetings lock so a concurrent
    // `resume_capture` / `end_meeting` can't observe a torn state
    // (FSM at `Recording` while flag is `true`, or vice versa).
    // The publish step is sync — `bus.publish` is non-blocking —
    // so holding the guard across it is safe per the existing
    // lock-discipline rules.
    let snapshot = {
        let mut active = lock_or_recover(&orch.active_meetings);
        let entry = active.get_mut(id).ok_or_else(|| SessionError::NotFound {
            what: format!("active meeting {id}"),
        })?;
        entry
            .fsm
            .on_pause()
            .map_err(|_| SessionError::InvalidState {
                current_state: entry.meeting.status,
            })?;
        entry.pause_flag.store(true, Ordering::SeqCst);
        entry.meeting.status = MeetingStatus::Paused;
        entry.meeting.clone()
    };
    // No dedicated `meeting.paused` event today: the wire surface
    // is the meeting's `status` field via `GET /meetings/{id}`,
    // which reflects the orchestrator's snapshot. A future PR can
    // add a typed bus event without changing the pause/resume HTTP
    // contract — keeping the `EventPayload` enum stable for now.
    tracing::info!(meeting_id = %id, "capture paused");
    let _ = snapshot;
    Ok(())
}

pub(crate) async fn resume_capture(
    orch: &LocalSessionOrchestrator,
    id: &MeetingId,
) -> Result<(), SessionError> {
    // Mirror image of `pause_capture`: drive `Paused → Recording`
    // and clear the flag under the same lock. `InvalidState`
    // surfaces when the meeting isn't in `Paused` (e.g. someone
    // hit Resume while we were already recording, or after end_meeting
    // dropped the entry — that path is already covered by the
    // NotFound short-circuit, but the FSM check keeps the typed
    // error tight).
    let snapshot = {
        let mut active = lock_or_recover(&orch.active_meetings);
        let entry = active.get_mut(id).ok_or_else(|| SessionError::NotFound {
            what: format!("active meeting {id}"),
        })?;
        entry
            .fsm
            .on_resume()
            .map_err(|_| SessionError::InvalidState {
                current_state: entry.meeting.status,
            })?;
        entry.pause_flag.store(false, Ordering::SeqCst);
        entry.meeting.status = MeetingStatus::Recording;
        entry.meeting.clone()
    };
    tracing::info!(meeting_id = %id, "capture resumed");
    let _ = snapshot;
    Ok(())
}
