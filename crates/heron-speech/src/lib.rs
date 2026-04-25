//! `heron-speech` — speech-to-text.
//!
//! v0 surface from [`docs/implementation.md`](../../../docs/implementation.md)
//! §8.1. Two backends ship in v1: WhisperKit (the §4 spike, primary)
//! and Sherpa (`sherpa-onnx` parakeet, fallback). Both implement
//! [`SttBackend`]; `heron-session` picks one at session start per
//! §8.6.
//!
//! Real WhisperKit + Sherpa wires arrive weeks 4–5 (§8.2 + §8.3); the
//! trait shape committed here lets `heron-zoom` aligner integration
//! (week 7, §9.3) compile against a stable surface today.

use std::path::Path;

use async_trait::async_trait;
use heron_types::{Channel, SessionId, Turn};
use thiserror::Error;

pub mod partial_writer;
pub mod selection;
pub mod whisperkit_bridge;

pub use partial_writer::{
    FLUSH_INTERVAL, FLUSH_TURNS, PartialWriter, PartialWriterError, read_partial_jsonl,
};
pub use selection::{
    Platform, RealPlatform, WER_THRESHOLDS, WerBaseline, WerThreshold, lookup_threshold,
    select_backend,
};
pub use whisperkit_bridge::{WkError, WkStatus, whisperkit_init, whisperkit_transcribe};

/// Per-backend telemetry collected during a successful transcription.
#[derive(Debug, Clone)]
pub struct TranscribeSummary {
    pub turns: usize,
    pub low_confidence_turns: usize,
    pub model: String,
    /// Wall-clock seconds the backend spent.
    pub elapsed_secs: f64,
}

#[derive(Debug, Error)]
pub enum SttError {
    #[error("not yet implemented (arrives weeks 4–5 per §8)")]
    NotYetImplemented,
    #[error("model not found / not downloaded: {0}")]
    ModelMissing(String),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("transcribe failed: {0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Boxed progress callback used by [`SttBackend::ensure_model`].
///
/// Spec text in §8.1 uses `impl FnMut(f32) + Send`, which would make
/// the trait non-object-safe (`Box<dyn SttBackend>` is required by
/// §8.6 `select_backend`). We deviate to `Box<dyn FnMut(...)>` here
/// to keep the trait dyn-dispatchable; callers wrap closures with
/// `Box::new(...)` at the call site, which is a one-line tax.
pub type ProgressFn = Box<dyn FnMut(f32) + Send>;

/// Boxed turn callback used by [`SttBackend::transcribe`]. Same
/// rationale as [`ProgressFn`].
pub type TurnFn = Box<dyn FnMut(Turn) + Send>;

/// Streaming STT backend.
#[async_trait]
pub trait SttBackend: Send + Sync {
    /// Download / verify / warm the model. The progress callback
    /// reports a value in `[0.0, 1.0]`; it is invoked at least once
    /// before the future resolves so first-run UIs (week 12, §14.1)
    /// can show a spinner.
    async fn ensure_model(&self, on_progress: ProgressFn) -> Result<(), SttError>;

    /// Transcribe `wav_path` and emit incremental [`Turn`]s into
    /// `partial_jsonl_path` per `plan.md` §3.5. The `on_turn`
    /// callback fires once per finalized turn.
    async fn transcribe(
        &self,
        wav_path: &Path,
        channel: Channel,
        session_id: SessionId,
        partial_jsonl_path: &Path,
        on_turn: TurnFn,
    ) -> Result<TranscribeSummary, SttError>;

    fn name(&self) -> &'static str;

    /// Cheap predicate the orchestrator queries before selecting this
    /// backend. WhisperKit returns `false` on Intel macs / pre-14;
    /// Sherpa is always `true` since it bundles an ONNX runtime.
    fn is_available(&self) -> bool;
}

/// Build a [`SttBackend`] by name. Selection per §8.6 is left to the
/// caller (`heron-session`); this factory exists so the CLI can take
/// a `--stt-backend whisperkit|sherpa` flag without each consumer
/// re-deriving the platform predicate.
pub fn build_backend(name: &str) -> Result<Box<dyn SttBackend>, SttError> {
    match name {
        "whisperkit" => Ok(Box::new(stub::WhisperKitStub)),
        "sherpa" => Ok(Box::new(stub::SherpaStub)),
        other => {
            tracing::warn!(name = other, "unknown stt backend requested");
            Err(SttError::Unavailable(format!("unknown backend: {other}")))
        }
    }
}

pub(crate) mod stub {
    use super::{Channel, ProgressFn, SessionId, SttBackend, SttError, TranscribeSummary, TurnFn};
    use async_trait::async_trait;
    use std::path::Path;

    pub struct WhisperKitStub;
    pub struct SherpaStub;

    #[async_trait]
    impl SttBackend for WhisperKitStub {
        async fn ensure_model(&self, _on_progress: ProgressFn) -> Result<(), SttError> {
            Err(SttError::NotYetImplemented)
        }
        async fn transcribe(
            &self,
            _wav_path: &Path,
            _channel: Channel,
            _session_id: SessionId,
            _partial_jsonl_path: &Path,
            _on_turn: TurnFn,
        ) -> Result<TranscribeSummary, SttError> {
            Err(SttError::NotYetImplemented)
        }
        fn name(&self) -> &'static str {
            "whisperkit"
        }
        fn is_available(&self) -> bool {
            // Real impl checks Apple-Silicon + macOS 14+. The stub
            // claims unavailable so a caller that picks-by-availability
            // routes to sherpa during the v0 phase.
            false
        }
    }

    #[async_trait]
    impl SttBackend for SherpaStub {
        async fn ensure_model(&self, _on_progress: ProgressFn) -> Result<(), SttError> {
            Err(SttError::NotYetImplemented)
        }
        async fn transcribe(
            &self,
            _wav_path: &Path,
            _channel: Channel,
            _session_id: SessionId,
            _partial_jsonl_path: &Path,
            _on_turn: TurnFn,
        ) -> Result<TranscribeSummary, SttError> {
            Err(SttError::NotYetImplemented)
        }
        fn name(&self) -> &'static str {
            "sherpa"
        }
        fn is_available(&self) -> bool {
            // Sherpa bundles its ONNX runtime; always available.
            true
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn whisperkit_stub_returns_not_yet_implemented() {
        let b = build_backend("whisperkit").expect("build");
        assert_eq!(b.name(), "whisperkit");
        let result = b.ensure_model(Box::new(|_p| {})).await;
        assert!(matches!(result, Err(SttError::NotYetImplemented)));
    }

    #[tokio::test]
    async fn sherpa_stub_returns_not_yet_implemented() {
        let b = build_backend("sherpa").expect("build");
        assert_eq!(b.name(), "sherpa");
        let result = b
            .transcribe(
                &PathBuf::from("/tmp/x.wav"),
                Channel::Mic,
                SessionId::nil(),
                &PathBuf::from("/tmp/x.jsonl"),
                Box::new(|_t| {}),
            )
            .await;
        assert!(matches!(result, Err(SttError::NotYetImplemented)));
    }

    #[test]
    fn unknown_backend_name_errors() {
        let result = build_backend("magic-asr");
        assert!(matches!(result, Err(SttError::Unavailable(_))));
    }

    #[test]
    fn availability_predicates_match_design() {
        // WhisperKit stub claims unavailable so the v0 orchestrator
        // routes to sherpa; sherpa is always available because it
        // bundles its ONNX runtime.
        let wk = build_backend("whisperkit").expect("wk");
        let sh = build_backend("sherpa").expect("sh");
        assert!(!wk.is_available());
        assert!(sh.is_available());
    }

    #[test]
    fn callbacks_can_capture_state() {
        // ProgressFn = Box<dyn FnMut(f32) + Send> defaults to a
        // 'static lifetime, so closures must own their captures.
        // The typical use is "increment a Counter behind an Arc"
        // for the diagnostics tab — verified here.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let count = Arc::new(AtomicU32::new(0));
        let count_in_closure = Arc::clone(&count);
        let mut progress: ProgressFn = Box::new(move |_p| {
            count_in_closure.fetch_add(1, Ordering::Relaxed);
        });
        progress(0.5);
        progress(1.0);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }
}
