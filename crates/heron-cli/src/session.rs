//! Session orchestrator skeleton.
//!
//! Wires the v0 stub backends — [`heron_audio::AudioCapture`],
//! [`heron_speech::SttBackend`], [`heron_zoom::AxBackend`],
//! [`heron_llm::Summarizer`] — onto a
//! single [`RecordingFsm`] driven session lifecycle.
//!
//! Until the real backends land in their respective weeks, the
//! orchestrator's `run_no_op()` exercises every FSM transition the
//! happy path goes through, so the test suite can guard the wiring
//! today. When the real impls plug in, the orchestrator's `run()`
//! will be a thin async wrapper that listens on the broadcast
//! channels and drives the FSM off events.

// Several fields and methods are part of the orchestrator's public
// shape but only exercised by tests + cmd_record's smoke wiring
// today. Real consumers (Tauri command handlers, week-11) will read
// every field. Allow dead_code until then so clippy's deny-warnings
// CI gate doesn't push us to delete shape we know we'll need.
#![allow(dead_code)]

use std::path::PathBuf;

use heron_types::{IdleReason, RecordingFsm, RecordingState, SessionId, SummaryOutcome};
use thiserror::Error;

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

/// Outcome of `run_no_op` and (eventually) `run`.
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
}

impl Orchestrator {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            fsm: RecordingFsm::new(),
        }
    }

    /// Drive the FSM through the full happy-path transitions without
    /// actually invoking any backends. Used today for testing the
    /// wiring; real `run()` arrives once the audio pipeline is real
    /// (week 11 per §13).
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

    /// Try to start the audio capture. Returns the `NotYetImplemented`
    /// error from the v0 stub today; real impl wires the broadcast
    /// channels.
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
    /// LLM selection is driven by the new
    /// [`heron_llm::select_summarizer`] selector — the chosen
    /// backend + reason are emitted via `tracing::info!` so an
    /// operator inspecting logs can tell whether the API path was
    /// picked or a CLI fallback fired.
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
}
