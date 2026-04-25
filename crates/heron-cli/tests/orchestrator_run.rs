//! Integration test for the real `Orchestrator::run` pipeline.
//!
//! Drives the orchestrator with in-memory stub backends so the test
//! doesn't depend on TCC / ANTHROPIC_API_KEY / network. The
//! `with_test_backends` constructor short-circuits live audio capture
//! and assumes the test seeds `<cache>/sessions/<id>/{mic,tap}.wav`,
//! which it does via a hand-rolled silent-WAV writer (the production
//! code path uses `hound::WavWriter`; this test mirrors the same
//! header so the m4a encode step's preconditions hold).

#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use heron_cli::session::{Backends, Orchestrator, SessionConfig};
use heron_llm::{LlmError, Summarizer, SummarizerInput, SummarizerOutput};
use heron_speech::{PartialWriter, ProgressFn, SttBackend, SttError, TranscribeSummary, TurnFn};
use heron_types::{
    Channel, Cost, Event, MeetingType, RecordingState, SessionClock, SessionId, SpeakerEvent,
    SpeakerSource, Turn,
};
use heron_vault::{CalendarError, CalendarEvent, CalendarReader};
use heron_zoom::{AxBackend, AxError, AxHandle};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};

const SAMPLE_RATE_HZ: u32 = 48_000;

struct StubStt;

#[async_trait]
impl SttBackend for StubStt {
    async fn ensure_model(&self, _on_progress: ProgressFn) -> Result<(), SttError> {
        Ok(())
    }
    async fn transcribe(
        &self,
        _wav_path: &Path,
        channel: Channel,
        _session_id: SessionId,
        partial_jsonl_path: &Path,
        mut on_turn: TurnFn,
    ) -> Result<TranscribeSummary, SttError> {
        // Emit one turn per channel so the merged transcript has both
        // sides represented and the diarize-source heuristic exercises
        // the Hybrid branch when paired with the AX stub.
        let (text, speaker, source) = match channel {
            Channel::Mic | Channel::MicClean => {
                ("hello from mic", "me".to_owned(), SpeakerSource::Self_)
            }
            Channel::Tap => ("hello from tap", "them".to_owned(), SpeakerSource::Channel),
        };
        let turn = Turn {
            t0: 0.0,
            t1: 0.5,
            text: text.to_owned(),
            channel,
            speaker,
            speaker_source: source,
            confidence: Some(0.9),
        };
        let mut writer = PartialWriter::create(partial_jsonl_path.to_path_buf())
            .map_err(|e| SttError::Failed(format!("partial writer: {e}")))?;
        writer
            .push(&turn)
            .map_err(|e| SttError::Failed(format!("partial push: {e}")))?;
        writer
            .finalize()
            .map_err(|e| SttError::Failed(format!("partial finalize: {e}")))?;
        on_turn(turn);
        Ok(TranscribeSummary {
            turns: 1,
            low_confidence_turns: 0,
            model: "stub".to_owned(),
            elapsed_secs: 0.001,
        })
    }
    fn name(&self) -> &'static str {
        "stub-stt"
    }
    fn is_available(&self) -> bool {
        true
    }
}

struct StubAx;

#[async_trait]
impl AxBackend for StubAx {
    async fn start(
        &self,
        _session_id: SessionId,
        _clock: SessionClock,
        _out: mpsc::Sender<SpeakerEvent>,
        _events: mpsc::Sender<Event>,
    ) -> Result<AxHandle, AxError> {
        // The orchestrator's pipeline calls AxBackend::start only on
        // the live-audio path; with `skip_audio_capture = true` this
        // method isn't reached. Return NotYetImplemented so a future
        // refactor that does invoke us fails loudly rather than
        // silently yielding zero events.
        Err(AxError::NotYetImplemented)
    }
    fn name(&self) -> &'static str {
        "stub-ax"
    }
}

struct StubLlm {
    /// Captures the rendered transcript path the pipeline handed us.
    /// The test asserts on it to confirm the orchestrator wired the
    /// merged transcript into the LLM input correctly.
    seen_transcript: Arc<Mutex<Option<PathBuf>>>,
}

#[async_trait]
impl Summarizer for StubLlm {
    async fn summarize(&self, input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError> {
        *self.seen_transcript.lock().expect("lock") = Some(input.transcript.to_path_buf());
        Ok(SummarizerOutput {
            body: "## Summary\n\nA short, generated summary.\n".to_owned(),
            company: Some("Stub Co".into()),
            meeting_type: MeetingType::Internal,
            tags: vec!["stub".into(), "test".into()],
            action_items: vec![],
            attendees: vec![],
            cost: Cost {
                summary_usd: 0.0,
                tokens_in: 1,
                tokens_out: 1,
                model: "stub-llm".into(),
            },
        })
    }
}

fn write_silent_wav(path: &Path, duration_secs: f64) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE_HZ,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    let total_samples = (SAMPLE_RATE_HZ as f64 * duration_secs) as u32;
    for _ in 0..total_samples {
        writer.write_sample(0.0_f32).expect("write sample");
    }
    writer.finalize().expect("finalize wav");
}

/// Calendar stub that always reports "no permission granted" — the
/// orchestrator's denial-contract path falls through to LLM-inferred
/// attendees + the "untitled" slug, matching the production behavior
/// on a machine where the user skipped step 4 of onboarding.
struct StubCalendarDenied;

impl CalendarReader for StubCalendarDenied {
    fn read_window(
        &self,
        _start_utc: DateTime<Utc>,
        _end_utc: DateTime<Utc>,
    ) -> Result<Option<Vec<CalendarEvent>>, CalendarError> {
        Ok(None)
    }
}

/// Calendar stub that returns one canned event covering the session
/// window. Lets a focused test pin that the pipeline picks up the
/// title for the slug and the attendees for the frontmatter.
struct StubCalendarOneEvent {
    event: CalendarEvent,
}

impl CalendarReader for StubCalendarOneEvent {
    fn read_window(
        &self,
        _start_utc: DateTime<Utc>,
        _end_utc: DateTime<Utc>,
    ) -> Result<Option<Vec<CalendarEvent>>, CalendarError> {
        Ok(Some(vec![self.event.clone()]))
    }
}

fn build_backends() -> (Backends, Arc<Mutex<Option<PathBuf>>>) {
    let seen = Arc::new(Mutex::new(None));
    let llm = StubLlm {
        seen_transcript: seen.clone(),
    };
    let backends: Backends = (
        Box::new(StubStt),
        Box::new(StubAx),
        Box::new(llm),
        Box::new(StubCalendarDenied),
    );
    (backends, seen)
}

#[tokio::test]
async fn run_pipeline_with_stub_backends_writes_markdown_note() {
    let tmp = TempDir::new().expect("tmpdir");
    let session_id = SessionId::from_u128(0x0193_1f00_dead_beef);
    let cache_dir = tmp.path().join("cache");
    let vault_root = tmp.path().join("vault");
    let session_dir = cache_dir.join("sessions").join(session_id.to_string());

    // Seed the WAVs the pipeline expects under skip_audio_capture.
    write_silent_wav(&session_dir.join("mic.wav"), 0.5);
    write_silent_wav(&session_dir.join("tap.wav"), 0.5);

    let cfg = SessionConfig {
        session_id,
        target_bundle_id: "us.zoom.xos".into(),
        cache_dir,
        vault_root: vault_root.clone(),
        stt_backend_name: "sherpa".into(),
        llm_preference: heron_llm::Preference::Auto,
    };
    let (backends, seen_transcript) = build_backends();
    let mut orch = Orchestrator::with_test_backends(cfg, backends);

    // Drive a 0.5s "recording" by firing stop_rx after 50 ms — the
    // stub backends don't depend on timing, so a short delay keeps
    // the test fast while still exercising the await path.
    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = stop_tx.send(());
    });

    let outcome = orch.run(stop_rx).await.expect("run");
    assert_eq!(outcome.final_state, RecordingState::Idle);

    let note_path = outcome.note_path.expect("note path returned");
    assert!(note_path.exists(), "note file must exist on disk");
    assert!(
        note_path.starts_with(&vault_root),
        "note must be under vault"
    );
    let body = std::fs::read_to_string(&note_path).expect("read note");
    assert!(body.contains("---\n"), "note must have YAML frontmatter");
    assert!(
        body.contains("A short, generated summary."),
        "summary body must be present"
    );

    // The transcript JSONL the LLM saw must be the same path the
    // vault writer persisted in the frontmatter.
    let seen = seen_transcript.lock().expect("lock").clone().expect("seen");
    let transcript_dir = vault_root.join("transcripts");
    assert!(
        seen.starts_with(&transcript_dir),
        "transcript path must live under vault/transcripts"
    );
    let transcript_body = std::fs::read_to_string(&seen).expect("read transcript");
    assert!(transcript_body.contains("hello from mic"));
    assert!(transcript_body.contains("hello from tap"));
}

/// Stub LLM that fails every call. Pins `pipeline::run_pipeline`'s
/// summarize-error branch: when the summarizer errors, the pipeline
/// must still write a transcript-only fallback note rather than
/// aborting the session.
struct StubLlmFailing;

#[async_trait]
impl Summarizer for StubLlmFailing {
    async fn summarize(&self, _input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError> {
        Err(LlmError::Backend("stub: summarizer is unhappy".into()))
    }
}

#[tokio::test]
async fn run_pipeline_with_failing_llm_writes_fallback_note() {
    // Pipeline's contract per §4.1: an LLM hiccup is non-fatal — we
    // still write a transcript-only note so the user has the raw
    // turns. This test exercises the `summarize → Err` branch and
    // the `fallback_body` rendering path.
    let tmp = TempDir::new().expect("tmpdir");
    let session_id = SessionId::from_u128(0x0193_1f00_face_d00d);
    let cache_dir = tmp.path().join("cache");
    let vault_root = tmp.path().join("vault");
    let session_dir = cache_dir.join("sessions").join(session_id.to_string());

    write_silent_wav(&session_dir.join("mic.wav"), 0.5);
    write_silent_wav(&session_dir.join("tap.wav"), 0.5);

    let cfg = SessionConfig {
        session_id,
        target_bundle_id: "us.zoom.xos".into(),
        cache_dir,
        vault_root: vault_root.clone(),
        stt_backend_name: "sherpa".into(),
        llm_preference: heron_llm::Preference::Auto,
    };
    let backends: Backends = (
        Box::new(StubStt),
        Box::new(StubAx),
        Box::new(StubLlmFailing),
        Box::new(StubCalendarDenied),
    );
    let mut orch = Orchestrator::with_test_backends(cfg, backends);

    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = stop_tx.send(());
    });

    let outcome = orch.run(stop_rx).await.expect("run");
    assert_eq!(outcome.final_state, RecordingState::Idle);

    // Note must still exist and contain the fallback body, not the
    // (would-be) LLM summary.
    let note_path = outcome.note_path.expect("note path returned");
    let body = std::fs::read_to_string(&note_path).expect("read note");
    assert!(
        body.contains("Transcript (no summary)"),
        "fallback body must render when LLM fails"
    );
    // Both stub turns survive into the fallback transcript.
    assert!(body.contains("hello from mic"));
    assert!(body.contains("hello from tap"));
    // The (no summarizer) cost line should be present in frontmatter
    // — pins the `MeetingType::Other` / zero-cost defaults branch.
    assert!(body.contains("(no summarizer)"));
}

/// Stub STT that emits zero turns. Pins the empty-transcript branch:
/// the pipeline must finalize cleanly even when both channels
/// produce no audio (silent meeting, or upstream STT skipped).
struct StubSttEmpty;

#[async_trait]
impl SttBackend for StubSttEmpty {
    async fn ensure_model(&self, _on_progress: ProgressFn) -> Result<(), SttError> {
        Ok(())
    }
    async fn transcribe(
        &self,
        _wav_path: &Path,
        _channel: Channel,
        _session_id: SessionId,
        partial_jsonl_path: &Path,
        _on_turn: TurnFn,
    ) -> Result<TranscribeSummary, SttError> {
        // Open + finalize the partial writer immediately so the
        // 0-segment-but-valid-artifact contract from §3.5 holds.
        let writer = PartialWriter::create(partial_jsonl_path.to_path_buf())
            .map_err(|e| SttError::Failed(format!("partial writer: {e}")))?;
        writer
            .finalize()
            .map_err(|e| SttError::Failed(format!("partial finalize: {e}")))?;
        Ok(TranscribeSummary {
            turns: 0,
            low_confidence_turns: 0,
            model: "stub-empty".to_owned(),
            elapsed_secs: 0.001,
        })
    }
    fn name(&self) -> &'static str {
        "stub-empty-stt"
    }
    fn is_available(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn run_pipeline_with_empty_stt_finalizes_to_idle_with_note() {
    let tmp = TempDir::new().expect("tmpdir");
    let session_id = SessionId::from_u128(0x0193_1f00_5111_1111);
    let cache_dir = tmp.path().join("cache");
    let vault_root = tmp.path().join("vault");
    let session_dir = cache_dir.join("sessions").join(session_id.to_string());

    write_silent_wav(&session_dir.join("mic.wav"), 0.5);
    write_silent_wav(&session_dir.join("tap.wav"), 0.5);

    let cfg = SessionConfig {
        session_id,
        target_bundle_id: "us.zoom.xos".into(),
        cache_dir,
        vault_root: vault_root.clone(),
        stt_backend_name: "sherpa".into(),
        llm_preference: heron_llm::Preference::Auto,
    };
    let llm_seen = Arc::new(Mutex::new(None));
    let llm = StubLlm {
        seen_transcript: Arc::clone(&llm_seen),
    };
    let backends: Backends = (
        Box::new(StubSttEmpty),
        Box::new(StubAx),
        Box::new(llm),
        Box::new(StubCalendarDenied),
    );
    let mut orch = Orchestrator::with_test_backends(cfg, backends);

    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = stop_tx.send(());
    });

    let outcome = orch.run(stop_rx).await.expect("run");
    assert_eq!(outcome.final_state, RecordingState::Idle);

    // The transcript file under <vault>/transcripts/ exists but is
    // empty (zero JSONL lines). The orchestrator must not skip the
    // write — downstream consumers rely on the path being present.
    let transcript_path = vault_root
        .join("transcripts")
        .join(format!("{session_id}.jsonl"));
    assert!(
        transcript_path.exists(),
        "transcript file must exist even when empty"
    );
    let transcript_body = std::fs::read_to_string(&transcript_path).expect("read transcript");
    assert!(
        transcript_body.is_empty() || transcript_body.lines().count() == 0,
        "transcript must be empty when STT emits zero turns, got {transcript_body:?}"
    );

    // Note still writes (LLM runs over empty transcript and returns
    // the canned StubLlm body). Pins that the orchestrator doesn't
    // shortcut to a transcript-only fallback when the transcript is
    // empty but the LLM is healthy.
    assert!(outcome.note_path.is_some());
    let note_path = outcome.note_path.expect("note path");
    let body = std::fs::read_to_string(&note_path).expect("read note");
    assert!(body.contains("A short, generated summary."));
}

#[tokio::test]
async fn run_pipeline_with_missing_wavs_finalizes_with_note() {
    // No WAVs seeded — `run_stt` graceful-skips both channels
    // (`if !wav.exists()` branch in pipeline.rs), `encode_to_m4a`
    // fails because the source files don't exist (or the m4a-verify
    // step rejects the resulting empty file), and the cache is
    // retained per the §12.3 "ringbuffer survives an aborted
    // session" contract. Pipeline must still finalize to Idle with
    // a note rendered against the (empty) transcript so the user
    // has *something* in their vault.
    let tmp = TempDir::new().expect("tmpdir");
    let session_id = SessionId::from_u128(0x0193_1f00_dead_5555);
    let cache_dir = tmp.path().join("cache");
    let vault_root = tmp.path().join("vault");

    let cfg = SessionConfig {
        session_id,
        target_bundle_id: "us.zoom.xos".into(),
        cache_dir: cache_dir.clone(),
        vault_root: vault_root.clone(),
        stt_backend_name: "sherpa".into(),
        llm_preference: heron_llm::Preference::Auto,
    };
    let (backends, _seen) = build_backends();
    let mut orch = Orchestrator::with_test_backends(cfg, backends);

    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = stop_tx.send(());
    });

    let outcome = orch.run(stop_rx).await.expect("run");
    assert_eq!(outcome.final_state, RecordingState::Idle);
    assert!(
        outcome.note_path.is_some(),
        "note must write even with no audio"
    );

    // The session cache directory must still exist — m4a-verify
    // failure routes us through the retain-cache branch.
    let session_dir = cache_dir.join("sessions").join(session_id.to_string());
    assert!(
        session_dir.exists(),
        "ringbuffer cache must be retained on encode/verify failure"
    );
}

#[tokio::test]
async fn run_pipeline_uses_calendar_event_for_slug_and_attendees() {
    // Pin the calendar wiring: when the calendar reader returns a
    // single event covering the session window, the pipeline must
    // (1) lift the event title into the filename slug per §3.2 and
    // (2) override LLM-inferred attendees with the calendar-supplied
    // attendees per §5 wks 7–8.
    let tmp = TempDir::new().expect("tmpdir");
    let session_id = SessionId::from_u128(0x0193_1f00_ca1e_d000);
    let cache_dir = tmp.path().join("cache");
    let vault_root = tmp.path().join("vault");
    let session_dir = cache_dir.join("sessions").join(session_id.to_string());

    write_silent_wav(&session_dir.join("mic.wav"), 0.5);
    write_silent_wav(&session_dir.join("tap.wav"), 0.5);

    let now_secs = Utc::now().timestamp() as f64;
    let event = CalendarEvent {
        title: "Acme sync".into(),
        // ±60 s around the call so the event fully spans the session.
        start: now_secs - 60.0,
        end: now_secs + 600.0,
        attendees: vec![
            heron_vault::CalendarAttendee {
                name: "Alice Anderson".into(),
                email: "mailto:alice@acme.test".into(),
            },
            heron_vault::CalendarAttendee {
                name: "Bob Brown".into(),
                email: "mailto:bob@acme.test".into(),
            },
        ],
    };

    let cfg = SessionConfig {
        session_id,
        target_bundle_id: "us.zoom.xos".into(),
        cache_dir,
        vault_root: vault_root.clone(),
        stt_backend_name: "sherpa".into(),
        llm_preference: heron_llm::Preference::Auto,
    };
    let llm_seen = Arc::new(Mutex::new(None));
    let llm = StubLlm {
        seen_transcript: Arc::clone(&llm_seen),
    };
    let backends: Backends = (
        Box::new(StubStt),
        Box::new(StubAx),
        Box::new(llm),
        Box::new(StubCalendarOneEvent { event }),
    );
    let mut orch = Orchestrator::with_test_backends(cfg, backends);

    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = stop_tx.send(());
    });

    let outcome = orch.run(stop_rx).await.expect("run");
    let note_path = outcome.note_path.expect("note path returned");
    let filename = note_path
        .file_name()
        .and_then(|s| s.to_str())
        .expect("utf-8 filename");
    assert!(
        filename.contains("Acme sync"),
        "filename {filename:?} must include the calendar title"
    );
    assert!(
        !filename.contains("untitled"),
        "calendar title must replace the 'untitled' fallback"
    );

    let body = std::fs::read_to_string(&note_path).expect("read note");
    assert!(
        body.contains("Alice Anderson"),
        "calendar attendee Alice must reach the frontmatter"
    );
    assert!(
        body.contains("Bob Brown"),
        "calendar attendee Bob must reach the frontmatter"
    );
}
