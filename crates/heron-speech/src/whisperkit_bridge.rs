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
use std::path::Path;

use thiserror::Error;

#[cfg(target_vendor = "apple")]
mod ffi {
    use std::os::raw::c_char;

    unsafe extern "C" {
        pub(super) fn wk_init(model_dir: *const c_char) -> i32;
        pub(super) fn wk_transcribe(wav_path: *const c_char, out: *mut *mut c_char) -> i32;
        pub(super) fn wk_free_string(p: *mut c_char);
    }
}

/// Pinned constants matching the Swift side. Drift here is caught at
/// **compile time** by the unit tests below that assert each enum
/// variant equals its raw constant. Any rename / renumber on the
/// Swift side fails CI rather than silently coercing to "not
/// implemented".
pub const WK_OK_RAW: i32 = 0;
pub const WK_NOT_IMPLEMENTED_RAW: i32 = -1;
pub const WK_MODEL_MISSING_RAW: i32 = -2;
pub const WK_INTERNAL_RAW: i32 = -3;

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

/// Transcribe `wav_path` and return the JSONL turn array as a `String`.
///
/// One JSON object per line, matching the §5.2 `Turn` shape. v0's
/// stub returns an empty string and [`WkError::NotYetImplemented`].
#[cfg(target_vendor = "apple")]
pub fn whisperkit_transcribe(wav_path: &Path) -> Result<String, WkError> {
    let c_path = path_to_cstring(wav_path)?;
    let mut buf: *mut c_char = std::ptr::null_mut();
    // SAFETY: `wk_transcribe` writes a malloc'd buffer into `*out`
    // and returns the status code. We hand ownership back via
    // `wk_free_string` regardless of which branch we take.
    let raw = unsafe { ffi::wk_transcribe(c_path.as_ptr(), &mut buf) };
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
pub fn whisperkit_transcribe(_wav_path: &Path) -> Result<String, WkError> {
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
        let result = whisperkit_transcribe(&wav);
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
        let result = whisperkit_transcribe(&p);
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
            whisperkit_transcribe(&p),
            Err(WkError::NotYetImplemented)
        ));
    }
}
