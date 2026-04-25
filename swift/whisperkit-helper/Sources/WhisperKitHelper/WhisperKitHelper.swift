// WhisperKitHelper — Swift bridge scaffold for the §4 WhisperKit
// integration. Mirrors the swift-bridge convention documented at
// docs/swift-bridge-pattern.md (canonical reference: eventkit-helper).
//
// v0 ships the @_cdecl surface only. Each entry point returns a
// stub error code (or empty string) so the Rust side compiles and
// links today, without depending on the WhisperKit Swift package.
// The week-4 implementation drops the real call into each stub
// body; the wire shape on the Rust side does not change.

import Foundation

/// Status codes returned by the bridge. Mirrors
/// `crates/heron-speech/src/whisperkit_bridge.rs::WkStatus` 1-for-1.
private let WK_OK: Int32 = 0
private let WK_NOT_IMPLEMENTED: Int32 = -1
private let WK_MODEL_MISSING: Int32 = -2
private let WK_INTERNAL: Int32 = -3

// Initialize the WhisperKit runtime against `model_dir`. The Rust
// side passes a NUL-terminated UTF-8 directory path. v0 always
// returns `WK_NOT_IMPLEMENTED`.
//
// Real impl (week 4): instantiate `WhisperKit(modelFolder:...)`
// and stash the handle in a global, returning `WK_OK` on success.
@_cdecl("wk_init")
public func wk_init(_ model_dir: UnsafePointer<CChar>?) -> Int32 {
    // Touch the parameter so the unused-arg warning doesn't fire.
    // The real impl reads it.
    if let model_dir = model_dir {
        _ = String(cString: model_dir)
    }
    return WK_NOT_IMPLEMENTED
}

// Transcribe `wav_path` (NUL-terminated UTF-8) and return the
// resulting JSONL turn array as a malloc'd NUL-terminated C string
// in `*out`. v0 writes an empty string and returns `WK_NOT_IMPLEMENTED`.
//
// Real impl (week 4): call `WhisperKit.transcribe(audioPath:)`,
// serialize each `WhisperKit.TranscriptionResult.segments` entry
// into the §5.2 `Turn` JSON shape (one line per turn), copy to a
// malloc'd buffer, write into `*out`, return `WK_OK`.
//
// Memory contract: caller frees `*out` via `wk_free_string`. We
// `malloc` + `memcpy` + explicit NUL terminator so an embedded NUL
// in a turn's text doesn't truncate the returned JSON.
@_cdecl("wk_transcribe")
public func wk_transcribe(
    _ wav_path: UnsafePointer<CChar>?,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    if let wav_path = wav_path {
        _ = String(cString: wav_path)
    }
    if let out = out {
        let empty = "".data(using: .utf8) ?? Data()
        let count = empty.count
        if let buf = malloc(count + 1)?.assumingMemoryBound(to: CChar.self) {
            empty.withUnsafeBytes { bp in
                if let base = bp.baseAddress, count > 0 {
                    memcpy(buf, base, count)
                }
            }
            buf[count] = 0
            out.pointee = buf
        } else {
            out.pointee = nil
        }
    }
    return WK_NOT_IMPLEMENTED
}

// Free a string previously returned via the `out` parameter of
// `wk_transcribe`. Convention: every @_cdecl that hands the caller
// a heap-allocated buffer ships a paired `_free_string`.
@_cdecl("wk_free_string")
public func wk_free_string(_ p: UnsafeMutablePointer<CChar>?) {
    if let p = p { free(p) }
}
