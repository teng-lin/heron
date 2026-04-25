// WhisperKitHelper — Swift bridge for the §4 WhisperKit integration.
// Mirrors the swift-bridge convention in docs/swift-bridge-pattern.md
// (canonical: eventkit-helper). Owned by crates/heron-speech.
//
// The @_cdecl surface — `wk_init`, `wk_transcribe`, `wk_free_string` —
// is the stable wire to Rust (see whisperkit_bridge.rs). Both entry
// points are *synchronous* C functions, so we bridge to WhisperKit's
// async-only API by blocking on a `DispatchSemaphore`. The Rust side
// is expected to call us from `tokio::task::spawn_blocking` so we
// never block the async runtime.

import Foundation
import WhisperKit

/// Status codes returned by the bridge. Mirrors
/// `crates/heron-speech/src/whisperkit_bridge.rs::WkStatus` 1-for-1.
private let WK_OK: Int32 = 0
private let WK_NOT_IMPLEMENTED: Int32 = -1
private let WK_MODEL_MISSING: Int32 = -2
private let WK_INTERNAL: Int32 = -3

// MARK: - Global instance (single-init contract)
//
// Rust calls `wk_init` once per process; subsequent `wk_transcribe`
// calls reuse the warm WhisperKit instance. The lock guards the
// reference, not WhisperKit's internal state — WhisperKit itself
// serializes transcribe calls per `Sources/WhisperKit/WhisperKit.swift`.

private final class InstanceBox: @unchecked Sendable {
    var instance: WhisperKit?
    let lock = NSLock()
}

private let box = InstanceBox()

// MARK: - wk_init

/// Initialize the WhisperKit runtime against `model_dir`.
///
/// Blocks the calling thread (Rust passes us through
/// `spawn_blocking`). Returns `WK_OK` on success, `WK_MODEL_MISSING`
/// if the directory is absent, `WK_INTERNAL` for any other failure.
@_cdecl("wk_init")
public func wk_init(_ model_dir: UnsafePointer<CChar>?) -> Int32 {
    guard let model_dir = model_dir else {
        return WK_INTERNAL
    }
    let path = String(cString: model_dir)

    var isDir: ObjCBool = false
    let exists = FileManager.default.fileExists(atPath: path, isDirectory: &isDir)
    if !exists || !isDir.boolValue {
        return WK_MODEL_MISSING
    }

    // Bridge async → sync via DispatchSemaphore. WhisperKit's README
    // recommends `Task { try await WhisperKit(...) }.value` from
    // synchronous contexts; we wrap that in a semaphore so the C
    // caller actually blocks until the load completes.
    let sem = DispatchSemaphore(value: 0)
    var initErr: Error?
    var instance: WhisperKit?

    Task {
        do {
            instance = try await WhisperKit(modelFolder: path)
        } catch {
            initErr = error
        }
        sem.signal()
    }
    sem.wait()

    if let _ = initErr {
        return WK_INTERNAL
    }
    guard let instance = instance else {
        return WK_INTERNAL
    }

    box.lock.lock()
    box.instance = instance
    box.lock.unlock()
    return WK_OK
}

// MARK: - wk_transcribe

/// Transcribe `wav_path` and return a JSONL turn array as a malloc'd
/// NUL-terminated C string in `*out`.
///
/// Wire shape (matching the Rust side's expectation in
/// whisperkit_bridge.rs): one JSON object per line, separated by `\n`,
/// with `{"start": f64, "end": f64, "text": String}`. The Rust side
/// upgrades each line to a `heron_types::Turn` by filling in channel,
/// speaker, source, and confidence at the call site (those fields are
/// not WhisperKit's to know).
///
/// Memory contract: caller frees `*out` via `wk_free_string`. Always
/// writes a buffer (possibly the empty string) on `WK_OK`. On non-Ok
/// paths, writes either an empty buffer or NULL — Rust handles both.
@_cdecl("wk_transcribe")
public func wk_transcribe(
    _ wav_path: UnsafePointer<CChar>?,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    // Always write *some* value to *out so the Rust side never sees
    // a stale uninitialized pointer; an empty-string buffer is the
    // contract for "no segments".
    func writeEmpty() {
        guard let out = out else { return }
        if let buf = malloc(1)?.assumingMemoryBound(to: CChar.self) {
            buf[0] = 0
            out.pointee = buf
        } else {
            out.pointee = nil
        }
    }

    guard let wav_path = wav_path else {
        writeEmpty()
        return WK_INTERNAL
    }
    let path = String(cString: wav_path)

    box.lock.lock()
    let instance = box.instance
    box.lock.unlock()

    guard let instance = instance else {
        writeEmpty()
        return WK_INTERNAL
    }

    // Sync wrapper around WhisperKit's async transcribe call.
    let sem = DispatchSemaphore(value: 0)
    var results: [TranscriptionResult] = []
    var transcribeErr: Error?

    Task {
        do {
            results = try await instance.transcribe(audioPath: path)
        } catch {
            transcribeErr = error
        }
        sem.signal()
    }
    sem.wait()

    if transcribeErr != nil {
        writeEmpty()
        return WK_INTERNAL
    }

    // Flatten all segments across all TranscriptionResult entries.
    // WhisperKit returns an array because longer-than-window audio
    // is decoded as multiple result chunks; the segments collectively
    // cover the input timeline.
    var lines: [String] = []
    for result in results {
        for segment in result.segments {
            // {"start": f64, "end": f64, "text": String} per line.
            // We hand-build the JSON instead of going through Codable
            // so we're robust to embedded NUL/quotes in `text` and
            // explicit about the f64 cast (segment.start is Float).
            let textJson = jsonEscape(segment.text)
            let line = "{\"start\":\(Double(segment.start)),\"end\":\(Double(segment.end)),\"text\":\(textJson)}"
            lines.append(line)
        }
    }
    let body = lines.joined(separator: "\n")

    if let out = out {
        let bytes = Array(body.utf8)
        let count = bytes.count
        if let buf = malloc(count + 1)?.assumingMemoryBound(to: CChar.self) {
            bytes.withUnsafeBufferPointer { bp in
                if let base = bp.baseAddress, count > 0 {
                    memcpy(buf, base, count)
                }
            }
            buf[count] = 0
            out.pointee = buf
        } else {
            out.pointee = nil
            return WK_INTERNAL
        }
    }
    return WK_OK
}

// MARK: - wk_free_string

/// Free a string previously returned via the `out` parameter of
/// `wk_transcribe`. Convention: every @_cdecl that hands the caller
/// a heap-allocated buffer ships a paired `_free_string`.
@_cdecl("wk_free_string")
public func wk_free_string(_ p: UnsafeMutablePointer<CChar>?) {
    if let p = p { free(p) }
}

// MARK: - JSON helpers

/// Minimal JSON string encoder for the segment text. We avoid pulling
/// `JSONEncoder` for a single field because (a) we need the embedded
/// quotes in the output literal anyway, and (b) `JSONEncoder` requires
/// an `Encodable` wrapper for a bare string.
private func jsonEscape(_ s: String) -> String {
    var out = "\""
    out.reserveCapacity(s.count + 2)
    for scalar in s.unicodeScalars {
        switch scalar {
        case "\"": out += "\\\""
        case "\\": out += "\\\\"
        case "\n": out += "\\n"
        case "\r": out += "\\r"
        case "\t": out += "\\t"
        case "\u{0008}": out += "\\b"
        case "\u{000C}": out += "\\f"
        default:
            if scalar.value < 0x20 {
                out += String(format: "\\u%04x", scalar.value)
            } else {
                out += String(scalar)
            }
        }
    }
    out += "\""
    return out
}
