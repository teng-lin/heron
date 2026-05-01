// WhisperKitHelper — Swift bridge for the §4 WhisperKit integration.
// Mirrors the swift-bridge convention in docs/archives/swift-bridge-pattern.md
// (canonical: eventkit-helper). Owned by crates/heron-speech.
//
// The @_cdecl surface — `wk_init`, `wk_transcribe`, `wk_free_string` —
// is the stable wire to Rust (see whisperkit_bridge.rs). Both entry
// points are *synchronous* C functions, so we bridge to WhisperKit's
// async-only API by blocking on a `DispatchSemaphore` with a per-call
// deadline (`WK_INIT_TIMEOUT` / `WK_FETCH_TIMEOUT` /
// `WK_TRANSCRIBE_TIMEOUT`). On deadline expiry we return `WK_TIMEOUT`
// rather than wedge the calling thread. The Rust side is expected to
// call us from `tokio::task::spawn_blocking` so we never block the
// async runtime even when waiting near the upper bound.

import Foundation
import WhisperKit

/// Status codes returned by the bridge. Mirrors
/// `crates/heron-speech/src/whisperkit_bridge.rs::WkStatus` 1-for-1.
///
/// `internal` (not `private`) so the XCTest target can `@testable
/// import WhisperKitHelper` and assert returned codes by name. Drift
/// against the Rust side is caught by the pinned-constant tests in
/// `whisperkit_bridge.rs`, mirroring the canonical bridge pattern in
/// `docs/archives/swift-bridge-pattern.md`.
internal let WK_OK: Int32 = 0
internal let WK_NOT_IMPLEMENTED: Int32 = -1
internal let WK_MODEL_MISSING: Int32 = -2
internal let WK_INTERNAL: Int32 = -3
internal let WK_TIMEOUT: Int32 = -4

/// Default WhisperKit variant downloaded by `wk_fetch_model` when the
/// caller passes a NULL `variant`. Mirrors the Rust-side default in
/// `whisperkit_bridge.rs::DEFAULT_WK_VARIANT`. ~1GB CoreML bundle per
/// `docs/archives/plan.md` week-9 onboarding step 5.
private let WK_DEFAULT_VARIANT = "openai_whisper-small.en"

/// Per-call deadlines for the async→sync semaphore bridge.
///
/// WhisperKit is async-only; we block the C caller on a
/// `DispatchSemaphore` until the Task finishes. Without a deadline a
/// hung model load (CoreML JIT regression, network stall, etc.) would
/// pin the Rust `spawn_blocking` worker forever. These bounds turn
/// "block forever" into a recoverable `WK_TIMEOUT` so the orchestrator
/// can surface a clear error instead of wedging.
///
/// Values are intentionally generous — the deadline is a watchdog,
/// not a performance budget. Each call site picks its own bound based
/// on how slow the slow-but-still-healthy case can legitimately be:
///
///   - `WK_INIT_TIMEOUT` (2m): first-run model load is mostly CoreML
///     graph compile; ~30s on Apple Silicon, longer on Intel. 2m
///     covers the slow-Intel + cold-disk case with headroom.
///   - `WK_FETCH_TIMEOUT` (30m): ~1GB CoreML bundle on a slow link
///     (~500 KB/s) takes ~33m, which is a real corporate-network
///     edge case but not the median; 30m is the watchdog upper bound.
///     A timed-out fetch may leave partial bytes under
///     `<dest_dir>/...`; WhisperKit's HubApi resumes on retry.
///   - `WK_TRANSCRIBE_TIMEOUT` (30m): a single archived session can
///     be 30+ minutes of audio, and on a slow Intel Mac the realtime
///     factor sits well below 1×. We pick 30m to match
///     "longest reasonable session × slowest expected RTF" without
///     becoming a budget. Per-chunk streaming would let us tighten
///     this; v1 transcribes the whole WAV in one call.
private let WK_INIT_TIMEOUT: DispatchTimeInterval = .seconds(120)
private let WK_FETCH_TIMEOUT: DispatchTimeInterval = .seconds(30 * 60)
private let WK_TRANSCRIBE_TIMEOUT: DispatchTimeInterval = .seconds(30 * 60)

/// Run an async `body` from a synchronous C entry point with a
/// deadline. Returns `true` if the work finished before the deadline,
/// `false` on timeout. On timeout the spawned Task is **not**
/// cancelled — Swift `Task` cancellation is cooperative and we have no
/// way to abort an in-flight CoreML model load. The Task may still
/// complete in the background and write to its captured variables;
/// callers must therefore not read those variables after a timeout
/// (and the captures themselves must be either `Sendable` or
/// internally-locked, since the Task and the caller can otherwise
/// touch them on different threads).
///
/// This helper exists so each `@_cdecl` body has a single, audited
/// shape for the async→sync bridge instead of three subtly different
/// open-coded copies.
private func runWithTimeout(
    _ timeout: DispatchTimeInterval,
    _ body: @escaping () async -> Void
) -> Bool {
    let sem = DispatchSemaphore(value: 0)
    // The bare `Task { ... }` form here is the same shape the v0
    // pre-timeout bridge used; it inherits the calling actor context
    // (none, for these `@_cdecl` functions). The body is permitted to
    // capture non-Sendable C-ABI pointers (e.g. `progress_userdata`)
    // because the C ABI is single-threaded — Rust serializes calls
    // through `spawn_blocking` per `whisperkit_bridge.rs`.
    Task {
        await body()
        sem.signal()
    }
    return sem.wait(timeout: .now() + timeout) == .success
}

/// Lock-guarded handoff slot for the async→sync outcome of the three
/// `@_cdecl` bodies. The Task writes through `set` on completion; the
/// C caller reads through `take` after the semaphore signals (success
/// path) or never reads at all (timeout path).
///
/// The explicit lock makes the Sendable contract auditable: every
/// access goes through `NSLock`, so the Task and the caller cannot
/// race on the field even on the timeout-then-late-write path. We use
/// `final class` (rather than a struct) because the value is shared
/// by reference between the Task closure and the synchronous caller —
/// a struct would copy and the Task's writes would never be visible.
private final class OutcomeSlot<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T?

    func set(_ v: T) {
        lock.lock()
        value = v
        lock.unlock()
    }

    func take() -> T? {
        lock.lock()
        defer { lock.unlock() }
        let v = value
        value = nil
        return v
    }
}

// MARK: - Global instance (single-init contract)
//
// Rust calls `wk_init` once per process; subsequent `wk_transcribe`
// calls reuse the warm WhisperKit instance. The lock guards the
// reference, not WhisperKit's internal state — WhisperKit itself
// serializes transcribe calls per `Sources/WhisperKit/WhisperKit.swift`.

internal final class InstanceBox: @unchecked Sendable {
    var instance: WhisperKit?
    let lock = NSLock()
}

internal let box = InstanceBox()

// MARK: - wk_init

/// Initialize the WhisperKit runtime against `model_dir`.
///
/// Blocks the calling thread (Rust passes us through
/// `spawn_blocking`). Returns `WK_OK` on success, `WK_MODEL_MISSING`
/// if the directory is absent, `WK_TIMEOUT` if the load doesn't
/// finish within `WK_INIT_TIMEOUT`, `WK_INTERNAL` for any other
/// failure.
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
    // caller actually blocks until the load completes — bounded by
    // `WK_INIT_TIMEOUT` so a hung load surfaces as a recoverable
    // error instead of wedging the spawn_blocking worker forever.
    let outcome = OutcomeSlot<Result<WhisperKit, Error>>()

    let finished = runWithTimeout(WK_INIT_TIMEOUT) {
        do {
            let inst = try await WhisperKit(modelFolder: path)
            outcome.set(.success(inst))
        } catch {
            outcome.set(.failure(error))
        }
    }
    if !finished {
        // The Task is still running and may eventually call
        // `outcome.set` — that's safe because the slot serializes
        // through its own lock. We deliberately don't `take` here:
        // any value the Task produces is dropped inside the slot
        // when the Task's closure goes out of scope.
        return WK_TIMEOUT
    }

    let instance: WhisperKit
    switch outcome.take() {
    case .some(.success(let inst)):
        instance = inst
    case .some(.failure):
        return WK_INTERNAL
    case .none:
        // Task signaled but didn't store an outcome — a programmer
        // error in this file, not a runtime path. Surface as
        // Internal rather than crash so the caller still gets a
        // clean error code.
        return WK_INTERNAL
    }

    box.lock.lock()
    box.instance = instance
    box.lock.unlock()
    return WK_OK
}

// MARK: - wk_fetch_model

/// Download a WhisperKit `variant` into `dest_dir` and report the
/// resolved model folder via `*out_model_dir`.
///
/// `dest_dir` is used as the HubApi `downloadBase`; WhisperKit itself
/// writes under `<dest_dir>/models/argmaxinc/whisperkit-coreml/<variant>`
/// (see swift-transformers HubApi.localRepoLocation). The caller wants
/// to know the *resolved* folder so it can pass that to `wk_init`, so
/// we hand it back as a malloc'd C string the caller frees via
/// `wk_free_string`. The same memory contract as `wk_transcribe`.
///
/// `variant` may be NULL → uses `WK_DEFAULT_VARIANT`. `progress_cb`
/// may be NULL → no progress reporting; otherwise it's invoked from
/// the Swift Task with values in `[0.0, 1.0]`. The userdata pointer
/// is forwarded verbatim so the Rust side can downcast back to a
/// `Box<dyn FnMut(f32)>` thunk.
///
/// Returns `WK_OK`, `WK_MODEL_MISSING` for an unknown variant (the
/// HubApi search returns zero matches), `WK_TIMEOUT` if the download
/// doesn't finish within `WK_FETCH_TIMEOUT`, or `WK_INTERNAL` for any
/// network / write failure.
@_cdecl("wk_fetch_model")
public func wk_fetch_model(
    _ variant: UnsafePointer<CChar>?,
    _ dest_dir: UnsafePointer<CChar>?,
    _ progress_cb: (@convention(c) (UnsafeMutableRawPointer?, Float) -> Void)?,
    _ progress_userdata: UnsafeMutableRawPointer?,
    _ out_model_dir: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    func writeOut(_ s: String?) {
        guard let out = out_model_dir else { return }
        guard let s = s else {
            out.pointee = nil
            return
        }
        let bytes = Array(s.utf8)
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
        }
    }

    guard let dest_dir = dest_dir else {
        writeOut(nil)
        return WK_INTERNAL
    }
    let destPath = String(cString: dest_dir)
    let variantStr: String = variant.map { String(cString: $0) } ?? WK_DEFAULT_VARIANT

    let destURL = URL(fileURLWithPath: destPath, isDirectory: true)
    do {
        try FileManager.default.createDirectory(at: destURL, withIntermediateDirectories: true)
    } catch {
        writeOut(nil)
        return WK_INTERNAL
    }

    // Bridge async → sync via DispatchSemaphore, mirroring `wk_init`.
    // Rust passes us through `spawn_blocking`, so blocking here is
    // expected. Bounded by `WK_FETCH_TIMEOUT` (~30m) — generous
    // because a ~1GB CoreML bundle on a slow link is legitimately
    // long, but finite so a stalled download eventually surfaces.
    let outcome = OutcomeSlot<Result<URL, Error>>()

    let finished = runWithTimeout(WK_FETCH_TIMEOUT) {
        do {
            let url = try await WhisperKit.download(
                variant: variantStr,
                downloadBase: destURL,
                progressCallback: { progress in
                    if let cb = progress_cb {
                        // Foundation's Progress reports 0.0…1.0 in
                        // `fractionCompleted`. Cast to Float because
                        // the C ABI we publish is Float-only — the
                        // extra precision wouldn't survive the wire.
                        cb(progress_userdata, Float(progress.fractionCompleted))
                    }
                }
            )
            outcome.set(.success(url))
        } catch {
            outcome.set(.failure(error))
        }
    }
    if !finished {
        // The Rust caller leaks the userdata box on `WK_TIMEOUT` so
        // late `progress_cb` invocations from the lingering Task
        // remain valid; see `whisperkit_fetch` in the Rust bridge.
        writeOut(nil)
        return WK_TIMEOUT
    }

    switch outcome.take() {
    case .some(.success(let resolvedFolder)):
        writeOut(resolvedFolder.path)
        return WK_OK
    case .some(.failure(let err)):
        // WhisperKit raises `WhisperError.modelsUnavailable` when the
        // variant search returns zero matches; surface that as
        // ModelMissing so the orchestrator distinguishes "bad variant"
        // from "network died mid-download".
        writeOut(nil)
        if case WhisperError.modelsUnavailable = err {
            return WK_MODEL_MISSING
        }
        return WK_INTERNAL
    case .none:
        writeOut(nil)
        return WK_INTERNAL
    }
}

// MARK: - wk_transcribe

/// Transcribe `wav_path` and return a JSONL turn array as a malloc'd
/// NUL-terminated C string in `*out`.
///
/// `prompt` is an optional UTF-8 NUL-terminated C string carrying
/// vocabulary-boost hotwords (Tier 4 #17). When non-NULL and non-empty,
/// the bridge tokenizes it via WhisperKit's tokenizer and forwards the
/// resulting token IDs as `DecodingOptions.promptTokens`. Mirrors the
/// `WhisperKitCLI/TranscribeCLI.swift` `--prompt` flag handling exactly,
/// down to the leading-space prepend the Whisper tokenizer expects so
/// hotwords don't accidentally fuse with the decoder's first emitted
/// token. NULL or empty → no prompt is set (preserves the pre-Tier-4
/// decode behavior identically).
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
    _ prompt: UnsafePointer<CChar>?,
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

    // Optional prompt → tokens. NULL pointer or zero-length string both
    // mean "no prompt"; we only build `DecodingOptions` when there's a
    // tokenizer AND something to encode. The leading space + special-
    // token filter mirrors `WhisperKitCLI/TranscribeCLI.swift` exactly
    // (lines 133-138 in upstream); diverging here would silently change
    // decode behavior compared to the reference CLI.
    let promptText: String? = prompt.flatMap { p -> String? in
        let s = String(cString: p)
        return s.isEmpty ? nil : s
    }

    box.lock.lock()
    let instance = box.instance
    box.lock.unlock()

    guard let instance = instance else {
        writeEmpty()
        return WK_INTERNAL
    }

    // Compose `DecodingOptions` only when a non-empty prompt was supplied.
    // Without a prompt we pass `nil` so WhisperKit picks its own defaults
    // (matching the pre-Tier-4 path byte-for-byte). With a prompt, we
    // tokenize via the loaded tokenizer; if the tokenizer isn't ready we
    // surface `WK_INTERNAL` rather than silently dropping the prompt —
    // that turns "hotwords requested but unwirable" into a loud failure
    // the operator can investigate.
    var decodeOptions: DecodingOptions? = nil
    if let promptText = promptText {
        guard let tokenizer = instance.tokenizer else {
            writeEmpty()
            return WK_INTERNAL
        }
        let trimmed = promptText.trimmingCharacters(in: .whitespaces)
        let tokens = tokenizer
            .encode(text: " " + trimmed)
            .filter { $0 < tokenizer.specialTokens.specialTokenBegin }
        var options = DecodingOptions()
        options.promptTokens = tokens
        options.usePrefillPrompt = true
        decodeOptions = options
    }

    // Sync wrapper around WhisperKit's async transcribe call. The
    // call is bounded by `WK_TRANSCRIBE_TIMEOUT` so a wedged decode
    // (CoreML driver hang, runaway loop) surfaces as a recoverable
    // error rather than blocking the spawn_blocking worker forever.
    let outcome = OutcomeSlot<Result<[TranscriptionResult], Error>>()

    let finished = runWithTimeout(WK_TRANSCRIBE_TIMEOUT) {
        do {
            let r: [TranscriptionResult]
            if let opts = decodeOptions {
                r = try await instance.transcribe(
                    audioPath: path,
                    decodeOptions: opts
                )
            } else {
                r = try await instance.transcribe(audioPath: path)
            }
            outcome.set(.success(r))
        } catch {
            outcome.set(.failure(error))
        }
    }
    if !finished {
        writeEmpty()
        return WK_TIMEOUT
    }

    let results: [TranscriptionResult]
    switch outcome.take() {
    case .some(.success(let r)):
        results = r
    case .some(.failure):
        writeEmpty()
        return WK_INTERNAL
    case .none:
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
