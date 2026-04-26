//! WhisperKit model-download Tauri command (gap #5b / phase 72+).
//!
//! (Tracked as "gap #3" in some upstream notes — the canonical entry
//! is row 5b of the punch list in `docs/archives/codebase-gaps.md`.)
//!
//! Wraps `heron_speech::WhisperKitBackend::ensure_model` behind a
//! `#[tauri::command]` so the §13.3 onboarding wizard's step 5 can
//! actually fetch the on-device STT model — replacing the previous
//! placeholder badge that only checked whether a model was already
//! on disk.
//!
//! ## Wire shape
//!
//! - Command: `heron_download_model`. Returns `Result<String, String>`
//!   where the success payload is a human-facing message
//!   ("WhisperKit model ready") and the error payload is a stringified
//!   `SttError`. The wizard renders both with the same `<TestStatus>`
//!   component the other onboarding steps use.
//! - Event: `model_download:progress`. Emitted at least twice (0.0 at
//!   the start of `ensure_model`, 1.0 just before it resolves Ok) and
//!   on every WhisperKit progress tick in between. Payload is
//!   `{ "fraction": f32 }` — clamped to `[0.0, 1.0]`.
//!
//! ## Why no `pub fn` core like `onboarding.rs` has
//!
//! The probes in `onboarding.rs` split sync wrappers from async cores
//! so unit tests can exercise the inner logic without a real Tokio
//! runtime. `ensure_model` itself is async (and lives in
//! `heron_speech`), and the Tauri command is also async, so the split
//! buys nothing. The interesting branches we DO want to test live in
//! `classify_ensure_model_result` below.
//!
//! ## Concurrency posture
//!
//! `WhisperKitBackend::ensure_model` is itself idempotent across
//! concurrent callers thanks to its `OnceCell`. Two simultaneous
//! `heron_download_model` invocations are therefore safe — both await
//! the same fetch — but the progress event channel is shared, so the
//! second caller would see the first caller's progress ticks. The
//! onboarding flow is a single-button wizard step, so a duplicate
//! invocation is a user double-click rather than a real concurrency
//! mode; we don't add a mutex on the desktop side.

// `SttBackend` trait methods (`ensure_model`) are exercised through
// the `Box<dyn SttBackend>` returned by `build_backend`; the trait
// itself doesn't need to be in scope at this call site.
use heron_speech::{ProgressFn, SttError, build_backend};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};

/// Tauri event name the renderer listens on for `[0.0, 1.0]` ticks.
///
/// Lives at `model_download:progress` (with the Tauri `:` separator
/// the WebView expects) so the listener name in
/// `Onboarding.tsx` stays a single source of truth.
pub const EVENT_MODEL_DOWNLOAD_PROGRESS: &str = "model_download:progress";

/// Wire-format payload for [`EVENT_MODEL_DOWNLOAD_PROGRESS`].
///
/// We intentionally name the field `fraction` rather than `progress`
/// to match `WhisperKitBackend::ensure_model`'s callback contract
/// ("value in `[0.0, 1.0]`"); a future regression that emits a
/// percentage (0..100) would surface immediately.
#[derive(Debug, Clone, Serialize)]
pub struct ProgressPayload {
    pub fraction: f32,
}

/// Tauri command body. See module-level docs for the contract.
///
/// Returns:
/// - `Ok(message)` when the underlying `ensure_model` resolves Ok.
///   `message` is the success copy the wizard renders next to the
///   green check ("WhisperKit model ready").
/// - `Err(message)` on every `SttError` variant. The frontend
///   surfaces this as a `Fail` outcome on the existing `<TestStatus>`
///   component; a future iteration could split `NotYetImplemented`
///   into a `Skipped`-style outcome, but for now the wizard treats
///   any non-Ok as failure.
pub async fn run_download<R: Runtime>(app: AppHandle<R>) -> Result<String, String> {
    let backend = build_backend("whisperkit").map_err(|e| format!("backend unavailable: {e}"))?;
    // Clone for the progress closure so the outer `app` stays available
    // for the post-`ensure_model` terminal-tick emit below.
    let app_for_progress = app.clone();
    let progress: ProgressFn = Box::new(move |fraction: f32| {
        // Clamp belt-and-suspenders. WhisperKit's bridge sends values
        // in `[0, 1]` already; if a future bridge bug emits something
        // outside that range, the renderer's progress bar would
        // misrender — clamp here so the wire contract stays tight.
        let clamped = fraction.clamp(0.0, 1.0);
        // Best-effort emit. A failed emit (e.g. a closed webview
        // mid-download) is logged but not propagated — losing a
        // progress tick is strictly cosmetic, and aborting the
        // download because the renderer disappeared would be worse
        // than letting the cache fill out anyway.
        if let Err(err) = app_for_progress.emit(
            EVENT_MODEL_DOWNLOAD_PROGRESS,
            ProgressPayload { fraction: clamped },
        ) {
            tracing::warn!(
                target: "heron_desktop::model_download",
                "progress emit failed: {err}",
            );
        }
    });

    let result = backend.ensure_model(progress).await;
    if result.is_ok() {
        // Belt-and-suspenders terminal tick. `WhisperKitBackend::ensure_model`
        // already fires `1.0` just before resolving Ok, but if a future
        // bridge change drops that tick the renderer's bar would stay
        // pinned below 100% on a green-check success. Re-emit here so the
        // contract holds even if the upstream guarantee softens.
        if let Err(err) = app.emit(
            EVENT_MODEL_DOWNLOAD_PROGRESS,
            ProgressPayload { fraction: 1.0 },
        ) {
            tracing::warn!(
                target: "heron_desktop::model_download",
                "terminal progress emit failed: {err}",
            );
        }
    }
    classify_ensure_model_result(result)
}

/// Pure mapping from `Result<(), SttError>` to the Tauri command's
/// `Result<String, String>` wire shape. Split out so the unit tests
/// can pin the user-facing copy for every error variant without
/// spinning up a real backend or runtime.
fn classify_ensure_model_result(result: Result<(), SttError>) -> Result<String, String> {
    match result {
        Ok(()) => Ok("WhisperKit model ready".to_owned()),
        Err(SttError::NotYetImplemented) => Err(
            "WhisperKit download is not available on this build (Apple Silicon + macOS 14+ \
             required). On other platforms, follow the bundled-model fallback in the docs."
                .to_owned(),
        ),
        Err(SttError::ModelMissing(detail)) => Err(format!(
            "Model directory unreachable: {detail}. Check HERON_WHISPERKIT_MODEL_DIR or remove \
             the override to fall back to the cache path."
        )),
        Err(SttError::Unavailable(detail)) => Err(format!("Backend unavailable: {detail}")),
        Err(SttError::Failed(detail)) => Err(format!("Download failed: {detail}")),
        Err(SttError::Io(err)) => Err(format!("Filesystem error during download: {err}")),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ok_as_ready_message() {
        let r = classify_ensure_model_result(Ok(()));
        assert!(matches!(r, Ok(ref msg) if msg.contains("ready")));
    }

    #[test]
    fn classifies_not_yet_implemented_with_platform_hint() {
        let r = classify_ensure_model_result(Err(SttError::NotYetImplemented));
        match r {
            Err(msg) => {
                assert!(
                    msg.contains("Apple Silicon"),
                    "expected platform hint, got {msg:?}"
                );
                assert!(
                    msg.contains("bundled-model fallback"),
                    "expected fallback hint, got {msg:?}"
                );
            }
            Ok(_) => panic!("expected Err for NotYetImplemented"),
        }
    }

    #[test]
    fn classifies_model_missing_carries_path_detail() {
        let r = classify_ensure_model_result(Err(SttError::ModelMissing(
            "/tmp/no-such-dir".to_owned(),
        )));
        match r {
            Err(msg) => {
                assert!(msg.contains("/tmp/no-such-dir"));
                assert!(msg.contains("HERON_WHISPERKIT_MODEL_DIR"));
            }
            Ok(_) => panic!("expected Err for ModelMissing"),
        }
    }

    #[test]
    fn classifies_unavailable_with_detail() {
        let r =
            classify_ensure_model_result(Err(SttError::Unavailable("missing dylib".to_owned())));
        match r {
            Err(msg) => assert!(msg.contains("missing dylib")),
            Ok(_) => panic!("expected Err for Unavailable"),
        }
    }

    #[test]
    fn classifies_failed_with_detail() {
        let r = classify_ensure_model_result(Err(SttError::Failed("network 500".to_owned())));
        match r {
            Err(msg) => {
                assert!(msg.contains("Download failed"));
                assert!(msg.contains("network 500"));
            }
            Ok(_) => panic!("expected Err for Failed"),
        }
    }

    #[test]
    fn classifies_io_with_detail() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked");
        let r = classify_ensure_model_result(Err(SttError::Io(io)));
        match r {
            Err(msg) => {
                assert!(msg.contains("Filesystem"));
                assert!(msg.contains("locked"));
            }
            Ok(_) => panic!("expected Err for Io"),
        }
    }

    #[test]
    fn progress_payload_serializes_with_fraction_field() {
        let p = ProgressPayload { fraction: 0.42 };
        let s = serde_json::to_string(&p).expect("ser");
        assert!(s.contains(r#""fraction""#));
        assert!(s.contains("0.42"));
    }
}
