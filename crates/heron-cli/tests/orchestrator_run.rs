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
use heron_cli::session::{Backends, Orchestrator, SessionConfig};
use heron_llm::{LlmError, Summarizer, SummarizerInput, SummarizerOutput};
use heron_speech::{PartialWriter, ProgressFn, SttBackend, SttError, TranscribeSummary, TurnFn};
use heron_types::{
    Channel, Cost, Event, MeetingType, RecordingState, SessionClock, SessionId, SpeakerEvent,
    SpeakerSource, Turn,
};
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

fn build_backends() -> (Backends, Arc<Mutex<Option<PathBuf>>>) {
    let seen = Arc::new(Mutex::new(None));
    let llm = StubLlm {
        seen_transcript: seen.clone(),
    };
    let backends: Backends = (Box::new(StubStt), Box::new(StubAx), Box::new(llm));
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
