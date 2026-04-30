//! `heron-speech` — speech-to-text.
//!
//! v0 surface from [`docs/archives/implementation.md`](../../../docs/archives/implementation.md)
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
pub mod sherpa;
pub mod whisperkit_bridge;

pub use partial_writer::{
    FLUSH_INTERVAL, FLUSH_TURNS, PartialWriter, PartialWriterError, read_partial_jsonl,
};
pub use selection::{
    Platform, RealPlatform, WER_THRESHOLDS, WerBaseline, WerThreshold, lookup_threshold,
    select_backend,
};
pub use sherpa::SherpaBackend;
pub use whisperkit_bridge::{
    DEFAULT_WK_VARIANT, WkError, WkStatus, compose_prompt, whisperkit_fetch, whisperkit_init,
    whisperkit_transcribe,
};

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
/// `"sherpa"` returns the production [`SherpaBackend`] per §8.3. Same
/// pattern as WhisperKit: model directory resolves at `ensure_model`
/// time (`HERON_SHERPA_MODEL_DIR` override; default
/// `~/Library/Caches/heron/sherpa/`), so a missing-model failure
/// surfaces inside the orchestrator's progress UI rather than at
/// flag-parse.
///
/// `hotwords` is the Tier 4 #17 vocabulary-boost list. The WhisperKit
/// backend forwards it as a tokenized prompt on every `transcribe`
/// call; the Sherpa backend ignores it for now (Sherpa has its own
/// hotword API that ships in a sibling Tier-4 PR). An empty slice is
/// the migration-safe default — `build_backend("whisperkit", &[])`
/// reproduces the pre-Tier-4 decode path byte-for-byte.
pub fn build_backend(name: &str, hotwords: &[String]) -> Result<Box<dyn SttBackend>, SttError> {
    match name {
        #[cfg(target_vendor = "apple")]
        "whisperkit" => Ok(Box::new(
            WhisperKitBackend::from_env().with_hotwords(hotwords.to_vec()),
        )),
        #[cfg(not(target_vendor = "apple"))]
        "whisperkit" => {
            // Off-Apple stub doesn't decode anything; hotwords have
            // nowhere to land. Discard rather than carrying through a
            // never-read field on the stub.
            let _ = hotwords;
            Ok(Box::new(stub::WhisperKitStub))
        }
        "sherpa" => {
            // Sherpa has its own hotword config (sibling Tier-4 PR);
            // for now we accept-and-ignore so the call-site signature
            // is uniform across backends.
            let _ = hotwords;
            Ok(Box::new(sherpa::SherpaBackend::from_env()))
        }
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
/// Construction is cheap (no model load and no network); the heavy
/// lifting — fetch + load — happens in [`SttBackend::ensure_model`].
/// The `init_cell` guards against accidental double-load: only the
/// first concurrent `ensure_model` call runs the fetch and `wk_init`;
/// others await its result. The cell stores the resolved model path
/// so subsequent runs (and the diagnostics tab) can read it back.
#[cfg(target_vendor = "apple")]
pub struct WhisperKitBackend {
    /// Configured model location. When `Override(dir)`, ensure_model
    /// uses it verbatim and skips the fetch step (escape hatch for CI
    /// and integration tests). When `Cache(root)`, ensure_model
    /// resolves the variant under `<root>/<variant>` and triggers a
    /// download on first run.
    location: ModelLocation,
    /// WhisperKit variant identifier, e.g. `"openai_whisper-small.en"`.
    variant: String,
    /// Tier 4 #17 vocabulary-boost terms, joined with single spaces and
    /// passed to WhisperKit as `DecodingOptions.promptTokens`. Empty
    /// → no prompt set (migration-safe default; preserves pre-Tier-4
    /// decode behaviour byte-for-byte).
    hotwords: Vec<String>,
    /// Resolved model folder after a successful `ensure_model` —
    /// the directory we actually pass to `wk_init`. Initialized
    /// exactly once across concurrent `ensure_model` callers.
    init_cell: OnceCell<PathBuf>,
}

/// Where the WhisperKit model lives.
#[cfg(target_vendor = "apple")]
#[derive(Debug, Clone)]
enum ModelLocation {
    /// Cache root under which the variant is fetched on first run.
    /// The actual model folder is resolved by `wk_fetch_model`.
    Cache(PathBuf),
    /// Operator-supplied path (via `HERON_WHISPERKIT_MODEL_DIR`). Used
    /// verbatim and never downloaded into.
    Override(PathBuf),
}

#[cfg(target_vendor = "apple")]
impl WhisperKitBackend {
    /// Construct a backend with an explicit model directory. The
    /// fetch step is skipped — the directory must already contain the
    /// `.mlmodelc` bundles. Used by tests and by `from_env` when the
    /// `HERON_WHISPERKIT_MODEL_DIR` escape hatch is set.
    pub fn new(model_dir: PathBuf) -> Self {
        Self {
            location: ModelLocation::Override(model_dir),
            variant: DEFAULT_WK_VARIANT.to_owned(),
            hotwords: Vec::new(),
            init_cell: OnceCell::new(),
        }
    }

    /// Construct a backend that downloads the default variant under
    /// the supplied cache root on first `ensure_model`.
    pub fn with_cache_root(cache_root: PathBuf) -> Self {
        Self {
            location: ModelLocation::Cache(cache_root),
            variant: DEFAULT_WK_VARIANT.to_owned(),
            hotwords: Vec::new(),
            init_cell: OnceCell::new(),
        }
    }

    /// Override the [`hotwords`](Self::hotwords) field. Builder-style
    /// so the construction call site stays a single chain:
    /// `WhisperKitBackend::from_env().with_hotwords(settings.hotwords)`.
    /// An empty `Vec` is equivalent to "no prompt" — see
    /// [`compose_prompt`] for the join semantics. Stored
    /// verbatim; the per-`transcribe`-call composition runs at
    /// transcribe time so a future `update_hotwords` setter (e.g.
    /// driven by a Settings live-reload) can swap in a new vec
    /// without rebuilding the backend.
    #[must_use]
    pub fn with_hotwords(mut self, hotwords: Vec<String>) -> Self {
        self.hotwords = hotwords;
        self
    }

    /// Construct from environment.
    ///
    /// - If `HERON_WHISPERKIT_MODEL_DIR` is set, use it verbatim
    ///   (no download). Tests and sandboxed CI take this path.
    /// - Otherwise, pick the OS cache dir (`dirs::cache_dir()`) and
    ///   use `<cache>/heron/whisperkit` as the cache root. On macOS
    ///   that's `~/Library/Caches/heron/whisperkit/`.
    /// - Last-resort fallback (cache_dir unavailable, e.g. a stripped
    ///   sandbox without HOME): `/tmp/heron-whisperkit-cache`. Lets
    ///   construction succeed; ensure_model surfaces any path issue.
    pub fn from_env() -> Self {
        if let Some(dir) = std::env::var_os("HERON_WHISPERKIT_MODEL_DIR") {
            return Self::new(PathBuf::from(dir));
        }
        let cache_root = dirs::cache_dir()
            .map(|d| d.join("heron").join("whisperkit"))
            .unwrap_or_else(|| PathBuf::from("/tmp/heron-whisperkit-cache"));
        Self::with_cache_root(cache_root)
    }

    /// Resolved model directory after a successful ensure_model. Only
    /// `Some` once the cell is initialized. Currently used by tests
    /// and the integration check; orchestrator code may want it once
    /// the diagnostics tab grows a "where is my model" line.
    #[cfg(test)]
    fn resolved_model_dir(&self) -> Option<&Path> {
        self.init_cell.get().map(PathBuf::as_path)
    }
}

/// Heuristic check that a directory contains a usable WhisperKit
/// bundle. WhisperKit's `download` writes one or more `.mlmodelc`
/// directories under the variant folder; if at least one exists we
/// assume the cache is warm and skip the network round-trip. A
/// stricter check would inspect tokenizer/json files, but the cost
/// of a false-positive here is `wk_init` returning Internal, which
/// the operator can resolve by deleting the cache.
#[cfg(target_vendor = "apple")]
fn dir_has_mlmodelc(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in rd.flatten() {
        if entry.path().extension().is_some_and(|e| e == "mlmodelc")
            && entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
        {
            return true;
        }
    }
    false
}

#[cfg(target_vendor = "apple")]
#[async_trait]
impl SttBackend for WhisperKitBackend {
    async fn ensure_model(&self, mut on_progress: ProgressFn) -> Result<(), SttError> {
        // Per §14.1, fire progress at start so the first-run UI never
        // sits at zero while the cache check / Swift Task spin up.
        on_progress(0.0);

        // Wrap the (boxed) progress callback in an Arc<Mutex<...>> so
        // both the fetch closure (which fires repeatedly) and the
        // post-init "1.0" sentinel can call it. Mutex (over an
        // exclusive borrow) is needed because spawn_blocking moves the
        // closure into another thread.
        let progress = std::sync::Arc::new(std::sync::Mutex::new(on_progress));

        let location = self.location.clone();
        let variant = self.variant.clone();
        let progress_for_init = std::sync::Arc::clone(&progress);

        // OnceCell::get_or_try_init runs the init body exactly once
        // across concurrent callers; failures don't poison the cell,
        // so a retry can re-run the fetch + init.
        self.init_cell
            .get_or_try_init(|| async move {
                let progress_inner = std::sync::Arc::clone(&progress_for_init);
                tokio::task::spawn_blocking(move || -> Result<PathBuf, SttError> {
                    let model_dir = match &location {
                        ModelLocation::Override(dir) => dir.clone(),
                        ModelLocation::Cache(root) => {
                            let variant_dir = root.join(&variant);
                            if dir_has_mlmodelc(&variant_dir) {
                                // Cache hit — skip the network round-trip.
                                variant_dir
                            } else {
                                std::fs::create_dir_all(root).map_err(|e| {
                                    SttError::Failed(format!(
                                        "create whisperkit cache root {}: {e}",
                                        root.display()
                                    ))
                                })?;
                                let cb_arc = std::sync::Arc::clone(&progress_inner);
                                // WhisperKit's `download` reports
                                // 0.0…1.0 incrementally; we forward
                                // each tick to the boxed callback
                                // under the mutex.
                                let on_p = move |p: f32| {
                                    if let Ok(mut g) = cb_arc.lock() {
                                        g(p);
                                    }
                                };
                                whisperkit_fetch(&variant, root, on_p).map_err(|e| match e {
                                    WkError::ModelMissing => SttError::ModelMissing(format!(
                                        "unknown whisperkit variant: {variant}"
                                    )),
                                    WkError::NotYetImplemented => SttError::NotYetImplemented,
                                    other => SttError::Failed(format!("whisperkit fetch: {other}")),
                                })?
                            }
                        }
                    };

                    whisperkit_init(&model_dir).map_err(|e| match e {
                        WkError::ModelMissing => {
                            SttError::ModelMissing(model_dir.display().to_string())
                        }
                        WkError::NotYetImplemented => SttError::NotYetImplemented,
                        other => SttError::Failed(format!("whisperkit init: {other}")),
                    })?;
                    Ok(model_dir)
                })
                .await
                .map_err(|e| SttError::Failed(format!("whisperkit init join failed: {e}")))?
            })
            .await?;

        if let Ok(mut g) = progress.lock() {
            g(1.0);
        }
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
        // Compose the prompt up-front (cheap join over a small Vec)
        // and move it into the blocking task. Doing it here rather than
        // inside spawn_blocking lets a misconfigured prompt — say, a
        // user who sneaked an internal NUL into a hotword — surface
        // synchronously where the call site has the original
        // `&self.hotwords` slice for diagnostics, rather than in a
        // boxed-error string from a join handle.
        let prompt = compose_prompt(&self.hotwords);
        let body = tokio::task::spawn_blocking(move || {
            whisperkit_transcribe(&wav_owned, prompt.as_deref())
        })
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
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn whisperkit_backend_with_missing_model_dir_errors() {
        // With `HERON_WHISPERKIT_MODEL_DIR` set to a non-existent
        // path, ensure_model takes the override branch (skip fetch,
        // pass the path verbatim to `wk_init`) and surfaces
        // `ModelMissing`. The override path is the contract tests
        // and sandboxed CI rely on so neither requires network. Off
        // Apple the stub returns `NotYetImplemented` regardless.
        // SAFETY: tests run with a single tokio runtime per test fn;
        // no other test reads this env var concurrently in the same
        // process.
        unsafe {
            std::env::set_var(
                "HERON_WHISPERKIT_MODEL_DIR",
                "/nonexistent/heron-whisperkit-model-dir",
            );
        }
        let b = build_backend("whisperkit", &[]).expect("build");
        assert_eq!(b.name(), "whisperkit");
        let result = b.ensure_model(Box::new(|_p| {})).await;
        unsafe {
            std::env::remove_var("HERON_WHISPERKIT_MODEL_DIR");
        }
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

    #[cfg(target_vendor = "apple")]
    #[test]
    fn whisperkit_backend_with_cache_root_carries_default_variant() {
        // Construction with an explicit cache root is the path
        // ensure_model takes when no `HERON_WHISPERKIT_MODEL_DIR`
        // override is set. Asserts the default variant flows through;
        // doesn't touch the env so it can't race the override test.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let b = WhisperKitBackend::with_cache_root(tmp.path().to_path_buf());
        assert_eq!(b.variant, DEFAULT_WK_VARIANT);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn whisperkit_backend_default_hotwords_is_empty() {
        // Migration contract: a backend constructed without
        // `with_hotwords` decodes byte-identically to the pre-Tier-4
        // path. The empty default in `new` / `with_cache_root` is what
        // upholds that contract.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let b = WhisperKitBackend::new(tmp.path().to_path_buf());
        assert!(b.hotwords.is_empty());
        let b2 = WhisperKitBackend::with_cache_root(tmp.path().to_path_buf());
        assert!(b2.hotwords.is_empty());
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn whisperkit_backend_with_hotwords_sets_field() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let b = WhisperKitBackend::new(tmp.path().to_path_buf())
            .with_hotwords(vec!["heron".into(), "WhisperKit".into()]);
        assert_eq!(
            b.hotwords,
            vec!["heron".to_owned(), "WhisperKit".to_owned()]
        );
    }

    #[test]
    fn build_backend_accepts_hotwords_for_every_known_name() {
        // The Tauri/desktop hand-off calls
        // `build_backend(stt_backend_name, settings.hotwords)`. This
        // test pins the contract that the hotwords parameter is
        // accepted by every recognized backend name without erroring —
        // a regression that hard-errored on Sherpa (which currently
        // has no hotwords API of its own) would silently disable
        // recording for any user with `stt_backend = "sherpa"` who
        // also configured hotwords.
        //
        // Coverage of the "hotwords actually reach the WhisperKit
        // backend" property lives on
        // `whisperkit_backend_with_hotwords_sets_field` (Apple-only)
        // and the `compose_prompt` unit tests in
        // `whisperkit_bridge::tests`; this test only asserts the
        // factory's own contract.
        for name in ["whisperkit", "sherpa"] {
            let result = build_backend(name, &["heron".to_owned(), "Anthropic".to_owned()]);
            assert!(
                result.is_ok(),
                "build_backend({name}) with hotwords must succeed; got error: {:?}",
                result.err(),
            );
        }
    }

    /// End-to-end fetch + init against a real WhisperKit download.
    /// Network-dependent, so gated by `HERON_WHISPERKIT_INTEGRATION=1`
    /// to keep CI without network egress green.
    #[cfg(target_vendor = "apple")]
    #[tokio::test]
    async fn ensure_model_downloads_real_whisperkit_when_opted_in() {
        if std::env::var("HERON_WHISPERKIT_INTEGRATION").as_deref() != Ok("1") {
            return;
        }
        // Construct via `with_cache_root` directly so we never touch
        // `HERON_WHISPERKIT_MODEL_DIR` — the env var coordinates with
        // the override-path test in the same process and a clear
        // here would race that test's set.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let b = WhisperKitBackend::with_cache_root(tmp.path().to_path_buf());
        let progress_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let pc = std::sync::Arc::clone(&progress_count);
        let result = b
            .ensure_model(Box::new(move |_p| {
                pc.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }))
            .await;
        assert!(result.is_ok(), "ensure_model failed: {result:?}");
        let resolved = b.resolved_model_dir().expect("init_cell populated");
        let mut found = false;
        for entry in std::fs::read_dir(resolved)
            .expect("read resolved dir")
            .flatten()
        {
            if entry.path().extension().is_some_and(|e| e == "mlmodelc") {
                found = true;
                break;
            }
        }
        assert!(found, "no .mlmodelc in resolved dir {resolved:?}");
        // At minimum the start (0.0) and end (1.0) ticks fire.
        assert!(progress_count.load(std::sync::atomic::Ordering::Relaxed) >= 2);
    }

    #[tokio::test]
    async fn sherpa_backend_is_real_and_available() {
        // Pre-§8.3 this slot held a `NotYetImplemented` stub. The real
        // backend reports `name() == "sherpa"`, claims availability
        // unconditionally (sherpa-onnx bundles its ONNX runtime), and
        // declines to transcribe before `ensure_model` has populated
        // the cache directory — that last shape gives the orchestrator
        // a `ModelMissing` to drive its first-run download UI off of.
        let b = build_backend("sherpa", &[]).expect("build");
        assert_eq!(b.name(), "sherpa");
        assert!(b.is_available());

        let tmp = tempfile::TempDir::new().expect("tmp");
        let model_dir = tmp.path().join("empty-model-dir");
        std::fs::create_dir_all(&model_dir).expect("mkdir");
        // Build a backend pointed at an empty cache dir so the missing-
        // model path is exercised deterministically.
        let backend = SherpaBackend::new(model_dir);
        let wav = tmp.path().join("input.wav");
        write_silent_wav(&wav, 16_000, 800);
        let result = backend
            .transcribe(
                &wav,
                Channel::Mic,
                SessionId::nil(),
                &tmp.path().join("p.jsonl"),
                Box::new(|_t| {}),
            )
            .await;
        assert!(
            matches!(result, Err(SttError::ModelMissing(_))),
            "expected ModelMissing, got {result:?}",
        );
    }

    fn write_silent_wav(path: &std::path::Path, rate: u32, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).expect("wav create");
        for _ in 0..samples {
            w.write_sample(0i16).expect("wav write");
        }
        w.finalize().expect("wav finalize");
    }

    #[test]
    fn unknown_backend_name_errors() {
        let result = build_backend("magic-asr", &[]);
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
        let wk = build_backend("whisperkit", &[]).expect("wk");
        let sh = build_backend("sherpa", &[]).expect("sh");
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
