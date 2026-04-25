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
#[cfg(target_vendor = "apple")]
use std::path::PathBuf;

use async_trait::async_trait;
#[cfg(target_vendor = "apple")]
use heron_types::SpeakerSource;
use heron_types::{Channel, SessionId, Turn};
use thiserror::Error;
#[cfg(target_vendor = "apple")]
use tokio::sync::OnceCell;

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
///
/// On Apple targets, `"whisperkit"` returns the real [`WhisperKitBackend`]
/// — it routes through the §4 Swift bridge. The model-directory env
/// var (`HERON_WHISPERKIT_MODEL_DIR`) is resolved at `ensure_model`
/// time, not at build time, so missing-model failures surface where
/// the orchestrator can show progress UI rather than at flag-parse.
/// Off Apple, `"whisperkit"` continues to return the test stub since
/// the Swift bridge isn't compiled there.
///
/// `"sherpa"` is still wired to its v0 stub. Real Sherpa lands in §8.3.
pub fn build_backend(name: &str) -> Result<Box<dyn SttBackend>, SttError> {
    match name {
        #[cfg(target_vendor = "apple")]
        "whisperkit" => Ok(Box::new(WhisperKitBackend::from_env())),
        #[cfg(not(target_vendor = "apple"))]
        "whisperkit" => Ok(Box::new(stub::WhisperKitStub)),
        "sherpa" => Ok(Box::new(stub::SherpaStub)),
        other => {
            tracing::warn!(name = other, "unknown stt backend requested");
            Err(SttError::Unavailable(format!("unknown backend: {other}")))
        }
    }
}

/// Production WhisperKit backend.
///
/// Wraps the §4 Swift bridge (`whisperkit_init` + `whisperkit_transcribe`)
/// behind the [`SttBackend`] trait. Each blocking FFI call is dispatched
/// onto the blocking-pool via `tokio::task::spawn_blocking` so an STT
/// pass doesn't stall the async runtime — the Swift bridge itself
/// blocks on a `DispatchSemaphore` waiting for WhisperKit's async
/// transcribe.
///
/// Construction is cheap (no model load); the heavy lifting happens
/// in [`SttBackend::ensure_model`]. The `init_cell` guards against
/// accidental double-load — Swift maintains a single global instance,
/// but a paranoid Rust caller can re-call `ensure_model` safely.
/// `OnceCell` (over `AtomicBool`) makes that idempotency race-free
/// across concurrent `ensure_model` calls: only the first task runs
/// the underlying `wk_init`, others await its result.
#[cfg(target_vendor = "apple")]
pub struct WhisperKitBackend {
    /// Folder containing the WhisperKit-compiled `.mlmodelc` bundles.
    /// Resolved from `HERON_WHISPERKIT_MODEL_DIR` by [`Self::from_env`]
    /// or supplied directly via [`Self::new`] in tests.
    model_dir: PathBuf,
    /// Initialized exactly once across all concurrent `ensure_model`
    /// callers. Failed inits leave the cell empty so a retry can run
    /// the init again rather than caching the failure.
    init_cell: OnceCell<()>,
}

#[cfg(target_vendor = "apple")]
impl WhisperKitBackend {
    /// Construct a backend with an explicit model directory.
    pub fn new(model_dir: PathBuf) -> Self {
        Self {
            model_dir,
            init_cell: OnceCell::new(),
        }
    }

    /// Construct from `HERON_WHISPERKIT_MODEL_DIR` if set, else point
    /// at a sentinel that will fail `ensure_model` with `ModelMissing`.
    /// We don't fail construction here so `build_backend("whisperkit")`
    /// keeps returning a value the CLI can store before any models are
    /// downloaded.
    pub fn from_env() -> Self {
        let dir = std::env::var_os("HERON_WHISPERKIT_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent/heron-whisperkit-model-dir"));
        Self::new(dir)
    }
}

#[cfg(target_vendor = "apple")]
#[async_trait]
impl SttBackend for WhisperKitBackend {
    async fn ensure_model(&self, mut on_progress: ProgressFn) -> Result<(), SttError> {
        // Per §14.1, fire progress at start *and* end so a first-run
        // UI never sits at zero. WhisperKit doesn't expose granular
        // load progress today; if it does in a future release we'll
        // wire a callback through the Swift bridge.
        on_progress(0.0);

        // OnceCell::get_or_try_init runs the init body exactly once
        // across concurrent callers; failures don't poison the cell,
        // so a retry can re-run the init.
        self.init_cell
            .get_or_try_init(|| async {
                let model_dir = self.model_dir.clone();
                tokio::task::spawn_blocking(move || whisperkit_init(&model_dir))
                    .await
                    .map_err(|e| SttError::Failed(format!("whisperkit init join failed: {e}")))?
                    .map_err(|e| match e {
                        WkError::ModelMissing => {
                            SttError::ModelMissing(self.model_dir.display().to_string())
                        }
                        WkError::NotYetImplemented => SttError::NotYetImplemented,
                        other => SttError::Failed(format!("whisperkit init: {other}")),
                    })
            })
            .await?;

        on_progress(1.0);
        Ok(())
    }

    async fn transcribe(
        &self,
        wav_path: &Path,
        channel: Channel,
        _session_id: SessionId,
        partial_jsonl_path: &Path,
        mut on_turn: TurnFn,
    ) -> Result<TranscribeSummary, SttError> {
        let started = std::time::Instant::now();

        let wav_owned = wav_path.to_path_buf();
        let body = tokio::task::spawn_blocking(move || whisperkit_transcribe(&wav_owned))
            .await
            .map_err(|e| SttError::Failed(format!("whisperkit transcribe join failed: {e}")))?
            .map_err(|e| match e {
                WkError::ModelMissing => SttError::ModelMissing(String::new()),
                WkError::NotYetImplemented => SttError::NotYetImplemented,
                other => SttError::Failed(format!("whisperkit transcribe: {other}")),
            })?;

        // Open the partial-writer eagerly so a transcription with zero
        // segments still leaves an (empty) on-disk artifact for the
        // recovery flow per §3.5.
        let mut writer = PartialWriter::create(partial_jsonl_path.to_path_buf())
            .map_err(|e| SttError::Failed(format!("partial writer: {e}")))?;

        // `MicClean` is the post-AEC mic stream — same speaker (the
        // user, "me") as raw `Mic`, just with speaker bleed removed.
        // STT consumes `MicClean` in the wired pipeline, but the
        // legacy raw-`Mic` path is still valid for offline rebuild
        // tests (re-running APM against archived mic.raw / tap.raw).
        let speaker = match channel {
            Channel::Mic | Channel::MicClean => "me".to_owned(),
            Channel::Tap => "them".to_owned(),
        };
        let speaker_source = match channel {
            Channel::Mic | Channel::MicClean => SpeakerSource::Self_,
            Channel::Tap => SpeakerSource::Channel,
        };

        let mut turns = 0usize;
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let segment: WhisperKitSegment = serde_json::from_str(line)
                .map_err(|e| SttError::Failed(format!("invalid segment json from bridge: {e}")))?;
            let turn = Turn {
                t0: segment.start,
                t1: segment.end,
                text: segment.text,
                channel,
                speaker: speaker.clone(),
                speaker_source,
                // WhisperKit's segment-level confidence isn't on the
                // wire yet; revisit when the Swift side adds it.
                confidence: None,
            };
            writer
                .push(&turn)
                .map_err(|e| SttError::Failed(format!("partial writer push: {e}")))?;
            on_turn(turn);
            turns += 1;
        }
        writer
            .finalize()
            .map_err(|e| SttError::Failed(format!("partial writer finalize: {e}")))?;

        Ok(TranscribeSummary {
            turns,
            // WhisperKit doesn't surface confidence yet; treat all turns
            // as high-confidence for now. The §8.4 telemetry will
            // re-derive this once `avgLogprob` makes it across the wire.
            low_confidence_turns: 0,
            model: "whisperkit".to_owned(),
            elapsed_secs: started.elapsed().as_secs_f64(),
        })
    }

    fn name(&self) -> &'static str {
        "whisperkit"
    }

    fn is_available(&self) -> bool {
        // Mirrors §8.6: WhisperKit needs Apple-Silicon + macOS 14+.
        // We probe via the same predicate `select_backend` uses so
        // the `build_backend` and `select_backend` paths agree.
        let p = RealPlatform;
        p.is_apple_silicon() && p.is_macos_14_plus()
    }
}

/// On-the-wire shape emitted by the Swift bridge, one JSON object per
/// line. Extra fields on the bridge side are tolerated (serde ignores
/// them by default); this struct only names the three we currently
/// project into the §5.2 [`Turn`].
#[cfg(target_vendor = "apple")]
#[derive(serde::Deserialize)]
struct WhisperKitSegment {
    start: f64,
    end: f64,
    text: String,
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
    async fn whisperkit_backend_with_missing_model_dir_errors() {
        // On Apple targets `build_backend("whisperkit")` returns the
        // real `WhisperKitBackend`, which resolves its model folder
        // from `HERON_WHISPERKIT_MODEL_DIR`. Without that env var the
        // backend points at a sentinel non-existent path; ensure_model
        // must surface `ModelMissing` rather than silently succeeding.
        // Off-Apple this test still hits the stub and gets
        // `NotYetImplemented`.
        // SAFETY: tests run with a single tokio runtime per test fn;
        // no other test reads this env var concurrently in the same
        // process.
        unsafe {
            std::env::remove_var("HERON_WHISPERKIT_MODEL_DIR");
        }
        let b = build_backend("whisperkit").expect("build");
        assert_eq!(b.name(), "whisperkit");
        let result = b.ensure_model(Box::new(|_p| {})).await;
        #[cfg(target_vendor = "apple")]
        assert!(
            matches!(result, Err(SttError::ModelMissing(_))),
            "expected ModelMissing on Apple, got {result:?}"
        );
        #[cfg(not(target_vendor = "apple"))]
        assert!(
            matches!(result, Err(SttError::NotYetImplemented)),
            "expected NotYetImplemented off-Apple, got {result:?}"
        );
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
        // Sherpa is always available because it bundles its ONNX
        // runtime. WhisperKit's availability is platform-conditional:
        //   - on Apple Silicon + macOS 14+, the real backend reports
        //     `true` (it can actually run);
        //   - on Intel macs, pre-Sonoma macs, or non-Apple platforms,
        //     the predicate is `false` so `select_backend` routes to
        //     Sherpa per §8.6.
        // The test runs the real predicate against the host so a
        // CI machine that loses Apple Silicon would surface the drift.
        let wk = build_backend("whisperkit").expect("wk");
        let sh = build_backend("sherpa").expect("sh");
        let expected_wk = cfg!(target_vendor = "apple") && {
            let p = RealPlatform;
            p.is_apple_silicon() && p.is_macos_14_plus()
        };
        assert_eq!(wk.is_available(), expected_wk);
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
