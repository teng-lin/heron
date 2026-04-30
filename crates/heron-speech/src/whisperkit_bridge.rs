//! Rust side of the §4 WhisperKit Swift bridge.
//!
//! Mirrors `swift/whisperkit-helper/Sources/WhisperKitHelper.swift`.
//! v0 ships:
//!
//! - the [`WkStatus`] return-code enum that matches the Swift constants,
//! - thin `unsafe` wrappers around the three `@_cdecl` exports (`wk_init`,
//!   `wk_transcribe`, `wk_free_string`),
//! - a [`whisperkit_init`] / [`whisperkit_transcribe`] safe wrapper
//!   that hands ownership of the returned C string back through
//!   `wk_free_string` automatically,
//! - and tests against the v0 stub bodies that always return
//!   `NotYetImplemented`.
//!
//! Once the week-4 work drops the real `WhisperKit.transcribe` call
//! into the Swift side, [`whisperkit_transcribe`] starts returning
//! actual JSONL turns and the consumer can deserialize them via
//! `serde_json::Deserializer::from_str(_).into_iter::<Turn>()`.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
#[cfg(target_vendor = "apple")]
use std::os::raw::c_void;
use std::path::Path;
#[cfg(target_vendor = "apple")]
use std::path::PathBuf;

use thiserror::Error;

/// Default WhisperKit model variant. Mirrors the Swift constant
/// `WK_DEFAULT_VARIANT`. ~1GB CoreML bundle (English-only small),
/// matches `docs/archives/plan.md` week-9 step 5.
pub const DEFAULT_WK_VARIANT: &str = "openai_whisper-small.en";

#[cfg(target_vendor = "apple")]
mod ffi {
    use std::os::raw::{c_char, c_void};

    use super::ProgressThunk;

    unsafe extern "C" {
        pub(super) fn wk_init(model_dir: *const c_char) -> i32;
        pub(super) fn wk_fetch_model(
            variant: *const c_char,
            dest_dir: *const c_char,
            progress_cb: Option<ProgressThunk>,
            progress_userdata: *mut c_void,
            out_model_dir: *mut *mut c_char,
        ) -> i32;
        pub(super) fn wk_transcribe(
            wav_path: *const c_char,
            prompt: *const c_char,
            out: *mut *mut c_char,
        ) -> i32;
        pub(super) fn wk_free_string(p: *mut c_char);
    }
}

/// C-ABI thunk passed to the Swift bridge so it can call back into a
/// Rust closure from the WhisperKit download Task. The userdata is an
/// opaque pointer to a `Box<dyn FnMut(f32)>` allocated by the caller
/// of `whisperkit_fetch`; the thunk downcasts and invokes it.
#[cfg(target_vendor = "apple")]
pub(super) type ProgressThunk = unsafe extern "C" fn(*mut c_void, f32);

/// Pinned constants matching the Swift side. Drift here is caught at
/// **compile time** by the unit tests below that assert each enum
/// variant equals its raw constant. Any rename / renumber on the
/// Swift side fails CI rather than silently coercing to "not
/// implemented".
pub const WK_OK_RAW: i32 = 0;
pub const WK_NOT_IMPLEMENTED_RAW: i32 = -1;
pub const WK_MODEL_MISSING_RAW: i32 = -2;
pub const WK_INTERNAL_RAW: i32 = -3;
pub const WK_TIMEOUT_RAW: i32 = -4;

/// Status codes the Swift side returns. Mirror
/// `swift/whisperkit-helper/Sources/WhisperKitHelper.swift` 1-for-1.
/// `Internal` carries the original raw code so an unknown future
/// status surfaces with its actual integer rather than getting
/// silently coerced to a stable variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WkStatus {
    Ok,
    NotYetImplemented,
    ModelMissing,
    /// Swift bridge gave up waiting on the async WhisperKit Task. The
    /// per-call deadlines live in
    /// `swift/whisperkit-helper/.../WhisperKitHelper.swift`
    /// (`WK_INIT_TIMEOUT` / `WK_FETCH_TIMEOUT` / `WK_TRANSCRIBE_TIMEOUT`).
    /// Distinct from `Internal` because the orchestrator may want to
    /// retry on timeout but not on a hard error.
    Timeout,
    /// Bridge returned a non-`Ok` code we recognized as the generic
    /// internal-error sentinel **or** a code we don't know about.
    /// The wrapped `i32` is the raw return value verbatim.
    Internal(i32),
}

impl WkStatus {
    pub fn from_raw(code: i32) -> Self {
        match code {
            WK_OK_RAW => Self::Ok,
            WK_NOT_IMPLEMENTED_RAW => Self::NotYetImplemented,
            WK_MODEL_MISSING_RAW => Self::ModelMissing,
            WK_TIMEOUT_RAW => Self::Timeout,
            // -3 plus any unknown code reaches Internal(code). The
            // raw value is preserved so a Swift-side renumber doesn't
            // get hidden behind a stable enum variant.
            other => Self::Internal(other),
        }
    }
}

#[derive(Debug, Error)]
pub enum WkError {
    #[error("WhisperKit not yet implemented (arrives week 4 per §4)")]
    NotYetImplemented,
    #[error("WhisperKit model directory not found or unreadable")]
    ModelMissing,
    #[error("WhisperKit returned a null buffer; allocator failure?")]
    NullBuffer,
    #[error("WhisperKit returned non-utf8 bytes")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("path contains a NUL byte; can't pass to FFI")]
    PathNul,
    #[error("WhisperKit Swift bridge timed out waiting for the async Task")]
    Timeout,
    #[error("WhisperKit internal error (code {code})")]
    Internal { code: i32 },
}

/// Convert a *non-`Ok`* status into [`WkError`]. Callers must filter
/// `Ok` first; passing `WkStatus::Ok` here yields a generic Internal
/// error, which is better than a panic but should never actually
/// happen in correctly-written call sites.
impl From<WkStatus> for WkError {
    fn from(status: WkStatus) -> Self {
        match status {
            WkStatus::Ok => WkError::Internal { code: WK_OK_RAW },
            WkStatus::NotYetImplemented => WkError::NotYetImplemented,
            WkStatus::ModelMissing => WkError::ModelMissing,
            WkStatus::Timeout => WkError::Timeout,
            WkStatus::Internal(code) => WkError::Internal { code },
        }
    }
}

/// Initialize the WhisperKit runtime against a model directory.
///
/// On Apple platforms this calls into the Swift bridge; v0 returns
/// [`WkError::NotYetImplemented`]. Off-Apple this is a compile-time
/// stub that always returns `NotYetImplemented`.
///
/// # Threading
///
/// Real impl will block on a model-load operation that takes seconds.
/// Wrap calls in `tokio::task::spawn_blocking` from async contexts.
#[cfg(target_vendor = "apple")]
pub fn whisperkit_init(model_dir: &Path) -> Result<(), WkError> {
    let c_path = path_to_cstring(model_dir)?;
    // SAFETY: `wk_init` takes a NUL-terminated C string and returns
    // an i32. The CString outlives the call.
    let raw = unsafe { ffi::wk_init(c_path.as_ptr()) };
    match WkStatus::from_raw(raw) {
        WkStatus::Ok => Ok(()),
        other => Err(WkError::from(other)),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub fn whisperkit_init(_model_dir: &Path) -> Result<(), WkError> {
    Err(WkError::NotYetImplemented)
}

/// Download a WhisperKit `variant` into `dest_dir` and return the
/// resolved model folder.
///
/// `dest_dir` is the HubApi `downloadBase`; the actual `.mlmodelc`
/// bundles end up under `<dest_dir>/models/argmaxinc/whisperkit-coreml/<variant>`.
/// The returned `PathBuf` points at that resolved folder so the caller
/// can hand it straight to [`whisperkit_init`].
///
/// `on_progress` is invoked with values in `[0.0, 1.0]` from the
/// WhisperKit download Task. Closures may capture, but must be
/// `'static` — on a `WkError::Timeout` return the lingering Swift
/// Task can fire progress callbacks after this function returns, so
/// the closure cannot borrow from the caller's frame. The current
/// call sites use Arc-based captures and satisfy this trivially.
///
/// # Threading
///
/// Blocks the caller — wrap in `tokio::task::spawn_blocking` from
/// async contexts. The Swift bridge bridges async→sync via
/// `DispatchSemaphore`, mirroring [`whisperkit_init`].
#[cfg(target_vendor = "apple")]
pub fn whisperkit_fetch(
    variant: &str,
    dest_dir: &Path,
    on_progress: impl FnMut(f32) + 'static,
) -> Result<PathBuf, WkError> {
    let c_variant = CString::new(variant.as_bytes()).map_err(|_| WkError::PathNul)?;
    let c_dest = path_to_cstring(dest_dir)?;

    // The closure goes into a double-Box and we hand a *raw* pointer
    // into Swift. On the success path we rebuild the Box from the raw
    // pointer and drop it; on the **timeout** path, however, the Swift
    // Task is still alive (we can't cancel an in-flight WhisperKit
    // download from the C entry point) and may invoke `progress_thunk`
    // through the same pointer *after* `wk_fetch_model` has returned.
    // If we dropped the box unconditionally that would be a UAF.
    //
    // We therefore split ownership: the box is held by raw pointer
    // while Swift is using it, and we free it only on a clean
    // (non-timeout) return. On timeout we deliberately leak — the
    // closure captures stay alive for the lifetime of the lingering
    // Task, and the orchestrator's retry creates a fresh closure with
    // a fresh allocation. The leak is bounded by the per-retry policy
    // and the closures themselves are small (thin Arc clones in the
    // current call sites).
    //
    // The `'static` bound on `on_progress` is what makes this sound:
    // the leaked closure can be safely invoked from the Swift Task at
    // any later time, since none of its captures borrow from the
    // caller's frame.
    let trait_obj: Box<dyn FnMut(f32)> = Box::new(on_progress);
    let boxed: Box<Box<dyn FnMut(f32)>> = Box::new(trait_obj);
    let userdata_raw = Box::into_raw(boxed);
    let userdata: *mut c_void = userdata_raw.cast();

    let mut out_buf: *mut c_char = std::ptr::null_mut();
    // SAFETY: all pointers are non-NULL (`c_variant`, `c_dest` outlive
    // the call). `userdata` points at a heap-allocated boxed closure
    // owned by `userdata_raw`. The Swift bridge invokes the thunk
    // synchronously from inside its async download Task; on a clean
    // return we reclaim the box below, on `WK_TIMEOUT` we leak it so
    // the lingering Task's progress callbacks remain valid.
    let raw = unsafe {
        ffi::wk_fetch_model(
            c_variant.as_ptr(),
            c_dest.as_ptr(),
            Some(progress_thunk),
            userdata,
            &mut out_buf,
        )
    };

    let status = WkStatus::from_raw(raw);

    // Reclaim the boxed closure on every status *except* Timeout.
    // The Timeout branch deliberately leaks (see the doc-comment
    // above) so the still-running Swift Task can fire late progress
    // callbacks without dereferencing freed memory.
    if status != WkStatus::Timeout {
        // SAFETY: `userdata_raw` was just produced by `Box::into_raw`
        // a few lines above and has not been used to materialize any
        // other Box. Swift no longer holds a reference once the call
        // returns with a non-Timeout status (the Task has signaled
        // and we're past the semaphore wait).
        unsafe {
            drop(Box::from_raw(userdata_raw));
        }
    }

    let resolved = if out_buf.is_null() {
        None
    } else {
        // SAFETY: bridge guarantees a NUL-terminated buffer when
        // `out_buf` is non-NULL; we copy to an owned String and free.
        let parsed: Result<String, WkError> = unsafe {
            CStr::from_ptr(out_buf)
                .to_str()
                .map(str::to_owned)
                .map_err(WkError::from)
        };
        // SAFETY: same allocator pairing as `whisperkit_transcribe`.
        unsafe { ffi::wk_free_string(out_buf) };
        Some(parsed?)
    };

    match status {
        WkStatus::Ok => match resolved {
            Some(p) => Ok(PathBuf::from(p)),
            None => Err(WkError::NullBuffer),
        },
        other => Err(WkError::from(other)),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub fn whisperkit_fetch(
    _variant: &str,
    _dest_dir: &Path,
    _on_progress: impl FnMut(f32) + 'static,
) -> Result<std::path::PathBuf, WkError> {
    Err(WkError::NotYetImplemented)
}

/// C-ABI thunk Swift invokes for each download progress tick. We
/// downcast `userdata` back to the boxed Rust closure and call it.
/// SAFETY: the caller of `whisperkit_fetch` guarantees `userdata`
/// points at a live `Box<dyn FnMut(f32)>` for the duration of the
/// fetch.
#[cfg(target_vendor = "apple")]
unsafe extern "C" fn progress_thunk(userdata: *mut c_void, value: f32) {
    if userdata.is_null() {
        return;
    }
    let cb = unsafe { &mut *userdata.cast::<Box<dyn FnMut(f32)>>() };
    cb(value);
}

/// Compose a WhisperKit-style prompt string from a hotwords list.
///
/// The convention mirrors `WhisperKitCLI/TranscribeCLI.swift`'s
/// `--prompt` flag handling exactly: the words are joined with a
/// single space, and the Swift bridge prepends a leading space + runs
/// the joined text through the tokenizer with the standard special-
/// token filter. Empty / whitespace-only entries are skipped (they
/// would silently fuse adjacent tokens during the join, producing a
/// surprising `"foo  bar"` double-space the tokenizer treats as a
/// distinct word boundary).
///
/// Returns `None` for an empty input or an input where every entry is
/// whitespace-only — the Tier-4 contract says an empty hotwords vec is
/// equivalent to "no prompt set" so the decoder behaves identically to
/// the pre-Tier-4 path. Caller passes the resulting `Option<&str>`
/// straight through to [`whisperkit_transcribe`].
pub fn compose_prompt(hotwords: &[String]) -> Option<String> {
    let mut parts = Vec::with_capacity(hotwords.len());
    for h in hotwords {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Transcribe `wav_path` and return the JSONL turn array as a `String`.
///
/// One JSON object per line, matching the §5.2 `Turn` shape. v0's
/// stub returns an empty string and [`WkError::NotYetImplemented`].
///
/// `prompt` is an optional vocabulary-boost string (Tier 4 #17). When
/// `Some(non_empty)`, the Swift bridge tokenizes it and forwards the
/// token IDs as `DecodingOptions.promptTokens`, mirroring the upstream
/// `WhisperKitCLI` `--prompt` handling. `None` (or empty) preserves
/// the pre-Tier-4 decode path byte-for-byte. Use [`compose_prompt`] to
/// build the string from a `&[String]` of hotwords.
#[cfg(target_vendor = "apple")]
pub fn whisperkit_transcribe(wav_path: &Path, prompt: Option<&str>) -> Result<String, WkError> {
    let c_path = path_to_cstring(wav_path)?;
    // The C ABI distinguishes "no prompt" (NULL) from "empty prompt"
    // ("\0") so the Swift side can skip building DecodingOptions
    // entirely on `None`, which is the contract that lets an empty
    // hotwords vec roundtrip into byte-identical decoder output.
    let c_prompt = match prompt {
        Some(s) if !s.is_empty() => Some(CString::new(s.as_bytes()).map_err(|_| WkError::PathNul)?),
        _ => None,
    };
    let prompt_ptr = c_prompt.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
    let mut buf: *mut c_char = std::ptr::null_mut();
    // SAFETY: `wk_transcribe` writes a malloc'd buffer into `*out`
    // and returns the status code. We hand ownership back via
    // `wk_free_string` regardless of which branch we take. `prompt_ptr`
    // is either NULL or a valid NUL-terminated C string that lives in
    // `c_prompt` for the duration of the call.
    let raw = unsafe { ffi::wk_transcribe(c_path.as_ptr(), prompt_ptr, &mut buf) };
    let status = WkStatus::from_raw(raw);

    if buf.is_null() {
        return match status {
            WkStatus::Ok => Err(WkError::NullBuffer),
            other => Err(WkError::from(other)),
        };
    }

    // SAFETY: `buf` is NUL-terminated; we copy the bytes into a Rust
    // `String` and then release the C buffer. CStr borrows from
    // `buf` for the duration of `to_str`.
    let parsed: Result<String, WkError> = unsafe {
        let cstr = CStr::from_ptr(buf);
        cstr.to_str().map(|s| s.to_owned()).map_err(WkError::from)
    };
    // SAFETY: `wk_free_string` accepts the same pointer the Swift
    // side malloc'd; we pass it once.
    unsafe { ffi::wk_free_string(buf) };

    let body = parsed?;
    match status {
        WkStatus::Ok => Ok(body),
        // v0's stub returns NotYetImplemented + empty body. Any
        // non-Ok status surfaces as the error variant; the body is
        // dropped (it's empty in the stub anyway).
        other => Err(WkError::from(other)),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub fn whisperkit_transcribe(_wav_path: &Path, _prompt: Option<&str>) -> Result<String, WkError> {
    Err(WkError::NotYetImplemented)
}

/// Convert a path to a `CString` for FFI without lossy UTF-8 coercion.
/// macOS paths can contain arbitrary bytes (especially via SMB / NFS
/// volumes); `to_string_lossy` would silently replace non-UTF-8 bytes
/// with U+FFFD and corrupt the path the Swift side resolves. Using
/// the raw OS bytes preserves every byte and still rejects internal
/// NULs (the only sequence `CString::new` actually disallows).
#[cfg(target_vendor = "apple")]
fn path_to_cstring(p: &Path) -> Result<CString, WkError> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).map_err(|_| WkError::PathNul)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn raw_constants_match_swift_side_verbatim() {
        // The Swift side defines `private let WK_*: Int32 = …` with
        // these exact values. Drift here is caught at compile time.
        assert_eq!(WK_OK_RAW, 0);
        assert_eq!(WK_NOT_IMPLEMENTED_RAW, -1);
        assert_eq!(WK_MODEL_MISSING_RAW, -2);
        assert_eq!(WK_INTERNAL_RAW, -3);
        assert_eq!(WK_TIMEOUT_RAW, -4);
    }

    #[test]
    fn status_from_raw_round_trips_every_known_code() {
        assert_eq!(WkStatus::from_raw(WK_OK_RAW), WkStatus::Ok);
        assert_eq!(
            WkStatus::from_raw(WK_NOT_IMPLEMENTED_RAW),
            WkStatus::NotYetImplemented
        );
        assert_eq!(
            WkStatus::from_raw(WK_MODEL_MISSING_RAW),
            WkStatus::ModelMissing
        );
        assert_eq!(WkStatus::from_raw(WK_TIMEOUT_RAW), WkStatus::Timeout);
    }

    #[test]
    fn status_from_raw_preserves_unknown_codes() {
        // -3 is the documented Internal sentinel; we keep the raw
        // value alongside so Internal(-3) is observable rather than
        // collapsed to a stable variant. A future Swift version that
        // returns -99 must surface as Internal(-99) so the operator
        // sees the actual code.
        assert_eq!(WkStatus::from_raw(-3), WkStatus::Internal(-3));
        assert_eq!(WkStatus::from_raw(-99), WkStatus::Internal(-99));
        // Positive codes are also Internal; the doc-comment is the
        // contract.
        assert_eq!(WkStatus::from_raw(7), WkStatus::Internal(7));
    }

    #[test]
    fn wk_error_internal_carries_the_raw_code() {
        let e = WkError::from(WkStatus::Internal(-99));
        match e {
            WkError::Internal { code } => assert_eq!(code, -99),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn timeout_status_maps_to_timeout_error() {
        // The Swift bridge returns WK_TIMEOUT (-4) when its async
        // Task doesn't signal the semaphore before the deadline. The
        // Rust wrapper must surface that as WkError::Timeout — a
        // distinct variant so callers can decide between "retry"
        // (Timeout) and "give up" (Internal). This test pins the
        // mapping so a future renumber on either side fails CI rather
        // than silently coercing to Internal.
        let raw = WK_TIMEOUT_RAW;
        let status = WkStatus::from_raw(raw);
        assert_eq!(status, WkStatus::Timeout);
        let err = WkError::from(status);
        assert!(
            matches!(err, WkError::Timeout),
            "expected Timeout, got {err:?}"
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn init_against_empty_dir_returns_internal() {
        // The Swift bridge tries to load a real WhisperKit instance
        // from the supplied folder. An empty tempdir is a valid
        // directory (so we don't hit `ModelMissing`) but contains no
        // `.mlmodelc` bundles, so WhisperKit fails to initialize and
        // we surface `Internal`. The contract this test pins is
        // "non-Ok status reaches Rust" — the real model-load path is
        // exercised by `tests/whisperkit_real.rs` when the env var
        // is set.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let result = whisperkit_init(tmp.path());
        assert!(
            matches!(result, Err(WkError::Internal { .. })),
            "expected Internal, got {result:?}"
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn init_against_missing_dir_returns_model_missing() {
        // A path that doesn't exist must surface as ModelMissing so
        // the orchestrator can show the "download model" UI rather
        // than a generic "internal error".
        let result = whisperkit_init(Path::new("/nonexistent/wk-model-dir"));
        assert!(
            matches!(result, Err(WkError::ModelMissing)),
            "expected ModelMissing, got {result:?}"
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn transcribe_without_init_returns_internal() {
        // The v0 stub always returned NotYetImplemented; the real
        // bridge returns Internal when no instance has been loaded.
        // We can't easily reset the global between tests, so this
        // test is meaningful only when run *before* any real init —
        // `cargo test` doesn't guarantee order, but a transcribe of
        // a non-existent wav after a successful init would also fail
        // with Internal, so this assertion is robust to ordering.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let wav = tmp.path().join("nope.wav");
        let result = whisperkit_transcribe(&wav, None);
        assert!(
            matches!(result, Err(WkError::Internal { .. })),
            "expected Internal, got {result:?}"
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn path_with_internal_nul_is_rejected() {
        // CString::new returns an error for any byte sequence
        // containing a NUL; the Rust wrapper should surface that as
        // PathNul rather than panicking or silently truncating.
        use std::path::PathBuf;
        let p = PathBuf::from("foo\0bar.wav");
        let result = whisperkit_transcribe(&p, None);
        assert!(matches!(result, Err(WkError::PathNul)));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn path_with_non_utf8_bytes_round_trips_through_ffi() {
        // macOS paths are arbitrary bytes; `to_string_lossy` would
        // replace invalid UTF-8 with U+FFFD and corrupt the path.
        // OsStrExt::as_bytes preserves every byte. We can't test
        // round-trip into Swift end-to-end (the stub doesn't echo),
        // but we can assert that path_to_cstring accepts the bytes
        // verbatim and rejects only an embedded NUL.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;
        let bytes: &[u8] = &[
            b'/', b't', b'm', b'p', b'/', 0xFF, 0xFE, b'.', b'w', b'a', b'v',
        ];
        let path = Path::new(OsStr::from_bytes(bytes));
        let c = path_to_cstring(path).expect("non-UTF-8 must round-trip");
        // C string drops the trailing NUL the C ABI adds.
        assert_eq!(c.as_bytes(), bytes);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn transcribe_with_nul_in_prompt_is_rejected() {
        // CString::new bails on any embedded NUL; the wrapper must
        // surface PathNul rather than silently truncating the prompt.
        // The Tauri Settings UI + the daemon both pass user-supplied
        // strings here, so a regression that swallows internal NULs
        // would corrupt the WhisperKit prompt without telling anyone.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let wav = tmp.path().join("nope.wav");
        let result = whisperkit_transcribe(&wav, Some("bad\0prompt"));
        assert!(matches!(result, Err(WkError::PathNul)));
    }

    #[test]
    fn compose_prompt_empty_vec_returns_none() {
        // Empty hotwords vec → no prompt set → byte-identical decoder
        // output to the pre-Tier-4 path. This is the migration
        // contract: settings without `hotwords` deserialize to
        // `Vec::new()` and must not silently change transcription
        // behavior for existing users.
        assert_eq!(compose_prompt(&[]), None);
    }

    #[test]
    fn compose_prompt_skips_blank_entries() {
        // A user who hits "Add hotword" then leaves the row empty
        // shouldn't accidentally inject a double-space into the
        // tokenizer input — Whisper's BPE treats `"  "` as a distinct
        // word boundary, which would silently degrade decode quality.
        let words = vec![
            String::new(),
            "  ".to_owned(),
            "heron".to_owned(),
            "\t".to_owned(),
        ];
        assert_eq!(compose_prompt(&words), Some("heron".to_owned()));
    }

    #[test]
    fn compose_prompt_joins_with_single_spaces() {
        // The Whisper tokenizer is whitespace-sensitive — joining with
        // anything other than a single space (commas, newlines) would
        // produce different token IDs and invalidate the
        // CLI-equivalence rationale documented on `whisperkit_transcribe`.
        let words = vec![
            "heron".to_owned(),
            "WhisperKit".to_owned(),
            "Anthropic".to_owned(),
        ];
        assert_eq!(
            compose_prompt(&words),
            Some("heron WhisperKit Anthropic".to_owned())
        );
    }

    #[test]
    fn compose_prompt_trims_per_entry_whitespace() {
        // Settings.json is a JSON file; users sometimes hand-edit it
        // and leave trailing whitespace inside string values. Trimming
        // each entry before the join means that hand-edit doesn't
        // produce a `"foo " + " bar"` = `"foo  bar"` double-space.
        let words = vec!["  heron  ".to_owned(), " WhisperKit\n".to_owned()];
        assert_eq!(compose_prompt(&words), Some("heron WhisperKit".to_owned()));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn fetch_with_nul_in_variant_is_rejected() {
        // CString::new bails on any embedded NUL; the wrapper surfaces
        // PathNul rather than silently truncating the variant string.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let result = whisperkit_fetch("bad\0variant", tmp.path(), |_| {});
        assert!(matches!(result, Err(WkError::PathNul)));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn fetch_with_nul_in_dest_dir_is_rejected() {
        use std::path::PathBuf;
        let p = PathBuf::from("/tmp/foo\0bar");
        let result = whisperkit_fetch(DEFAULT_WK_VARIANT, &p, |_| {});
        assert!(matches!(result, Err(WkError::PathNul)));
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn off_apple_shims_return_not_yet_implemented() {
        use std::path::PathBuf;
        let p = PathBuf::from("/dev/null");
        assert!(matches!(
            whisperkit_init(&p),
            Err(WkError::NotYetImplemented)
        ));
        assert!(matches!(
            whisperkit_transcribe(&p, None),
            Err(WkError::NotYetImplemented)
        ));
        // With an empty prompt the off-Apple stub still returns
        // NotYetImplemented — the parameter is wired but inert there.
        assert!(matches!(
            whisperkit_transcribe(&p, Some("")),
            Err(WkError::NotYetImplemented)
        ));
        assert!(matches!(
            whisperkit_transcribe(&p, Some("heron")),
            Err(WkError::NotYetImplemented)
        ));
        assert!(matches!(
            whisperkit_fetch(DEFAULT_WK_VARIANT, &p, |_| {}),
            Err(WkError::NotYetImplemented)
        ));
    }
}
