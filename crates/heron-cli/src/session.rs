//! Session orchestrator.
//!
//! Drives one session from `idle → armed → recording → transcribing
//! → summarizing → idle` per [`heron_types::RecordingFsm`], wiring
//! [`heron_audio::AudioCapture`], [`heron_speech::SttBackend`],
//! [`heron_zoom::AxBackend`], [`heron_llm::Summarizer`], and
//! [`heron_vault::VaultWriter`] into the §4.1 data flow described in
//! `docs/plan.md`.
//!
//! Two entry points are exposed:
//!
//! - [`Orchestrator::run_no_op`] — synchronous FSM-only walk, used by
//!   the test suite + the `--no-op` CLI flag for environments without
//!   TCC permissions.
//! - [`Orchestrator::run`] — the real async pipeline.

// Several fields and methods are part of the orchestrator's public
// shape but only exercised by tests + cmd_record's smoke wiring
// today. Real consumers (Tauri command handlers, week-11) will read
// every field. Allow dead_code until then so clippy's deny-warnings
// CI gate doesn't push us to delete shape we know we'll need.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use heron_llm::{Summarizer, SummarizerInput, SummarizerOutput};
use heron_types::{
    IdleReason, MeetingType, RecordingFsm, RecordingState, SessionId, SummaryOutcome,
};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

use crate::pipeline;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("recording-flow transition rejected: {0}")]
    Transition(#[from] heron_types::TransitionError),
    #[error("audio capture failed: {0}")]
    Audio(#[from] heron_audio::AudioError),
    #[error("STT failed: {0}")]
    Stt(#[from] heron_speech::SttError),
    #[error("AX failed: {0}")]
    Ax(#[from] heron_zoom::AxError),
    #[error("LLM failed: {0}")]
    Llm(#[from] heron_llm::LlmError),
    #[error("vault writer failed: {0}")]
    Vault(#[from] heron_vault::VaultError),
    #[error("m4a encode/verify failed: {0}")]
    Encode(#[from] heron_vault::EncodeError),
    #[error("partial-jsonl writer failed: {0}")]
    Partial(#[from] heron_speech::PartialWriterError),
    #[error("crash-recovery state write failed: {0}")]
    Recovery(#[from] heron_types::RecoveryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("session pipeline aborted: {0}")]
    Pipeline(String),
}

/// Configuration the orchestrator needs to start a session.
///
/// Most fields are user-supplied via the CLI / Tauri shell. The
/// orchestrator owns the lifetime of all derived state.
pub struct SessionConfig {
    pub session_id: SessionId,
    pub target_bundle_id: String,
    pub cache_dir: PathBuf,
    pub vault_root: PathBuf,
    /// `"whisperkit"` or `"sherpa"`; resolves via
    /// [`heron_speech::build_backend`].
    pub stt_backend_name: String,
    /// User-expressed LLM backend preference. Phase 40's selector
    /// picks the first viable backend under this preference at
    /// `backends()` call time; the returned `SelectionReason` is
    /// surfaced via tracing so the diagnostics tab can render
    /// "we picked X because Y".
    pub llm_preference: heron_llm::Preference,
}

/// Outcome of `run_no_op` and `run`.
#[derive(Debug)]
pub struct SessionOutcome {
    pub final_state: RecordingState,
    pub last_idle_reason: Option<IdleReason>,
    pub note_path: Option<PathBuf>,
}

/// The trait-object trio every consumer needs to drive a session.
/// Type alias rather than a struct so consumers can destructure
/// directly without a `.0/.1/.2` indexing tax.
pub type Backends = (
    Box<dyn heron_speech::SttBackend>,
    Box<dyn heron_zoom::AxBackend>,
    Box<dyn heron_llm::Summarizer>,
);

/// Orchestrates one session from arm → record → transcribe →
/// summarize → idle. The struct is intentionally cheap to construct
/// so the CLI / Tauri shell can drop and re-create it per session.
pub struct Orchestrator {
    config: SessionConfig,
    fsm: RecordingFsm,
    /// Optional pre-built backends. When `None`, [`Orchestrator::run`]
    /// resolves them from config via [`Orchestrator::backends`]. The
    /// integration test path injects stub backends here so it doesn't
    /// touch the host environment (TCC, network, ANTHROPIC_API_KEY).
    /// `Mutex` because `run` consumes the inner value via `take()` and
    /// needs interior mutability behind `&mut self` callers without
    /// requiring `&mut self` on construction.
    injected: Arc<Mutex<Option<Backends>>>,
    /// When `true`, [`Orchestrator::run`] skips the live
    /// `AudioCapture::start` call and assumes the caller seeded
    /// `<cache>/sessions/<id>/{mic,tap}.wav` ahead of time. Set by
    /// [`Orchestrator::with_test_backends`] for the integration-test
    /// path; production CLI / Tauri callers leave it `false`.
    skip_audio_capture: bool,
}

impl Orchestrator {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            fsm: RecordingFsm::new(),
            injected: Arc::new(Mutex::new(None)),
            skip_audio_capture: false,
        }
    }

    /// Construct an orchestrator with pre-built backends. Live audio
    /// capture is still attempted — useful when a caller wants real
    /// PCM but a non-default LLM (e.g., a hosted backend that needs
    /// custom auth headers).
    pub fn with_backends(config: SessionConfig, backends: Backends) -> Self {
        Self {
            config,
            fsm: RecordingFsm::new(),
            injected: Arc::new(Mutex::new(Some(backends))),
            skip_audio_capture: false,
        }
    }

    /// Test-only constructor: inject backends AND skip live audio
    /// capture. Caller must seed `<cache>/sessions/<id>/{mic,tap}.wav`
    /// before calling [`Orchestrator::run`].
    pub fn with_test_backends(config: SessionConfig, backends: Backends) -> Self {
        Self {
            config,
            fsm: RecordingFsm::new(),
            injected: Arc::new(Mutex::new(Some(backends))),
            skip_audio_capture: true,
        }
    }

    /// Drive the FSM through the full happy-path transitions without
    /// actually invoking any backends. Used today for testing the
    /// wiring; real `run()` is below.
    pub fn run_no_op(&mut self, summary: SummaryOutcome) -> Result<SessionOutcome, SessionError> {
        // idle → armed
        self.fsm.on_hotkey()?;
        // armed → recording
        self.fsm.on_yes()?;
        // recording → transcribing
        self.fsm.on_hotkey()?;
        // transcribing → summarizing
        self.fsm.on_transcribe_done()?;
        // summarizing → idle
        self.fsm.on_summary(summary)?;
        Ok(SessionOutcome {
            final_state: self.fsm.state(),
            last_idle_reason: self.fsm.last_idle_reason(),
            note_path: None,
        })
    }

    /// Real session pipeline. Drives audio capture → STT → aligner →
    /// JSONL → m4a encode → LLM summarize → vault writer.
    ///
    /// `stop_rx` fires when the user releases the hotkey or the CLI
    /// receives Ctrl-C. Any backend failure downstream of "recording"
    /// is logged and the FSM is still walked to `idle` with the
    /// appropriate [`IdleReason`] — partial outputs (transcript only,
    /// no summary) are preferable to a panic that loses the session.
    pub async fn run(
        &mut self,
        stop_rx: oneshot::Receiver<()>,
    ) -> Result<SessionOutcome, SessionError> {
        let backends = match self.injected.lock().await.take() {
            Some(b) => b,
            None => self.backends()?,
        };
        // FSM walks idle → armed → recording inside `run_pipeline` so
        // the integration-test path that injects backends still goes
        // through the same transition guards as the live path.
        let outcome = pipeline::run_pipeline(
            &mut self.fsm,
            &self.config,
            backends,
            stop_rx,
            self.skip_audio_capture,
        )
        .await;
        match outcome {
            Ok(out) => Ok(out),
            Err(e) => {
                tracing::error!(error = %e, "orchestrator pipeline failed");
                Err(e)
            }
        }
    }

    /// Try to start the audio capture. Returns the live handle; surfaces
    /// `AudioError::NotYetImplemented` on non-Apple builds.
    pub async fn try_start_audio(&self) -> Result<heron_audio::AudioCaptureHandle, SessionError> {
        let handle = heron_audio::AudioCapture::start(
            self.config.session_id,
            &self.config.target_bundle_id,
            &self.config.cache_dir,
        )
        .await?;
        Ok(handle)
    }

    /// Helper that builds the trait-object backends from config so
    /// downstream code (Tauri commands, CLI subcommands) can consume
    /// them without re-deriving the selection logic.
    ///
    /// LLM selection is driven by the
    /// [`heron_llm::select_summarizer`] selector — the chosen backend
    /// + reason are emitted via `tracing::info!` so an operator
    ///   inspecting logs can tell whether the API path was picked or a
    ///   CLI fallback fired.
    pub fn backends(&self) -> Result<Backends, SessionError> {
        let stt = heron_speech::build_backend(&self.config.stt_backend_name)?;
        let ax = heron_zoom::select_ax_backend();
        let (llm, backend, reason) = heron_llm::select_summarizer(self.config.llm_preference)?;
        tracing::info!(?backend, ?reason, "LLM backend selected");
        Ok((stt, ax, llm))
    }

    pub fn state(&self) -> RecordingState {
        self.fsm.state()
    }

    /// Re-summarize an existing meeting note while honoring the §10.5
    /// ID-preservation contract.
    ///
    /// Reads the prior `action_items` + `attendees` from the current
    /// `<note>.md` (per §11.2 — *not* `.md.bak`), passes them to the
    /// LLM via [`SummarizerInput::existing_action_items`] /
    /// `existing_attendees` so the prompt-side preservation block
    /// fires (§10.5 layer 1), then runs the text-similarity matcher
    /// in [`heron_vault::match_action_items_by_text`] over the LLM's
    /// output to rewrite any LLM-minted IDs back to their base
    /// counterparts (§10.5 layer 2). The resulting `SummarizerOutput`
    /// has IDs that the §10.3 merge can rely on.
    ///
    /// Today the caller still owns the `Summarizer` (the orchestrator
    /// would `select_summarizer()` in `backends()` but holding a live
    /// Summarizer across the full session lifetime is the next phase
    /// of wiring). This shape lets a unit test inject a no-op
    /// summarizer and assert the prior items reach
    /// [`render_meeting_prompt`] / the matcher.
    pub async fn re_summarize_note(
        &self,
        summarizer: &dyn Summarizer,
        note_path: &Path,
        meeting_type: MeetingType,
        transcript: &Path,
    ) -> Result<SummarizerOutput, SessionError> {
        // Layer-1: read prior items (action_items + attendees) from
        // the current `<note>.md` and pass them to the LLM. The
        // §10.5 prompt block then asks the model to RETURN THE EXACT
        // SAME `id` for items that mean the same thing as before.
        // `as_summarizer_inputs` maps empty → `None` so the prompt
        // block stays out on a first summarize.
        let prior = heron_vault::read_prior_items(note_path)?;
        let (existing_action_items, existing_attendees) = prior.as_summarizer_inputs();

        let input = SummarizerInput {
            transcript,
            meeting_type,
            existing_action_items,
            existing_attendees,
        };
        let mut output = summarizer.summarize(input).await?;

        // Layer-2: the LLM may have minted fresh UUIDs for items it
        // should have preserved IDs for (the §10.5 contract floor is
        // 80%, not 100%). Run the text-similarity fallback to rewrite
        // those IDs back to the matching base IDs so the standard
        // §10.3 merge resolves them per stable `ItemId`.
        if !prior.action_items.is_empty() {
            let matches =
                heron_vault::match_action_items_by_text(&prior.action_items, &output.action_items);
            let rewrites = heron_vault::apply_matches(&mut output.action_items, &matches);
            if rewrites > 0 {
                tracing::info!(
                    rewrites,
                    prior = prior.action_items.len(),
                    fresh = output.action_items.len(),
                    "ID-preservation layer-2 matcher rewrote LLM-minted IDs to base IDs"
                );
            }
        }

        Ok(output)
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg() -> SessionConfig {
        SessionConfig {
            session_id: SessionId::nil(),
            target_bundle_id: "us.zoom.xos".into(),
            cache_dir: PathBuf::from("/tmp/heron-test-cache"),
            vault_root: PathBuf::from("/tmp/heron-test-vault"),
            stt_backend_name: "sherpa".into(),
            // Auto picks Anthropic when ANTHROPIC_API_KEY is set,
            // else falls back to a CLI; the test machine's
            // environment determines which path runs.
            llm_preference: heron_llm::Preference::Auto,
        }
    }

    #[test]
    fn run_no_op_drives_full_happy_path() {
        let mut orch = Orchestrator::new(cfg());
        let outcome = orch.run_no_op(SummaryOutcome::Done).expect("run");
        assert_eq!(outcome.final_state, RecordingState::Idle);
        assert_eq!(outcome.last_idle_reason, Some(IdleReason::SummaryDone));
    }

    #[test]
    fn run_no_op_propagates_summary_failure() {
        let mut orch = Orchestrator::new(cfg());
        let outcome = orch.run_no_op(SummaryOutcome::Failed).expect("run");
        assert_eq!(outcome.last_idle_reason, Some(IdleReason::SummaryFailed));
    }

    /// Off-Apple targets still get `NotYetImplemented` from
    /// `AudioCapture::start` — the cidre process tap only compiles
    /// on macOS.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn audio_start_returns_not_yet_implemented_off_apple() {
        let orch = Orchestrator::new(cfg());
        let result = orch.try_start_audio().await;
        match result {
            Err(SessionError::Audio(heron_audio::AudioError::NotYetImplemented)) => {}
            Err(other) => panic!("expected NotYetImplemented, got {other:?}"),
            Ok(_) => panic!("expected NotYetImplemented, got Ok(_handle)"),
        }
    }

    /// On macOS we now exercise the real Core Audio process tap path.
    /// On a CI runner without TCC granted this surfaces as
    /// `PermissionDenied` / `ProcessNotFound` / `Aborted`; on a dev
    /// machine with the meeting app NOT running it surfaces as
    /// `ProcessNotFound`. What we lock down here is that we never
    /// regress back to `NotYetImplemented`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn audio_start_does_not_return_not_yet_implemented_on_macos() {
        let orch = Orchestrator::new(cfg());
        let result = orch.try_start_audio().await;
        match result {
            Err(SessionError::Audio(heron_audio::AudioError::NotYetImplemented)) => {
                panic!("macOS branch must not return NotYetImplemented");
            }
            Err(SessionError::Audio(_)) | Ok(_) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn backends_resolve_for_each_supported_backend() {
        let orch = Orchestrator::new(cfg());
        // The LLM selector probes the live environment for a key /
        // `claude` / `codex`. CI runners often have none of those,
        // which surfaces as Err(SessionError::Llm(_)). The contract
        // we want to pin: when the selector DOES return a backend,
        // the STT + AX shapes are correct.
        match orch.backends() {
            Ok((stt, ax, _llm)) => {
                assert_eq!(stt.name(), "sherpa");
                assert_eq!(ax.name(), "ax-observer");
            }
            Err(SessionError::Llm(_)) => {
                // No LLM backend available on this host; STT + AX
                // would have resolved fine — exercised by other
                // tests at the heron-speech / heron-zoom layer.
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn backends_errors_on_unknown_stt() {
        let mut config = cfg();
        config.stt_backend_name = "magic".into();
        let orch = Orchestrator::new(config);
        let result = orch.backends();
        assert!(matches!(result, Err(SessionError::Stt(_))));
    }

    #[test]
    fn state_starts_idle() {
        let orch = Orchestrator::new(cfg());
        assert_eq!(orch.state(), RecordingState::Idle);
    }

    /// Capturing summarizer: records the inputs it received so the
    /// test can assert on `existing_action_items` / `existing_attendees`,
    /// and returns a deterministic [`SummarizerOutput`] (with one
    /// LLM-minted action-item ID — to exercise the §10.5 layer-2
    /// rewrite path).
    struct CapturingSummarizer {
        captured_action_items: std::sync::Mutex<Option<Vec<heron_types::ActionItem>>>,
        captured_attendees: std::sync::Mutex<Option<Vec<heron_types::Attendee>>>,
        canned_output: std::sync::Mutex<Option<SummarizerOutput>>,
    }

    #[async_trait::async_trait]
    impl heron_llm::Summarizer for CapturingSummarizer {
        async fn summarize(
            &self,
            input: SummarizerInput<'_>,
        ) -> Result<SummarizerOutput, heron_llm::LlmError> {
            *self.captured_action_items.lock().expect("lock") =
                input.existing_action_items.map(<[_]>::to_vec);
            *self.captured_attendees.lock().expect("lock") =
                input.existing_attendees.map(<[_]>::to_vec);
            self.canned_output
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| heron_llm::LlmError::Backend("test fixture exhausted".into()))
        }
    }

    #[tokio::test]
    async fn re_summarize_note_threads_prior_items_to_summarizer_and_preserves_ids() {
        use heron_types::{
            ActionItem, Attendee, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter,
            ItemId,
        };
        use heron_vault::VaultWriter;

        // Lay down a finalized note with one action item + one
        // attendee in its frontmatter. The base ID is what the
        // ID-preservation contract must keep alive across re-runs.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let base_action_id = ItemId::from_u128(0xA1);
        let base_attendee_id = ItemId::from_u128(0xB1);
        let prior_action = ActionItem {
            id: base_action_id,
            owner: "alice".into(),
            text: "Send pricing deck to Acme".into(),
            due: None,
        };
        let prior_attendee = Attendee {
            id: base_attendee_id,
            name: "Alice".into(),
            company: Some("Acme".into()),
        };
        let frontmatter = Frontmatter {
            date: chrono::NaiveDate::from_ymd_opt(2026, 4, 24).expect("date"),
            start: "14:00".into(),
            duration_min: 47,
            company: Some("Acme".into()),
            attendees: vec![prior_attendee.clone()],
            meeting_type: heron_types::MeetingType::Client,
            source_app: "us.zoom.xos".into(),
            recording: PathBuf::from("recordings/2026-04-24-1400.m4a"),
            transcript: PathBuf::from("transcripts/2026-04-24-1400.jsonl"),
            diarize_source: DiarizeSource::Ax,
            disclosed: Disclosure {
                stated: true,
                when: Some("00:14".into()),
                how: DisclosureHow::Verbal,
            },
            cost: Cost {
                summary_usd: 0.04,
                tokens_in: 14_231,
                tokens_out: 612,
                model: "claude-sonnet-4-6".into(),
            },
            action_items: vec![prior_action.clone()],
            tags: vec!["acme".into()],
            extra: serde_yaml::Mapping::default(),
        };
        let note_path = writer
            .finalize_session("2026-04-24", "1400", "acme", &frontmatter, "Body.\n")
            .expect("finalize");

        // The "LLM" mints a fresh UUID for the same item — exactly
        // the failure mode the §10.5 layer-2 matcher exists to fix.
        let minted_id = ItemId::from_u128(0xDEADBEEF);
        let canned = SummarizerOutput {
            body: "Polished body.".into(),
            company: Some("Acme".into()),
            meeting_type: heron_types::MeetingType::Client,
            tags: vec!["acme".into()],
            action_items: vec![ActionItem {
                id: minted_id,
                owner: "alice".into(),
                text: "Send the pricing deck to Acme".into(),
                due: None,
            }],
            attendees: vec![prior_attendee.clone()],
            cost: Cost {
                summary_usd: 0.05,
                tokens_in: 1000,
                tokens_out: 200,
                model: "claude-sonnet-4-6".into(),
            },
        };

        let summarizer = CapturingSummarizer {
            captured_action_items: std::sync::Mutex::new(None),
            captured_attendees: std::sync::Mutex::new(None),
            canned_output: std::sync::Mutex::new(Some(canned)),
        };

        let orch = Orchestrator::new(cfg());
        let transcript = PathBuf::from("/tmp/heron-test-transcript.jsonl");
        let output = orch
            .re_summarize_note(
                &summarizer,
                &note_path,
                heron_types::MeetingType::Client,
                &transcript,
            )
            .await
            .expect("re_summarize_note");

        // Assertion 1: the orchestrator handed the prior items to
        // the summarizer (layer-1 prompt-side preservation can fire).
        let captured_actions = summarizer
            .captured_action_items
            .lock()
            .expect("lock")
            .clone()
            .expect("existing_action_items must be Some on a re-summarize");
        assert_eq!(captured_actions, vec![prior_action.clone()]);
        let captured_attendees = summarizer
            .captured_attendees
            .lock()
            .expect("lock")
            .clone()
            .expect("existing_attendees must be Some on a re-summarize");
        assert_eq!(captured_attendees, vec![prior_attendee.clone()]);

        // Assertion 2: the layer-2 text-similarity matcher rewrote
        // the LLM's minted id back to the base id — the §10.5
        // contract is honored even when the LLM ignores layer-1.
        assert_eq!(output.action_items.len(), 1);
        assert_eq!(
            output.action_items[0].id, base_action_id,
            "layer-2 matcher must rewrite minted id {minted_id:?} back to base id {base_action_id:?}"
        );
    }

    #[tokio::test]
    async fn re_summarize_note_omits_prior_items_when_note_has_none() {
        use heron_types::{
            Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, MeetingType,
        };
        use heron_vault::VaultWriter;

        let tmp = tempfile::TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let frontmatter = Frontmatter {
            date: chrono::NaiveDate::from_ymd_opt(2026, 4, 24).expect("date"),
            start: "14:00".into(),
            duration_min: 30,
            company: None,
            attendees: vec![],
            meeting_type: MeetingType::Other,
            source_app: "us.zoom.xos".into(),
            recording: PathBuf::from("r.m4a"),
            transcript: PathBuf::from("t.jsonl"),
            diarize_source: DiarizeSource::Channel,
            disclosed: Disclosure {
                stated: false,
                when: None,
                how: DisclosureHow::None,
            },
            cost: Cost {
                summary_usd: 0.0,
                tokens_in: 0,
                tokens_out: 0,
                model: "stub".into(),
            },
            action_items: vec![],
            tags: vec![],
            extra: serde_yaml::Mapping::default(),
        };
        let note_path = writer
            .finalize_session("2026-04-24", "1400", "x", &frontmatter, "Body.\n")
            .expect("finalize");

        let canned = SummarizerOutput {
            body: "Body.".into(),
            company: None,
            meeting_type: MeetingType::Other,
            tags: vec![],
            action_items: vec![],
            attendees: vec![],
            cost: Cost {
                summary_usd: 0.0,
                tokens_in: 0,
                tokens_out: 0,
                model: "stub".into(),
            },
        };
        let summarizer = CapturingSummarizer {
            captured_action_items: std::sync::Mutex::new(None),
            captured_attendees: std::sync::Mutex::new(None),
            canned_output: std::sync::Mutex::new(Some(canned)),
        };

        let orch = Orchestrator::new(cfg());
        let transcript = PathBuf::from("/tmp/heron-test-transcript.jsonl");
        orch.re_summarize_note(&summarizer, &note_path, MeetingType::Other, &transcript)
            .await
            .expect("re_summarize_note");

        // First-summarize-shaped vault: pass `None` so the §10.5
        // prompt block stays out of the prompt entirely.
        assert_eq!(
            *summarizer.captured_action_items.lock().expect("lock"),
            None
        );
        assert_eq!(*summarizer.captured_attendees.lock().expect("lock"), None);
    }
}
