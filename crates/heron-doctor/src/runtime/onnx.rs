//! ONNX runtime health check.
//!
//! `docs/plan.md` §8 calls out sherpa-onnx as the always-available
//! fallback (vs. the WhisperKit happy path). The crate ships its own
//! `libonnxruntime.dylib` via the `download-binaries` cargo feature on
//! `sherpa-rs`; the dylib is what `crates/heron-speech/src/sherpa.rs`
//! loads at startup. If the dylib is missing or fails to load the user
//! has no transcript path at all.
//!
//! **What we don't do here:** we don't actually load the runtime. The
//! sherpa dylib path is environment-sensitive (workspace-relative on a
//! dev box, bundle-relative under `.app`) and a load failure on this
//! machine is a known-environmental issue per `CLAUDE.md` — running
//! `cargo test --workspace` already fails `heron-cli` on dyld for the
//! same reason. Forcing the doctor to prove dylib loadability would
//! make this test suite flaky for the same reason.
//!
//! Instead the check **probes for the model artifacts on the
//! expected search path** via an [`OnnxProbe`] trait. The real impl
//! uses `dirs::cache_dir()` to inspect the same cache layout
//! `heron-speech` writes to (`<cache>/heron/sherpa/silero_vad.onnx`),
//! matching the constants in `crates/heron-speech/src/sherpa.rs`;
//! tests inject a stub that reports "found" / "missing" / "load error"
//! without touching the filesystem. That gives us the runtime check
//! the onboarding wizard wants (does this machine have the runtime
//! pieces it needs?) without coupling the doctor's CI to ONNX's
//! dylib-path quirks.
//!
//! ### What this does not check
//!
//! - The `libonnxruntime.dylib` itself is bundled inside the heron app
//!   via `sherpa-rs`'s `download-binaries` feature; verifying it is on
//!   the dyld search path requires loading the runtime, which the §16
//!   `heron-doctor` automation hook (out of scope here) will eventually
//!   do via a synthetic inference round-trip. v1 stays artifact-only.
//! - File contents — only existence and `len() > 0`. A truncated or
//!   corrupted `.onnx` blob would pass and fail at recognizer init.
//!   The §16 automation hook is the right layer for content
//!   verification (SHA-256 against the pinned hashes in
//!   `crates/heron-speech/src/sherpa.rs:93,97`).

use std::path::PathBuf;

use super::{CheckSeverity, RuntimeCheck, RuntimeCheckOptions, RuntimeCheckResult};

const NAME: &str = "onnx_runtime";

/// Outcome the [`OnnxProbe`] reports back. Distinguishing "missing"
/// from "load error" lets the wizard render different remediation
/// copy: missing → "click Download model" (gap #7 wires this up),
/// load error → "reinstall heron — your bundle is damaged."
///
/// `#[non_exhaustive]` so a future variant (e.g. `BundleStale`
/// when the upstream sherpa-onnx version mismatches) lands as
/// non-breaking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnnxProbeOutcome {
    /// VAD + ASR model artifacts are on disk and the runtime can load
    /// them. The happy path.
    Healthy,
    /// Model artifacts not on disk. Onboarding wizard's "download
    /// model" step has not yet run.
    ModelMissing { searched: PathBuf },
    /// Dylib / runtime present but failed to instantiate. Carries the
    /// underlying error so the support flow has something to grep.
    LoadError { message: String },
}

/// Trait the [`OnnxRuntimeCheck`] uses to ask "is the ONNX runtime
/// usable?" Indirected so tests can stub the answer without dragging
/// the real sherpa-rs deps into the doctor's test harness.
pub trait OnnxProbe: Send + Sync {
    fn probe(&self) -> OnnxProbeOutcome;
}

/// Real-world probe. Inspects the same cache layout
/// `crates/heron-speech/src/sherpa.rs` writes to:
///
/// ```text
/// <cache>/heron/sherpa/
///   silero_vad.onnx
///   sherpa-onnx-whisper-tiny.en/
///     tiny.en-encoder.int8.onnx
///     tiny.en-decoder.int8.onnx
///     tiny.en-tokens.txt
/// ```
///
/// Returns `Healthy` only if **every** required artifact above is
/// present and non-zero — a partial download (decoder or tokens
/// missing) would otherwise pass this probe but fail at recognizer
/// init. Returns `ModelMissing` otherwise. Does **not** attempt to
/// load the runtime — see the module doc for why.
pub fn real_probe() -> Box<dyn OnnxProbe> {
    Box::new(RealProbe)
}

/// Required artifact names inside `<cache>/heron/sherpa/`. Pinned
/// against the constants in `crates/heron-speech/src/sherpa.rs:87,
/// 95, 99-101`. If those constants ever change, this list must be
/// kept in sync — there is no shared module today (`heron-speech` is
/// an audio dep we explicitly don't want to pull into the diagnostic
/// crate to keep the build fast).
const VAD_FILE: &str = "silero_vad.onnx";
const WHISPER_BUNDLE_DIR: &str = "sherpa-onnx-whisper-tiny.en";
const WHISPER_ENCODER: &str = "tiny.en-encoder.int8.onnx";
const WHISPER_DECODER: &str = "tiny.en-decoder.int8.onnx";
const WHISPER_TOKENS: &str = "tiny.en-tokens.txt";

struct RealProbe;

impl OnnxProbe for RealProbe {
    fn probe(&self) -> OnnxProbeOutcome {
        // Mirror `heron-speech::sherpa::SherpaBackend::from_env`'s
        // resolution: `dirs::cache_dir()` first, override via
        // `HERON_SHERPA_MODEL_DIR` so a user / CI rig can point at a
        // pre-staged bundle.
        let sherpa_dir = match std::env::var_os("HERON_SHERPA_MODEL_DIR") {
            // `OsStr::is_empty` is stable since Rust 1.84; workspace
            // MSRV is 1.88 (`Cargo.toml::workspace.package.rust-version`)
            // so this is safe. `len() > 0` would also work but
            // clippy's `len_zero` lint flags it.
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => match dirs::cache_dir() {
                Some(p) => p.join("heron").join("sherpa"),
                None => {
                    return OnnxProbeOutcome::LoadError {
                        message: "could not resolve platform cache dir".to_owned(),
                    };
                }
            },
        };
        let bundle = sherpa_dir.join(WHISPER_BUNDLE_DIR);
        let required: [PathBuf; 4] = [
            sherpa_dir.join(VAD_FILE),
            bundle.join(WHISPER_ENCODER),
            bundle.join(WHISPER_DECODER),
            bundle.join(WHISPER_TOKENS),
        ];

        for path in &required {
            // Treat "doesn't exist", "exists but is empty", and "is
            // a directory not a file" as ModelMissing. Empty files
            // happen when a partial download leaves a 0-byte file
            // behind; the directory check guards against an out-of-
            // sync test fixture that pre-creates a same-named dir.
            // Remediation in every case is "re-run the download
            // step." (Per Gemini review at PR #125.)
            let present = std::fs::metadata(path)
                .map(|m| m.is_file() && m.len() > 0)
                .unwrap_or(false);
            if !present {
                return OnnxProbeOutcome::ModelMissing {
                    searched: path.clone(),
                };
            }
        }
        OnnxProbeOutcome::Healthy
    }
}

/// ONNX runtime health check. Construct with [`Self::new`] and a
/// real-world probe via [`real_probe`], or with a stub probe in
/// tests.
pub struct OnnxRuntimeCheck {
    probe: Box<dyn OnnxProbe>,
}

impl OnnxRuntimeCheck {
    pub fn new(probe: Box<dyn OnnxProbe>) -> Self {
        Self { probe }
    }
}

impl RuntimeCheck for OnnxRuntimeCheck {
    fn name(&self) -> &'static str {
        NAME
    }

    fn run(&self, _opts: &RuntimeCheckOptions) -> RuntimeCheckResult {
        match self.probe.probe() {
            OnnxProbeOutcome::Healthy => {
                RuntimeCheckResult::pass(NAME, "ONNX runtime + sherpa-onnx model artifacts present")
            }
            OnnxProbeOutcome::ModelMissing { searched } => RuntimeCheckResult {
                name: NAME,
                severity: CheckSeverity::Warn,
                summary: "sherpa-onnx model artifacts not yet downloaded".to_owned(),
                detail: format!(
                    "expected to find a non-empty file at {} — re-run \
                     onboarding's 'download model' step or set \
                     HERON_SHERPA_MODEL_DIR",
                    searched.display(),
                ),
            },
            OnnxProbeOutcome::LoadError { message } => RuntimeCheckResult::fail(
                NAME,
                "ONNX runtime failed to load",
                format!(
                    "sherpa-onnx returned: {message}. The bundled \
                     dylib may be missing or corrupt; reinstall \
                     heron and check Console.app for `dyld` errors.",
                ),
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    struct StubProbe(OnnxProbeOutcome);

    impl OnnxProbe for StubProbe {
        fn probe(&self) -> OnnxProbeOutcome {
            self.0.clone()
        }
    }

    fn check(outcome: OnnxProbeOutcome) -> RuntimeCheckResult {
        OnnxRuntimeCheck::new(Box::new(StubProbe(outcome))).run(&RuntimeCheckOptions::default())
    }

    #[test]
    fn healthy_probe_yields_pass() {
        let r = check(OnnxProbeOutcome::Healthy);
        assert_eq!(r.severity, CheckSeverity::Pass);
        assert_eq!(r.name, NAME);
        assert!(r.detail.is_empty());
    }

    #[test]
    fn missing_probe_yields_warn_with_search_path() {
        let r = check(OnnxProbeOutcome::ModelMissing {
            searched: PathBuf::from("/tmp/heron/sherpa/silero_vad.onnx"),
        });
        assert_eq!(r.severity, CheckSeverity::Warn);
        assert!(r.detail.contains("/tmp/heron/sherpa/silero_vad.onnx"));
    }

    #[test]
    fn load_error_yields_fail_with_message() {
        let r = check(OnnxProbeOutcome::LoadError {
            message: "image not found: libonnxruntime.dylib".to_owned(),
        });
        assert_eq!(r.severity, CheckSeverity::Fail);
        assert!(r.detail.contains("libonnxruntime.dylib"));
    }

    #[test]
    fn name_is_stable() {
        let c = OnnxRuntimeCheck::new(Box::new(StubProbe(OnnxProbeOutcome::Healthy)));
        assert_eq!(c.name(), "onnx_runtime");
    }

    #[test]
    fn real_probe_does_not_panic() {
        // Smoke test: real_probe() against an unconfigured machine
        // should return ModelMissing (or LoadError if HOME is unset)
        // without panicking. It MUST NOT actually load any dylib.
        let p = real_probe();
        let outcome = p.probe();
        match outcome {
            OnnxProbeOutcome::Healthy
            | OnnxProbeOutcome::ModelMissing { .. }
            | OnnxProbeOutcome::LoadError { .. } => {}
        }
    }

    /// Tests that touch `HERON_SHERPA_MODEL_DIR`. Held under one
    /// process-global mutex because `set_var` is racy across threads
    /// under Rust 2024.
    mod real_probe_dir_override {
        use super::*;
        use std::sync::Mutex;
        use tempfile::TempDir;

        // Crate-local lock; isolated from the workspace-wide
        // `heron_llm::test_env::ENV_LOCK` since this test mutates a
        // distinct env var.
        static ENV_LOCK: Mutex<()> = Mutex::new(());

        fn with_override<R>(dir: &std::path::Path, body: impl FnOnce() -> R) -> R {
            let _g = ENV_LOCK.lock().expect("env lock");
            let saved = std::env::var_os("HERON_SHERPA_MODEL_DIR");
            // SAFETY: process-global env mutation is unsafe under
            // Rust 2024. ENV_LOCK serializes; we restore the prior
            // value on exit (paired Drop would be cleaner but a tiny
            // helper is plenty for two tests).
            unsafe {
                std::env::set_var("HERON_SHERPA_MODEL_DIR", dir);
            }
            let r = body();
            unsafe {
                match saved {
                    Some(v) => std::env::set_var("HERON_SHERPA_MODEL_DIR", v),
                    None => std::env::remove_var("HERON_SHERPA_MODEL_DIR"),
                }
            }
            r
        }

        fn write_bundle(root: &std::path::Path, files: &[(&str, usize)]) {
            for (rel, size) in files {
                let p = root.join(rel);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).expect("mkdir");
                }
                std::fs::write(&p, vec![0u8; *size]).expect("write");
            }
        }

        #[test]
        fn complete_bundle_yields_healthy() {
            let tmp = TempDir::new().expect("tmp");
            write_bundle(
                tmp.path(),
                &[
                    (VAD_FILE, 1024),
                    (&format!("{WHISPER_BUNDLE_DIR}/{WHISPER_ENCODER}"), 1024),
                    (&format!("{WHISPER_BUNDLE_DIR}/{WHISPER_DECODER}"), 1024),
                    (&format!("{WHISPER_BUNDLE_DIR}/{WHISPER_TOKENS}"), 1024),
                ],
            );
            with_override(tmp.path(), || {
                let outcome = real_probe().probe();
                assert_eq!(outcome, OnnxProbeOutcome::Healthy);
            });
        }

        #[test]
        fn partial_bundle_missing_decoder_yields_model_missing() {
            // Codex flagged that probing only encoder lets a partial
            // bundle false-pass. Pin the regression: a bundle with
            // VAD + encoder but no decoder must NOT be Healthy.
            let tmp = TempDir::new().expect("tmp");
            write_bundle(
                tmp.path(),
                &[
                    (VAD_FILE, 1024),
                    (&format!("{WHISPER_BUNDLE_DIR}/{WHISPER_ENCODER}"), 1024),
                    // decoder + tokens deliberately absent
                ],
            );
            with_override(tmp.path(), || {
                let outcome = real_probe().probe();
                assert!(matches!(outcome, OnnxProbeOutcome::ModelMissing { .. }));
            });
        }

        #[test]
        fn empty_file_yields_model_missing() {
            // Partial-download case: file exists but is 0 bytes.
            let tmp = TempDir::new().expect("tmp");
            write_bundle(
                tmp.path(),
                &[
                    (VAD_FILE, 0),
                    (&format!("{WHISPER_BUNDLE_DIR}/{WHISPER_ENCODER}"), 1024),
                    (&format!("{WHISPER_BUNDLE_DIR}/{WHISPER_DECODER}"), 1024),
                    (&format!("{WHISPER_BUNDLE_DIR}/{WHISPER_TOKENS}"), 1024),
                ],
            );
            with_override(tmp.path(), || {
                let outcome = real_probe().probe();
                match outcome {
                    OnnxProbeOutcome::ModelMissing { searched } => {
                        assert!(searched.ends_with(VAD_FILE));
                    }
                    other => panic!("expected ModelMissing, got {other:?}"),
                }
            });
        }

        #[test]
        fn empty_override_falls_back_to_cache_dir() {
            // An empty `HERON_SHERPA_MODEL_DIR=""` must NOT be
            // treated as "use empty dir" (which would always
            // ModelMissing) — must fall through to dirs::cache_dir.
            let _g = ENV_LOCK.lock().expect("env lock");
            let saved = std::env::var_os("HERON_SHERPA_MODEL_DIR");
            unsafe {
                std::env::set_var("HERON_SHERPA_MODEL_DIR", "");
            }
            let outcome = real_probe().probe();
            // Whatever the cache dir resolves to, the call must not
            // panic and must not have searched "" (root) — assert
            // the searched path, if any, isn't a bare slash.
            match outcome {
                OnnxProbeOutcome::ModelMissing { searched } => {
                    assert!(!searched.as_os_str().is_empty());
                    assert_ne!(searched, std::path::PathBuf::from("/"));
                }
                OnnxProbeOutcome::Healthy | OnnxProbeOutcome::LoadError { .. } => {}
            }
            unsafe {
                match saved {
                    Some(v) => std::env::set_var("HERON_SHERPA_MODEL_DIR", v),
                    None => std::env::remove_var("HERON_SHERPA_MODEL_DIR"),
                }
            }
        }
    }
}
