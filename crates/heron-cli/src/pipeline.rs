//! End-to-end session pipeline.
//!
//! Implements [`run_pipeline`], the body of [`crate::session::Orchestrator::run`].
//! Stages mirror `docs/archives/plan.md` §4.1 (data flow):
//!
//! 1. Walk the FSM `idle → armed → recording`, persisting state to
//!    `<cache>/state.json` per §14.3 so a SIGKILL leaves a salvage
//!    candidate.
//! 2. Start [`heron_audio::AudioCapture`]; spawn writer tasks for the
//!    mic + tap WAV files and an AX listener task.
//! 3. Wait on `stop_rx`. When it fires, drain the in-flight frames,
//!    finalize the WAVs, stop the AX backend.
//! 4. Run the STT pass over each channel, merge into a single
//!    timeline-sorted JSONL under `<vault>/transcripts/<id>.jsonl`.
//! 5. Encode the pair to m4a, verify, then summarize via the LLM and
//!    write the markdown note via [`heron_vault::VaultWriter`].
//! 6. Purge the ringbuffer iff the m4a verifies.
//!
//! Errors past the recording phase are logged and the FSM is still
//! walked to `idle` with the appropriate [`IdleReason`] — partial
//! outputs (transcript-only note) are preferable to crashing the
//! orchestrator on a transient backend hiccup.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use heron_types::{
    Attendee, Channel, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, ItemId,
    MeetingType, RecordingFsm, SessionId, SessionPhase, SessionStateRecord, SpeakerEvent,
    SpeakerSource, SummaryOutcome, Turn, write_state,
};
use heron_vault::{CalendarAttendee, CalendarEvent, CalendarReader};
use heron_zoom::Aligner;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::session::{Backends, SessionConfig, SessionError, SessionOutcome};

/// Slop on either side of the session window when querying calendar.
/// 30 minutes covers users who join early or run long without pulling
/// in unrelated all-day events.
const CALENDAR_WINDOW_SLOP_SECS: i64 = 30 * 60;

/// Hard cap on the calendar FFI round-trip. The Swift side blocks on
/// `EKEventStore.requestFullAccessToEvents` via a `DispatchSemaphore`
/// with no timeout knob (per the comment in `heron_vault::calendar`),
/// so on a fresh install or after `tccutil reset Calendar` the call
/// can stall waiting for the user to dismiss a TCC dialog. The plan
/// `§4.1` data flow says calendar reads must not prompt from the CLI
/// path; this timeout enforces that contract: if the bridge doesn't
/// return promptly we fall through to the "denied" branch and the
/// note still finalizes (slug stays "untitled", attendees stay
/// LLM-inferred). 5s is well above typical EventKit query latency
/// (low-ms when access is already granted) but tight enough to
/// prevent a wedged TCC daemon from hanging session finalize.
const CALENDAR_READ_TIMEOUT_SECS: u64 = 5;

/// 48 kHz f32 mono per `docs/archives/implementation.md` §6 (capture sample rate).
const SAMPLE_RATE_HZ: u32 = 48_000;

/// Channel size for the AX → aligner mpsc. 256 events is ~1 minute of
/// active-speaker churn at the upper bound observed in the §4 spike.
const AX_EVENT_CHANNEL_SIZE: usize = 256;

/// Run the full pipeline. Drives the FSM and threads outputs from each
/// backend into the next per the module preamble.
///
/// `skip_audio_capture` short-circuits the live `AudioCapture::start`
/// call and assumes the caller (typically an integration test) has
/// pre-populated `<cache>/sessions/<id>/{mic,tap}.wav`. Production
/// callers always pass `false`; the field exists so tests can drive
/// the pipeline through STT → aligner → vault writer without needing
/// TCC permissions.
pub async fn run_pipeline(
    fsm: &mut RecordingFsm,
    config: &SessionConfig,
    backends: Backends,
    stop_rx: oneshot::Receiver<()>,
    skip_audio_capture: bool,
) -> Result<SessionOutcome, SessionError> {
    let (stt, ax, llm, calendar) = backends;

    // Capture wall-clock at session arm so the note's filename and
    // frontmatter `start` reflect when the meeting actually began,
    // not when the LLM finished. Long sessions or near-midnight calls
    // would otherwise file under the wrong day / time.
    let started_at_wall = Utc::now();

    // §14.2: idle → armed → recording. State.json is persisted on each
    // edge so a SIGKILL leaves a salvage candidate for §14.3.
    fsm.on_hotkey()?;
    persist_state(config, SessionPhase::Armed, started_at_wall)?;
    fsm.on_yes()?;
    persist_state(config, SessionPhase::Recording, started_at_wall)?;

    let session_dir = session_cache_dir(config);
    std::fs::create_dir_all(&session_dir)?;
    let mic_wav = session_dir.join("mic.wav");
    let mic_clean_wav = session_dir.join("mic_clean.wav");
    let tap_wav = session_dir.join("tap.wav");

    let started_at = std::time::Instant::now();
    let ax_events: Vec<SpeakerEvent> = if skip_audio_capture {
        // Test path: caller seeded the WAVs and is responsible for
        // any AX events it wants the aligner to see (writes them
        // directly into the in-memory aligner via injected backend
        // state). Just wait for the stop signal so the test can drive
        // the timing.
        let _ = stop_rx.await;
        Vec::new()
    } else {
        match run_capture_phase(
            config,
            &mic_wav,
            &mic_clean_wav,
            &tap_wav,
            ax.as_ref(),
            stop_rx,
        )
        .await
        {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(error = %e, "audio capture failed; ending session early");
                return finalize_aborted(
                    fsm,
                    config,
                    started_at_wall,
                    format!("audio capture: {e}"),
                );
            }
        }
    };

    // recording → transcribing
    fsm.on_hotkey()?;
    persist_state(config, SessionPhase::Transcribing, started_at_wall)?;

    let elapsed = started_at.elapsed();
    let duration_secs = elapsed.as_secs_f64().max(0.5);

    // STT pass per channel. PartialWriter drops its outputs into the
    // session cache as `.partial`; the merged final transcript lands
    // in <vault>/transcripts/<id>.jsonl.
    let mic_partial = session_dir.join("mic.partial.jsonl");
    let tap_partial = session_dir.join("tap.partial.jsonl");
    // STT consumes the post-AEC `mic_clean.wav` per the heron-audio
    // contract — raw `mic.wav` would re-feed any tap bleed back into
    // the transcript. The integration test path falls back to mic.wav
    // if mic_clean wasn't seeded (test stubs don't run AEC).
    let stt_mic_path = if mic_clean_wav.exists() {
        &mic_clean_wav
    } else {
        &mic_wav
    };
    let mic_turns = run_stt(
        stt.as_ref(),
        stt_mic_path,
        Channel::MicClean,
        config.session_id,
        &mic_partial,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "mic STT pass failed; transcript will exclude mic");
        Vec::new()
    });
    let tap_turns_raw = run_stt(
        stt.as_ref(),
        &tap_wav,
        Channel::Tap,
        config.session_id,
        &tap_partial,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "tap STT pass failed; transcript will exclude tap");
        Vec::new()
    });

    // Aligner: merge AX events into the tap-channel turns. Mic turns
    // pass through untouched (the aligner short-circuits Channel::Mic
    // to "me" / SpeakerSource::Self_).
    let mut aligner = Aligner::new();
    for evt in ax_events {
        aligner.ingest_event(evt);
    }
    let mut turns: Vec<Turn> = mic_turns
        .into_iter()
        .chain(tap_turns_raw)
        .map(|t| aligner.attribute(t))
        .collect();
    // Stable timeline ordering: turns from both channels are emitted
    // in t0 order so downstream consumers (LLM, review UI) see a
    // single coherent transcript.
    turns.sort_by(|a, b| a.t0.partial_cmp(&b.t0).unwrap_or(std::cmp::Ordering::Equal));

    // Final transcript path under the vault. Created with the same
    // mode as everything else heron writes (0600 via atomic_write).
    let transcripts_dir = config.vault_root.join("transcripts");
    std::fs::create_dir_all(&transcripts_dir)?;
    let transcript_path = transcripts_dir.join(format!("{}.jsonl", config.session_id));
    write_jsonl_atomic(&transcript_path, &turns)?;

    // m4a encode — best-effort. A missing ffmpeg surfaces as
    // EncodeError::FfmpegMissing; we log and continue with a
    // transcript-only note rather than failing the whole session.
    let recordings_dir = config.vault_root.join("recordings");
    std::fs::create_dir_all(&recordings_dir)?;
    let m4a_path = recordings_dir.join(format!("{}.m4a", config.session_id));
    let m4a_ok = match heron_vault::encode_to_m4a(&mic_wav, &tap_wav, &m4a_path) {
        Ok(()) => match heron_vault::verify_m4a(&m4a_path, duration_secs) {
            Ok(true) => true,
            Ok(false) => {
                tracing::warn!("m4a verification rejected encoded file; ringbuffer retained");
                false
            }
            Err(e) => {
                tracing::warn!(error = %e, "m4a verification errored; ringbuffer retained");
                false
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "m4a encode failed; note will reference WAV cache");
            false
        }
    };

    // transcribing → summarizing
    fsm.on_transcribe_done()?;
    persist_state(config, SessionPhase::Summarizing, started_at_wall)?;

    let date_str = started_at_wall.format("%Y-%m-%d").to_string();
    let start_hhmm = started_at_wall.format("%H%M").to_string();
    let frontmatter_start = started_at_wall.format("%H:%M").to_string();

    // Calendar lookup. Picks the event whose [start, end] window has
    // maximum overlap with the session window; its title becomes the
    // filename slug and its attendees override anything the LLM
    // inferred from the transcript (calendar is authoritative for who
    // was on the call). Honors the §12.2 denial contract — when the
    // user has not granted Calendar access, `read_window` returns
    // `Ok(None)` and the slug falls through to "untitled" + frontmatter
    // attendees stay LLM-inferred.
    let session_end_wall =
        started_at_wall + chrono::Duration::milliseconds((duration_secs * 1000.0) as i64);
    let calendar_match = match read_calendar_event(calendar, started_at_wall, session_end_wall)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "calendar read failed; falling back to LLM-inferred attendees + 'untitled' slug");
            None
        }
    };
    let (slug_owned, calendar_attendees) = match &calendar_match {
        Some(event) => (
            slug_from_title(&event.title),
            Some(calendar_attendees_to_attendees(&event.attendees)),
        ),
        None => ("untitled".to_owned(), None),
    };
    let slug = &slug_owned;

    // LLM summarize. A failure here is non-fatal — we still write a
    // transcript-only note so the user has the raw turns to read.
    let llm_out = match summarize(llm.as_ref(), &transcript_path).await {
        Ok(out) => Some(out),
        Err(e) => {
            tracing::warn!(error = %e, "summarize failed; writing transcript-only note");
            None
        }
    };

    // Frontmatter assembly. Defaults align with §3.3; the LLM output
    // overrides company / meeting_type / tags / attendees / action
    // items when available.
    let (body, cost, action_items, attendees, tags, company, meeting_type) = match llm_out {
        Some(o) => (
            o.body,
            o.cost,
            o.action_items,
            o.attendees,
            o.tags,
            o.company,
            o.meeting_type,
        ),
        None => (
            fallback_body(&turns),
            Cost {
                summary_usd: 0.0,
                tokens_in: 0,
                tokens_out: 0,
                model: "(no summarizer)".into(),
            },
            Vec::new(),
            Vec::new(),
            vec!["meeting".into()],
            None,
            MeetingType::Other,
        ),
    };

    // Calendar attendees win over LLM-inferred attendees when present:
    // calendar entries have the canonical attendee list for invited
    // meetings, while the LLM's list is best-effort name extraction
    // from the transcript. An empty calendar attendees list (event
    // exists, no attendees recorded) falls back to LLM inference so a
    // self-scheduled blocker doesn't wipe transcript-derived names.
    let attendees = calendar_attendees
        .filter(|a| !a.is_empty())
        .unwrap_or(attendees);

    let frontmatter = Frontmatter {
        date: started_at_wall.date_naive(),
        start: frontmatter_start,
        duration_min: (duration_secs / 60.0).ceil() as u32,
        company,
        attendees,
        meeting_type,
        source_app: config.target_bundle_id.clone(),
        recording: PathBuf::from("recordings").join(format!("{}.m4a", config.session_id)),
        transcript: PathBuf::from("transcripts").join(format!("{}.jsonl", config.session_id)),
        diarize_source: derive_diarize_source(&turns),
        disclosed: Disclosure {
            stated: false,
            when: None,
            how: DisclosureHow::None,
        },
        cost,
        action_items,
        tags,
        extra: serde_yaml::Mapping::default(),
    };

    let writer = heron_vault::VaultWriter::new(&config.vault_root);
    let note_path = match writer.finalize_session(&date_str, &start_hhmm, slug, &frontmatter, &body)
    {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::error!(error = %e, "vault finalize_session failed; note not written");
            None
        }
    };

    // Purge the cache iff m4a verified. A retain is the right
    // behavior on encode/verify failure — the user can still salvage
    // the WAV cache from `<cache>/sessions/<id>/`.
    if m4a_ok {
        let outcome = heron_vault::purge_after_verify(&m4a_path, duration_secs, &session_dir);
        if !outcome.cache_purged() {
            tracing::warn!(?outcome, "ringbuffer cache retained for salvage");
        }
    } else {
        tracing::info!("ringbuffer cache retained: m4a did not verify");
    }

    let summary_outcome = if note_path.is_some() {
        SummaryOutcome::Done
    } else {
        SummaryOutcome::Failed
    };
    fsm.on_summary(summary_outcome)?;
    persist_state(config, SessionPhase::Done, started_at_wall)?;

    Ok(SessionOutcome {
        final_state: fsm.state(),
        last_idle_reason: fsm.last_idle_reason(),
        note_path,
    })
}

/// Walk the FSM home on a fatal pre-stt failure. Treats the session
/// as "summary failed" since no note was written.
fn finalize_aborted(
    fsm: &mut RecordingFsm,
    config: &SessionConfig,
    started_at_wall: chrono::DateTime<Utc>,
    reason: String,
) -> Result<SessionOutcome, SessionError> {
    tracing::warn!(reason = %reason, "session aborted before STT");
    fsm.on_hotkey()?;
    persist_state(config, SessionPhase::Transcribing, started_at_wall)?;
    fsm.on_transcribe_done()?;
    persist_state(config, SessionPhase::Summarizing, started_at_wall)?;
    fsm.on_summary(SummaryOutcome::Failed)?;
    persist_state(config, SessionPhase::Done, started_at_wall)?;
    Ok(SessionOutcome {
        final_state: fsm.state(),
        last_idle_reason: fsm.last_idle_reason(),
        note_path: None,
    })
}

/// Live-capture half of the pipeline. Spawns the WAV writers, the AX
/// listener, waits for `stop_rx`, then joins everything before
/// returning the buffered AX events for the aligner.
async fn run_capture_phase(
    config: &SessionConfig,
    mic_wav: &Path,
    mic_clean_wav: &Path,
    tap_wav: &Path,
    ax: &dyn heron_zoom::AxBackend,
    stop_rx: oneshot::Receiver<()>,
) -> Result<Vec<SpeakerEvent>, SessionError> {
    let session_dir = session_cache_dir(config);
    let capture =
        heron_audio::AudioCapture::start(config.session_id, &config.target_bundle_id, &session_dir)
            .await?;
    let mic_handle = spawn_wav_writer(
        capture.frames.resubscribe(),
        Channel::Mic,
        mic_wav.to_path_buf(),
    );
    let mic_clean_handle = spawn_wav_writer(
        capture.frames.resubscribe(),
        Channel::MicClean,
        mic_clean_wav.to_path_buf(),
    );
    let tap_handle = spawn_wav_writer(
        capture.frames.resubscribe(),
        Channel::Tap,
        tap_wav.to_path_buf(),
    );

    // AX listener: subscribe to SpeakerEvents into a Vec we own. The
    // `events` channel in heron_audio carries the mute / device-change
    // signal; we surface it via tracing only for now.
    let (ax_tx, ax_rx) = mpsc::channel::<SpeakerEvent>(AX_EVENT_CHANNEL_SIZE);
    let (evt_tx, _evt_rx) = mpsc::channel(AX_EVENT_CHANNEL_SIZE);
    let ax_handle = match ax
        .start(config.session_id, capture.clock, ax_tx, evt_tx)
        .await
    {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(error = %e, "AX backend unavailable; falling back to channel attribution");
            None
        }
    };
    let ax_collector = spawn_ax_collector(ax_rx);

    let _ = stop_rx.await;

    // Drop the capture handle to flush + close the broadcast senders;
    // writer tasks see RecvError::Closed and finalize their WAVs.
    drop(capture);
    if let Err(e) = mic_handle.await {
        tracing::warn!(error = %e, "mic WAV writer panicked");
    }
    if let Err(e) = mic_clean_handle.await {
        tracing::warn!(error = %e, "mic_clean WAV writer panicked");
    }
    if let Err(e) = tap_handle.await {
        tracing::warn!(error = %e, "tap WAV writer panicked");
    }

    if let Some(h) = ax_handle
        && let Err(e) = h.stop().await
    {
        tracing::warn!(error = %e, "AX backend stop returned error");
    }
    Ok(ax_collector.await.unwrap_or_default())
}

fn session_cache_dir(config: &SessionConfig) -> PathBuf {
    config
        .cache_dir
        .join("sessions")
        .join(config.session_id.to_string())
}

fn persist_state(
    config: &SessionConfig,
    phase: SessionPhase,
    started_at: chrono::DateTime<Utc>,
) -> Result<(), SessionError> {
    let dir = session_cache_dir(config);
    std::fs::create_dir_all(&dir)?;
    let record = SessionStateRecord {
        state_version: heron_types::STATE_VERSION,
        session_id: config.session_id,
        started_at,
        last_updated: Utc::now(),
        source_app: config.target_bundle_id.clone(),
        cache_dir: dir,
        phase,
        mic_bytes_written: 0,
        tap_bytes_written: 0,
        turns_finalized: 0,
    };
    Ok(write_state(&record)?)
}

/// Spawn a tokio task that consumes capture frames matching `channel`
/// and streams them into a 48 kHz f32 mono WAV via `hound`. Returns a
/// JoinHandle so the caller can await finalization.
fn spawn_wav_writer(
    mut rx: broadcast::Receiver<heron_audio::CaptureFrame>,
    channel: Channel,
    path: PathBuf,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE_HZ,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = match hound::WavWriter::create(&path, spec) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "WAV create failed");
                return;
            }
        };
        loop {
            // blocking_recv on a broadcast receiver (sync) — we're on
            // a spawn_blocking thread, so this is the right primitive.
            match rx.blocking_recv() {
                Ok(frame) => {
                    if frame.channel != channel {
                        continue;
                    }
                    for &sample in &frame.samples {
                        if let Err(e) = writer.write_sample(sample) {
                            tracing::warn!(error = %e, "WAV sample write failed; aborting writer");
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        lagged = n,
                        ?channel,
                        "WAV writer lagged on broadcast channel"
                    );
                }
            }
        }
        if let Err(e) = writer.finalize() {
            tracing::warn!(error = %e, path = %path.display(), "WAV finalize failed");
        }
    })
}

/// Drain the AX mpsc into a Vec. Returns when the sender side is
/// dropped (the AX backend exited). Buffer size is bounded by the
/// channel cap upstream so a runaway emitter can't OOM the
/// orchestrator.
fn spawn_ax_collector(mut rx: mpsc::Receiver<SpeakerEvent>) -> JoinHandle<Vec<SpeakerEvent>> {
    tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(evt) = rx.recv().await {
            events.push(evt);
        }
        events
    })
}

/// Thin wrapper around `SttBackend::transcribe`. Reads the partial
/// JSONL back into a Vec<Turn> after the pass since `transcribe`'s
/// callback path is best-effort + we want the on-disk record as the
/// authoritative source per §3.5.
async fn run_stt(
    stt: &dyn heron_speech::SttBackend,
    wav: &Path,
    channel: Channel,
    session_id: SessionId,
    partial_jsonl: &Path,
) -> Result<Vec<Turn>, SessionError> {
    if !wav.exists() {
        tracing::info!(path = %wav.display(), ?channel, "WAV missing; skipping STT pass");
        return Ok(Vec::new());
    }
    let _summary = stt
        .transcribe(wav, channel, session_id, partial_jsonl, Box::new(|_| {}))
        .await?;
    let turns = heron_speech::read_partial_jsonl(partial_jsonl)?;
    Ok(turns)
}

/// Run the LLM summarizer over the merged transcript. Failures
/// surface as `SessionError::Llm` and are caught at the call site.
async fn summarize(
    llm: &dyn heron_llm::Summarizer,
    transcript: &Path,
) -> Result<heron_llm::SummarizerOutput, SessionError> {
    let input = heron_llm::SummarizerInput {
        transcript,
        meeting_type: MeetingType::Other,
        existing_action_items: None,
        existing_attendees: None,
    };
    Ok(llm.summarize(input).await?)
}

/// Run the EventKit bridge on a `[start - slop, end + slop]` window
/// and pick the event whose own `[start, end]` has maximum overlap
/// with the session. Wraps the synchronous FFI call in
/// `spawn_blocking` so it can't stall the orchestrator's tokio thread
/// if EventKit takes a moment to return.
///
/// Returns `Ok(None)` when Calendar permission is not granted (the
/// reader's denial contract) or when the window has no overlapping
/// events. `Err` only on bridge / parse failures the caller should
/// log; a failure must not abort the session.
async fn read_calendar_event(
    reader: Box<dyn CalendarReader>,
    session_start: DateTime<Utc>,
    session_end: DateTime<Utc>,
) -> Result<Option<CalendarEvent>, heron_vault::CalendarError> {
    let window_start = session_start - chrono::Duration::seconds(CALENDAR_WINDOW_SLOP_SECS);
    let window_end = session_end + chrono::Duration::seconds(CALENDAR_WINDOW_SLOP_SECS);
    // `Box<dyn CalendarReader>` is `Send`; move it into the blocking
    // task so the FFI call (which may block on a TCC dialog or the
    // EventKit semaphore) does not stall a tokio worker.
    let blocking =
        tokio::task::spawn_blocking(move || reader.read_window(window_start, window_end));
    let timed = tokio::time::timeout(
        std::time::Duration::from_secs(CALENDAR_READ_TIMEOUT_SECS),
        blocking,
    )
    .await;
    let join_result = match timed {
        Ok(jr) => jr,
        Err(_elapsed) => {
            // Bridge didn't return within the budget — almost certainly
            // a TCC prompt waiting for user input. Treat as denial; the
            // session still finalizes with "untitled" + LLM attendees.
            tracing::warn!(
                "calendar read exceeded {}s budget; treating as denial",
                CALENDAR_READ_TIMEOUT_SECS,
            );
            return Ok(None);
        }
    };
    let result = match join_result {
        Ok(r) => r,
        Err(e) => {
            // Blocking task panicked — log and treat as denial so we
            // fall through to the no-calendar code path rather than
            // failing the whole session over an EventKit hiccup.
            tracing::warn!(error = %e, "calendar read task panicked; treating as denial");
            return Ok(None);
        }
    };
    let events: Vec<CalendarEvent> = match result? {
        Some(events) => events,
        None => return Ok(None),
    };
    Ok(pick_best_calendar_event(&events, session_start, session_end).cloned())
}

/// Pick the calendar event with the largest time-overlap against the
/// session `[session_start, session_end]` window. Ties are broken by
/// proximity of the event's start to the session's start. Returns
/// `None` when no event has any overlap.
fn pick_best_calendar_event(
    events: &[CalendarEvent],
    session_start: DateTime<Utc>,
    session_end: DateTime<Utc>,
) -> Option<&CalendarEvent> {
    // Millisecond precision so a sub-second test session (or a
    // legitimately short call cancelled in the first second) can still
    // intersect a calendar event that fully contains it.
    let session_start_secs = session_start.timestamp_millis() as f64 / 1000.0;
    let session_end_secs = session_end.timestamp_millis() as f64 / 1000.0;
    let mut best: Option<(&CalendarEvent, f64, f64)> = None;
    for event in events {
        // Reject only when the windows truly don't touch. A 0-length
        // session contained inside the event window has 0.0 overlap
        // but is still a legitimate match — gate on containment, not
        // strict positive overlap.
        let touches = event.start <= session_end_secs && event.end >= session_start_secs;
        if !touches {
            continue;
        }
        let overlap_start = event.start.max(session_start_secs);
        let overlap_end = event.end.min(session_end_secs);
        let overlap = (overlap_end - overlap_start).max(0.0);
        let start_distance = (event.start - session_start_secs).abs();
        match &best {
            Some((_, best_overlap, best_dist))
                if overlap < *best_overlap
                    || (overlap == *best_overlap && start_distance >= *best_dist) => {}
            _ => best = Some((event, overlap, start_distance)),
        }
    }
    best.map(|(e, _, _)| e)
}

/// Maximum slug byte length. APFS caps each path component at 255
/// bytes; the surrounding `YYYY-MM-DD-HHMM <slug>.md` template eats
/// 22 bytes (date + space + ".md"), so 200 leaves comfortable room
/// for date-prefix changes without ever provoking ENAMETOOLONG.
const MAX_SLUG_BYTES: usize = 200;

/// Strip path-unsafe characters from a calendar title so it can sit in
/// the `YYYY-MM-DD-HHMM <slug>.md` filename template. Keeps spaces
/// (per §3.2 the template explicitly allows them) and collapses
/// whitespace runs. Empty or all-stripped titles return `"untitled"`.
fn slug_from_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '\0' | '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    // Drop pure-dot tokens during whitespace-collapse so titles like
    // ". . . ." don't survive as a residue of spaces and dots after
    // `trim_matches`. A calendar entry of ". v2 ." reduces to "v2"
    // rather than ". v2 .", and "v1.0 release" stays intact (the
    // tokens carry non-dot content).
    let collapsed: String = cleaned
        .split_whitespace()
        .filter(|p| !p.chars().all(|c| c == '.'))
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = collapsed.trim_matches('.').trim();
    if trimmed.is_empty() {
        return "untitled".to_owned();
    }
    // APFS-safe length cap on a UTF-8 char boundary. Calendar titles
    // can run hundreds of chars (shared agendas, pasted URLs); without
    // this the vault writer would silently fail with ENAMETOOLONG and
    // the session would land with `note_path = None`.
    if trimmed.len() <= MAX_SLUG_BYTES {
        return trimmed.to_owned();
    }
    let mut cut = MAX_SLUG_BYTES;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    trimmed[..cut].trim_end().to_owned()
}

/// Convert EventKit's `(name, email)` shape into heron's `Attendee`
/// shape. Each attendee gets a fresh `ItemId`; company defaults to
/// `None` (EventKit doesn't carry an org field — the LLM fills it in
/// via frontmatter merge if it can derive one from the transcript).
///
/// This minting is safe across re-summarizes because the calendar
/// path runs at first-summarize *only*: re-summarize goes through
/// [`crate::session::Orchestrator::re_summarize_note`], which feeds
/// the prior attendees (with their stable IDs from this first run)
/// to the LLM via the §10.5 ID-preservation contract — calendar is
/// not consulted again.
fn calendar_attendees_to_attendees(attendees: &[CalendarAttendee]) -> Vec<Attendee> {
    attendees
        .iter()
        .filter(|a| !a.name.trim().is_empty())
        .map(|a| Attendee {
            id: ItemId::now_v7(),
            name: a.name.clone(),
            company: None,
        })
        .collect()
}

fn derive_diarize_source(turns: &[Turn]) -> DiarizeSource {
    if turns.is_empty() {
        return DiarizeSource::Channel;
    }
    let mut has_ax = false;
    let mut has_channel = false;
    for t in turns {
        match t.speaker_source {
            SpeakerSource::Ax => has_ax = true,
            SpeakerSource::Channel => has_channel = true,
            // Self / Cluster don't shift the diarize_source bucket;
            // a Mic-only session is reported as `Channel` per §3.3.
            SpeakerSource::Self_ | SpeakerSource::Cluster => {}
        }
    }
    match (has_ax, has_channel) {
        (true, false) => DiarizeSource::Ax,
        (true, true) => DiarizeSource::Hybrid,
        (false, _) => DiarizeSource::Channel,
    }
}

/// Render a deterministic transcript-only body when the LLM is
/// unavailable. Keeps the user looking at *something* rather than an
/// empty file.
fn fallback_body(turns: &[Turn]) -> String {
    let mut out = String::from("# Transcript (no summary)\n\n");
    for t in turns {
        out.push_str(&format!("- **{}**: {}\n", t.speaker, t.text));
    }
    out
}

/// Atomic JSONL write. Mirrors `heron_vault::atomic_write`'s temp +
/// rename pattern but yields one line per turn so consumers (review
/// UI, weekly-client-summary skill) can stream without parsing the
/// whole file.
fn write_jsonl_atomic(path: &Path, turns: &[Turn]) -> Result<(), SessionError> {
    let mut buf = Vec::new();
    for t in turns {
        let line = serde_json::to_string(t)
            .map_err(|e| SessionError::Pipeline(format!("serialize turn: {e}")))?;
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    heron_vault::atomic_write(path, &buf)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::SpeakerSource;

    fn t(t0: f64, channel: Channel, source: SpeakerSource) -> Turn {
        Turn {
            t0,
            t1: t0 + 1.0,
            text: "hi".into(),
            channel,
            speaker: "x".into(),
            speaker_source: source,
            confidence: None,
        }
    }

    #[test]
    fn diarize_source_hybrid_when_both_ax_and_channel_present() {
        let turns = vec![
            t(0.0, Channel::Tap, SpeakerSource::Ax),
            t(1.0, Channel::Tap, SpeakerSource::Channel),
        ];
        assert_eq!(derive_diarize_source(&turns), DiarizeSource::Hybrid);
    }

    #[test]
    fn diarize_source_ax_when_all_ax() {
        let turns = vec![t(0.0, Channel::Tap, SpeakerSource::Ax)];
        assert_eq!(derive_diarize_source(&turns), DiarizeSource::Ax);
    }

    #[test]
    fn diarize_source_channel_when_empty() {
        assert_eq!(derive_diarize_source(&[]), DiarizeSource::Channel);
    }

    #[test]
    fn fallback_body_renders_one_bullet_per_turn() {
        let turns = vec![
            t(0.0, Channel::Mic, SpeakerSource::Self_),
            t(1.0, Channel::Tap, SpeakerSource::Channel),
        ];
        let body = fallback_body(&turns);
        assert!(body.contains("Transcript (no summary)"));
        assert_eq!(body.lines().filter(|l| l.starts_with("- ")).count(), 2);
    }

    #[test]
    fn write_jsonl_atomic_round_trips() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("nested/transcript.jsonl");
        let turns = vec![t(0.0, Channel::Mic, SpeakerSource::Self_)];
        write_jsonl_atomic(&path, &turns).expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        assert_eq!(body.lines().count(), 1);
        let back: Turn = serde_json::from_str(body.lines().next().expect("line")).expect("parse");
        assert_eq!(back.text, "hi");
    }

    // ----- slug_from_title -----

    #[test]
    fn slug_from_title_keeps_normal_titles() {
        assert_eq!(slug_from_title("Acme sync"), "Acme sync");
        assert_eq!(slug_from_title("  Acme sync  "), "Acme sync");
    }

    #[test]
    fn slug_from_title_strips_path_separators_and_collapses_whitespace() {
        assert_eq!(
            slug_from_title("foo/bar\\baz: weekly\nstandup"),
            "foo bar baz weekly standup"
        );
    }

    #[test]
    fn slug_from_title_neutralizes_dot_traversal() {
        // `.` characters that would let a title escape the meetings/
        // directory or produce a hidden dotfile must collapse to the
        // "untitled" fallback. Tests the trailing-trim guard against
        // the residual-whitespace bug Claude flagged on partial strips.
        assert_eq!(slug_from_title("..."), "untitled");
        assert_eq!(slug_from_title(". . . ."), "untitled");
        assert_eq!(slug_from_title("..foo.."), "foo");
    }

    #[test]
    fn slug_from_title_empty_or_all_whitespace_falls_back_to_untitled() {
        assert_eq!(slug_from_title(""), "untitled");
        assert_eq!(slug_from_title("   "), "untitled");
        assert_eq!(slug_from_title("\t\n\r"), "untitled");
    }

    #[test]
    fn slug_from_title_truncates_to_apfs_safe_byte_length_on_char_boundary() {
        // 250 ASCII chars exceeds MAX_SLUG_BYTES (200) — slug must
        // truncate to ≤ MAX_SLUG_BYTES so the surrounding filename
        // template stays under APFS's 255-byte component limit.
        let long = "a".repeat(250);
        let s = slug_from_title(&long);
        assert!(s.len() <= MAX_SLUG_BYTES, "got {} bytes", s.len());
        assert!(s.chars().all(|c| c == 'a'));

        // Multibyte: ✨ is 3 bytes. 100 stars = 300 bytes; truncation
        // must land on a UTF-8 char boundary (no panic, valid UTF-8).
        let stars = "✨".repeat(100);
        let s = slug_from_title(&stars);
        assert!(s.len() <= MAX_SLUG_BYTES);
        assert!(s.chars().all(|c| c == '✨'));
    }

    // ----- pick_best_calendar_event -----

    fn cal_event(title: &str, start: f64, end: f64) -> CalendarEvent {
        CalendarEvent {
            title: title.into(),
            start,
            end,
            attendees: vec![],
        }
    }

    fn dt(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).expect("in-range")
    }

    #[test]
    fn pick_best_calendar_event_returns_none_for_empty_list() {
        assert!(pick_best_calendar_event(&[], dt(1000), dt(2000)).is_none());
    }

    #[test]
    fn pick_best_calendar_event_skips_non_overlapping() {
        let events = vec![
            cal_event("before", 0.0, 500.0),
            cal_event("after", 3000.0, 4000.0),
        ];
        assert!(pick_best_calendar_event(&events, dt(1000), dt(2000)).is_none());
    }

    #[test]
    fn pick_best_calendar_event_picks_max_overlap() {
        // Session 1000–2000. Three candidates with overlaps 100, 800, 250.
        let events = vec![
            cal_event("brief", 900.0, 1100.0),
            cal_event("best", 1100.0, 1900.0),
            cal_event("tail", 1750.0, 2500.0),
        ];
        let pick = pick_best_calendar_event(&events, dt(1000), dt(2000)).expect("match");
        assert_eq!(pick.title, "best");
    }

    #[test]
    fn pick_best_calendar_event_breaks_overlap_ties_by_start_proximity() {
        // Both events fully contain the session window (overlap = full
        // session for both). Tie-break by start-distance picks the
        // event that begins closer to the session start.
        let events = vec![
            cal_event("far", 0.0, 5000.0),    // start_distance 1000s
            cal_event("near", 950.0, 5000.0), // start_distance 50s
        ];
        let pick = pick_best_calendar_event(&events, dt(1000), dt(2000)).expect("match");
        assert_eq!(pick.title, "near");
    }

    #[test]
    fn pick_best_calendar_event_accepts_zero_duration_session_inside_event() {
        // A session that starts and ends in the same instant (e.g. an
        // immediately-cancelled record) must still match an event that
        // contains that instant — gated on touches-not-overlap.
        let events = vec![cal_event("sync", 1000.0, 2000.0)];
        let pick = pick_best_calendar_event(&events, dt(1500), dt(1500)).expect("match");
        assert_eq!(pick.title, "sync");
    }

    // ----- calendar_attendees_to_attendees -----

    #[test]
    fn calendar_attendees_to_attendees_filters_empty_names() {
        let raw = vec![
            CalendarAttendee {
                name: "Alice".into(),
                email: "mailto:a@x".into(),
            },
            CalendarAttendee {
                name: "  ".into(),
                email: "mailto:b@x".into(),
            },
            CalendarAttendee {
                name: String::new(),
                email: "mailto:c@x".into(),
            },
            CalendarAttendee {
                name: "Bob".into(),
                email: "mailto:d@x".into(),
            },
        ];
        let out = calendar_attendees_to_attendees(&raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "Alice");
        assert_eq!(out[1].name, "Bob");
        // Each attendee gets a distinct fresh ID.
        assert_ne!(out[0].id, out[1].id);
        // Company stays None — EventKit doesn't carry org info.
        assert!(out.iter().all(|a| a.company.is_none()));
    }
}
