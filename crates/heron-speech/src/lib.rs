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

pub mod metrics_names;
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

/// Wrap a [`SttBackend::transcribe`] call with the
/// `stt_duration_seconds` histogram + `stt_failures_total{reason}`
/// counter from `docs/observability.md`. Consumers (today
/// `heron-pipeline::pipeline::run_stt`) should call through this
/// wrapper rather than calling `backend.transcribe` directly so
/// every backend (WhisperKit, Sherpa, the stub) is covered at one
/// site.
///
/// **Backend label.** The `backend` dimension is mapped from
/// `backend.name()` to a pinned `redacted!` literal — free-form
/// `String` labels are forbidden by the foundation's privacy
/// posture, so an unknown name falls through to the `unknown`
/// bucket rather than smuggling a user-provided string into the
/// time series. Today the closed set is `{"whisperkit", "sherpa",
/// "whisperkit_stub", "unknown"}`; adding a new backend means
/// growing the match arm here.
///
/// **Failure-reason label.** Each [`SttError`] variant maps to a
/// pinned `redacted!` literal per the documented closed set. An
/// empty-transcript success (zero turns) is recorded as a separate
/// `stt_failures_total{reason="transcription_empty"}` bump even
/// though `transcribe` returned `Ok` — the consumer's existing
/// soft-fail behaviour is preserved (the wrapper still returns
/// `Ok(summary)`), and the metric surfaces the silent-empty case to
/// dashboards.
pub async fn transcribe_with_metrics(
    backend: &dyn SttBackend,
    wav_path: &Path,
    channel: Channel,
    session_id: SessionId,
    partial_jsonl_path: &Path,
    on_turn: TurnFn,
) -> Result<TranscribeSummary, SttError> {
    let backend_name = backend.name();
    let started = std::time::Instant::now();
    let result = backend
        .transcribe(wav_path, channel, session_id, partial_jsonl_path, on_turn)
        .await;
    let elapsed_secs = started.elapsed().as_secs_f64();

    // The histogram is recorded on BOTH the success and failure
    // paths — a backend that hangs and returns a typed error after
    // 30s should still show up in the latency distribution.
    metrics::histogram!(
        metrics_names::STT_DURATION_SECONDS,
        "backend" => backend_name_to_label(backend_name).into_inner(),
    )
    .record(elapsed_secs);

    match &result {
        Ok(summary) if summary.turns == 0 => {
            // Soft-fail bucket: the backend produced no turns. The
            // consumer treats this as success (and writes an empty
            // transcript), but the dashboard answer "are we silently
            // producing empty transcripts?" is load-bearing.
            // The `backend` label mirrors the histogram's dimension
            // so a failure-rate-per-backend dashboard query can
            // `sum by (backend)` cleanly.
            metrics::counter!(
                metrics_names::STT_FAILURES_TOTAL,
                "backend" => backend_name_to_label(backend_name).into_inner(),
                "reason" => heron_metrics::redacted!("transcription_empty").into_inner(),
            )
            .increment(1);
        }
        Ok(_) => {}
        Err(err) => {
            let reason = stt_error_to_reason_label(err);
            metrics::counter!(
                metrics_names::STT_FAILURES_TOTAL,
                "backend" => backend_name_to_label(backend_name).into_inner(),
                "reason" => reason.into_inner(),
            )
            .increment(1);
        }
    }

    result
}

/// Map a backend's `name()` string to a pinned `redacted!` literal.
/// Free-form labels would violate the privacy posture; an unknown
/// name falls through to the `unknown` bucket rather than smuggling
/// a user-provided string into the time series.
fn backend_name_to_label(name: &str) -> heron_metrics::RedactedLabel {
    match name {
        "whisperkit" => heron_metrics::redacted!("whisperkit"),
        "sherpa" => heron_metrics::redacted!("sherpa"),
        "whisperkit_stub" => heron_metrics::redacted!("whisperkit_stub"),
        _ => heron_metrics::redacted!("unknown"),
    }
}

/// Map an [`SttError`] to a pinned `redacted!` reason label.
/// Free-form error text would smuggle the model path / file path /
/// vendor diagnostic into the metric label; the matching
/// `tracing::warn!` at the call site keeps the human-readable text
/// in the log layer where it belongs.
fn stt_error_to_reason_label(err: &SttError) -> heron_metrics::RedactedLabel {
    match err {
        // `NotYetImplemented` only fires from the off-Apple WhisperKit
        // stub today; semantically it's "no real backend usable on
        // this host", which collapses into the same operator signal
        // as `ModelMissing` / `Unavailable`. Keeping the bucket count
        // small keeps the dashboard readable; the matching
        // `tracing::warn!` carries the exact variant for log triage.
        SttError::NotYetImplemented => heron_metrics::redacted!("model_unavailable"),
        SttError::ModelMissing(_) => heron_metrics::redacted!("model_unavailable"),
        SttError::Unavailable(_) => heron_metrics::redacted!("model_unavailable"),
        SttError::Failed(_) => heron_metrics::redacted!("failed"),
        SttError::Io(_) => heron_metrics::redacted!("io"),
    }
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
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::metrics_names::{STT_DURATION_SECONDS, STT_FAILURES_TOTAL};

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

    /// Test backend that hands the wrapper a configurable result so
    /// `transcribe_with_metrics` can be exercised on the success,
    /// empty-transcript, and failure paths without dragging in
    /// WhisperKit / Sherpa.
    struct StubMetricsBackend {
        name: &'static str,
        outcome: StubOutcome,
    }

    enum StubOutcome {
        Ok(usize),
        Failed,
        ModelMissing,
    }

    #[async_trait]
    impl SttBackend for StubMetricsBackend {
        async fn ensure_model(&self, _on_progress: ProgressFn) -> Result<(), SttError> {
            Ok(())
        }
        async fn transcribe(
            &self,
            _wav_path: &Path,
            _channel: Channel,
            _session_id: SessionId,
            _partial_jsonl_path: &Path,
            _on_turn: TurnFn,
        ) -> Result<TranscribeSummary, SttError> {
            match self.outcome {
                StubOutcome::Ok(turns) => Ok(TranscribeSummary {
                    turns,
                    low_confidence_turns: 0,
                    model: "test-stub".into(),
                    elapsed_secs: 0.0,
                }),
                StubOutcome::Failed => Err(SttError::Failed("synthetic failure".into())),
                StubOutcome::ModelMissing => {
                    Err(SttError::ModelMissing("stub model missing".into()))
                }
            }
        }
        fn name(&self) -> &'static str {
            self.name
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    /// Helper: parse the labelled `<name>{...} <value>` line out of
    /// the Prometheus exposition body. Counter-or-gauge value, not
    /// histogram (histograms surface as `<name>_bucket{...le="..."}`,
    /// `<name>_sum`, `<name>_count` — see `histogram_*` helpers).
    fn scrape_counter_with_label(body: &str, name: &str, label_match: &str) -> u64 {
        for line in body.lines() {
            if line.starts_with('#') {
                continue;
            }
            if line.starts_with(name)
                && line.contains(label_match)
                && let Some(val) = line.rsplit(' ').next()
                && let Ok(n) = val.parse::<u64>()
            {
                return n;
            }
        }
        0
    }

    fn scrape_histogram_count(body: &str, name: &str, label_match: &str) -> u64 {
        let key = format!("{name}_count");
        for line in body.lines() {
            if line.starts_with('#') {
                continue;
            }
            if line.starts_with(&key)
                && line.contains(label_match)
                && let Some(val) = line.rsplit(' ').next()
                && let Ok(n) = val.parse::<u64>()
            {
                return n;
            }
        }
        0
    }

    /// Happy path: a backend that returns 3 turns bumps the histogram
    /// `_count` series for the matching backend label by one and
    /// does NOT bump `stt_failures_total{reason="transcription_empty"}`.
    ///
    /// Test-isolation note: the recorder is process-global and tests
    /// run in parallel by default, so each metric-emission test below
    /// uses a *distinct* backend name so the per-label time series
    /// are non-overlapping. This test uses `sherpa`; see the failure /
    /// empty-transcript / model-missing tests for the other choices.
    #[tokio::test]
    async fn transcribe_with_metrics_records_duration_on_success() {
        let handle = heron_metrics::init_prometheus_recorder().expect("recorder");
        let backend = StubMetricsBackend {
            name: "sherpa",
            outcome: StubOutcome::Ok(3),
        };
        let tmp = tempfile::TempDir::new().expect("tmp");
        let wav = tmp.path().join("in.wav");
        let partial = tmp.path().join("p.jsonl");
        // Need a real WAV so `read_partial_jsonl` doesn't fail
        // upstream (the stub doesn't write anything; the wrapper
        // only times the call, so we don't actually need partial
        // contents for this assertion).
        std::fs::write(&wav, b"fake").expect("write");

        // Test isolation: the recorder is process-global. We use the
        // `sherpa` backend bucket which no other metrics-emission
        // unit test in this crate touches, so the histogram-count
        // delta is exact under parallel execution. (The
        // `reason="transcription_empty"` invariant is exercised by
        // `transcribe_with_metrics_flags_empty_transcript` instead —
        // a "must not bump" assertion here would race with that test.)
        let before = handle.render();
        let before_count = scrape_histogram_count(&before, STT_DURATION_SECONDS, "sherpa");

        let _ = transcribe_with_metrics(
            &backend,
            &wav,
            Channel::Mic,
            SessionId::nil(),
            &partial,
            Box::new(|_| {}),
        )
        .await
        .expect("ok");

        let after = handle.render();
        let after_count = scrape_histogram_count(&after, STT_DURATION_SECONDS, "sherpa");
        assert_eq!(
            after_count - before_count,
            1,
            "happy-path call must bump the histogram count exactly once; got {} vs {} \
             from rendered exposition:\n{after}",
            after_count,
            before_count,
        );
    }

    /// Empty-transcript path: the wrapper still returns Ok (preserving
    /// the consumer's soft-fail contract), but bumps
    /// `stt_failures_total{reason="transcription_empty"}` exactly once.
    #[tokio::test]
    async fn transcribe_with_metrics_flags_empty_transcript() {
        let handle = heron_metrics::init_prometheus_recorder().expect("recorder");
        let backend = StubMetricsBackend {
            name: "whisperkit",
            outcome: StubOutcome::Ok(0),
        };
        let tmp = tempfile::TempDir::new().expect("tmp");
        let wav = tmp.path().join("in.wav");
        let partial = tmp.path().join("p.jsonl");
        std::fs::write(&wav, b"fake").expect("write");

        let before = handle.render();
        let before_empty = scrape_counter_with_label(
            &before,
            STT_FAILURES_TOTAL,
            "reason=\"transcription_empty\"",
        );

        let summary = transcribe_with_metrics(
            &backend,
            &wav,
            Channel::Mic,
            SessionId::nil(),
            &partial,
            Box::new(|_| {}),
        )
        .await
        .expect("ok");
        assert_eq!(summary.turns, 0);

        let after = handle.render();
        let after_empty =
            scrape_counter_with_label(&after, STT_FAILURES_TOTAL, "reason=\"transcription_empty\"");
        assert_eq!(
            after_empty - before_empty,
            1,
            "empty-transcript success path must bump the counter exactly once; got {} vs {} \
             from rendered exposition:\n{after}",
            after_empty,
            before_empty,
        );
    }

    /// Failure path: a backend returning `SttError::Failed` bumps
    /// `stt_failures_total{reason="failed"}` AND records the
    /// histogram (a long-tail timeout still wants its latency
    /// observed). The error propagates verbatim.
    ///
    /// Uses the `whisperkit_stub` label so the histogram time series
    /// here doesn't overlap with the success-path test running in
    /// parallel against `backend="sherpa"`.
    #[tokio::test]
    async fn transcribe_with_metrics_records_failure_reason() {
        let handle = heron_metrics::init_prometheus_recorder().expect("recorder");
        let backend = StubMetricsBackend {
            name: "whisperkit_stub",
            outcome: StubOutcome::Failed,
        };
        let tmp = tempfile::TempDir::new().expect("tmp");
        let wav = tmp.path().join("in.wav");
        let partial = tmp.path().join("p.jsonl");
        std::fs::write(&wav, b"fake").expect("write");

        let before = handle.render();
        let before_failed =
            scrape_counter_with_label(&before, STT_FAILURES_TOTAL, "reason=\"failed\"");
        let before_count = scrape_histogram_count(&before, STT_DURATION_SECONDS, "whisperkit_stub");

        let result = transcribe_with_metrics(
            &backend,
            &wav,
            Channel::Mic,
            SessionId::nil(),
            &partial,
            Box::new(|_| {}),
        )
        .await;
        assert!(matches!(result, Err(SttError::Failed(_))));

        let after = handle.render();
        let after_failed =
            scrape_counter_with_label(&after, STT_FAILURES_TOTAL, "reason=\"failed\"");
        let after_count = scrape_histogram_count(&after, STT_DURATION_SECONDS, "whisperkit_stub");
        assert_eq!(
            after_failed - before_failed,
            1,
            "failure path must bump stt_failures_total{{reason=failed}} exactly once",
        );
        assert_eq!(
            after_count - before_count,
            1,
            "failure path must still record duration so latency dashboards see timeout outliers",
        );
    }

    /// `ModelMissing` and `Unavailable` both map to the
    /// `model_unavailable` reason bucket — same operator action
    /// (download / install the model), one dashboard signal.
    #[tokio::test]
    async fn transcribe_with_metrics_collapses_model_errors_to_one_bucket() {
        let handle = heron_metrics::init_prometheus_recorder().expect("recorder");
        let backend = StubMetricsBackend {
            name: "whisperkit",
            outcome: StubOutcome::ModelMissing,
        };
        let tmp = tempfile::TempDir::new().expect("tmp");
        let wav = tmp.path().join("in.wav");
        let partial = tmp.path().join("p.jsonl");
        std::fs::write(&wav, b"fake").expect("write");

        let before = handle.render();
        let before_unavailable =
            scrape_counter_with_label(&before, STT_FAILURES_TOTAL, "reason=\"model_unavailable\"");

        let _ = transcribe_with_metrics(
            &backend,
            &wav,
            Channel::Mic,
            SessionId::nil(),
            &partial,
            Box::new(|_| {}),
        )
        .await;

        let after = handle.render();
        let after_unavailable =
            scrape_counter_with_label(&after, STT_FAILURES_TOTAL, "reason=\"model_unavailable\"");
        assert_eq!(
            after_unavailable - before_unavailable,
            1,
            "ModelMissing must bump the model_unavailable bucket exactly once",
        );
    }
}
