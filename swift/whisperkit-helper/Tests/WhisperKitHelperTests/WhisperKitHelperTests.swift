// WhisperKitHelperTests — XCTest coverage for the @_cdecl bridge.
//
// Constraints:
//
//   - WhisperKit needs a ~1GB CoreML model on disk to do anything real,
//     and downloading it from HuggingFace takes minutes. We can't pay
//     that cost on every PR, so the live round-trip is gated behind the
//     `HERON_WK_LIVE_MODEL_DIR` env var: set it to a folder containing a
//     pre-fetched model and the round-trip test runs; otherwise it
//     skips. CI leaves the var unset; the human-driven path is documented
//     in `docs/archives/manual-test-matrix.md`.
//
//   - Everything else (NULL handling, MODEL_MISSING surface, transcribe
//     before init, the malloc/free contract) runs hermetically on every
//     CI macos-14 runner without a network or model.

import Foundation
import XCTest
@testable import WhisperKitHelper

final class WhisperKitHelperTests: XCTestCase {
    // Status codes (`WK_OK`, `WK_MODEL_MISSING`, `WK_INTERNAL`) come
    // from the bridge source via `@testable import`. That avoids the
    // split-brain risk where a re-declared local copy could mask a
    // future renumbering of the wire codes. Drift against the Rust
    // side is caught separately by the pinned-constant tests in
    // `whisperkit_bridge.rs`.

    // MARK: - Initialization

    /// Sanity: the @_cdecl symbol is reachable and a NULL pointer
    /// surfaces as `WK_INTERNAL` rather than crashing.
    func testInitNullPathIsInternal() {
        XCTAssertEqual(wk_init(nil), WK_INTERNAL)
    }

    /// `wk_init` against a non-existent directory must surface
    /// `WK_MODEL_MISSING` so the orchestrator can prompt the user to
    /// run `wk_fetch_model` or `heron model fetch`.
    func testInitMissingModelDir() {
        let path = "/tmp/heron-test-nonexistent-model-\(UUID().uuidString)"
        let result = path.withCString { wk_init($0) }
        XCTAssertEqual(result, WK_MODEL_MISSING)
    }

    /// `wk_init` against a path that exists but is a regular file (not a
    /// directory) is the same failure class as "not present" — the
    /// fileExists / isDir gate trips before we hand the path to
    /// WhisperKit.
    func testInitFilePathIsModelMissing() throws {
        let tempFile = NSTemporaryDirectory() + "heron-wk-not-a-dir-\(UUID().uuidString)"
        try "not a model".write(toFile: tempFile, atomically: true, encoding: .utf8)
        defer { try? FileManager.default.removeItem(atPath: tempFile) }

        let result = tempFile.withCString { wk_init($0) }
        XCTAssertEqual(result, WK_MODEL_MISSING)
    }

    // MARK: - Common failure modes

    /// `wk_transcribe` without a prior successful `wk_init` must return
    /// `WK_INTERNAL`. Calling transcribe-without-init is a Rust-side
    /// ordering bug; the bridge surfaces it as a clean error rather
    /// than crashing on the absent instance.
    ///
    /// The bridge contract on the error path is "write a malloc'd empty
    /// (or NULL) buffer to *out" so the Rust wrapper can free
    /// unconditionally. We assert that contract explicitly: any non-NULL
    /// buffer must be a valid NUL-terminated C string.
    ///
    /// Order matters: WhisperKit holds a process-wide singleton
    /// (`InstanceBox`). If the live round-trip ran before this test and
    /// populated the singleton, this assertion would fail spuriously.
    /// We `XCTSkip` in that case rather than emit a false negative;
    /// the live test is gated on `HERON_WK_LIVE_MODEL_DIR` so on CI the
    /// singleton stays empty and this test runs as intended.
    func testTranscribeBeforeInitIsInternal() throws {
        box.lock.lock()
        let alreadyInit = box.instance != nil
        box.lock.unlock()
        try XCTSkipIf(
            alreadyInit,
            "singleton is populated (likely from a prior live-model test); skipping"
        )

        var out: UnsafeMutablePointer<CChar>? = nil
        let path = "/tmp/heron-wk-stub.wav"
        let result = path.withCString { wkPath -> Int32 in
            wk_transcribe(wkPath, nil, &out)
        }
        XCTAssertEqual(result, WK_INTERNAL)
        // Out-pointer contract: bridge MUST write a malloc'd empty C
        // string on the error path (writeEmpty() in the bridge source).
        // A non-empty buffer or a stale uninitialised pointer would be
        // a real regression — the Rust wrapper passes the buffer to
        // `String::from_utf8` and a non-empty error-path payload would
        // poison the JSONL parse.
        let buf = try? XCTUnwrap(out, "bridge wrote nil where empty C-string was expected")
        defer { if let b = buf { wk_free_string(b) } }
        if let b = buf {
            XCTAssertEqual(strlen(b), 0, "error-path buffer must be empty")
        }
    }

    /// `wk_transcribe(nil, …)` must return `WK_INTERNAL` and write a
    /// malloc'd empty buffer to `*out`. Defensive contract for the
    /// Rust wrapper.
    func testTranscribeNullPathIsInternal() {
        var out: UnsafeMutablePointer<CChar>? = nil
        let result = wk_transcribe(nil, nil, &out)
        XCTAssertEqual(result, WK_INTERNAL)
        let buf = try? XCTUnwrap(out, "bridge wrote nil where empty C-string was expected")
        defer { if let b = buf { wk_free_string(b) } }
        if let b = buf {
            XCTAssertEqual(strlen(b), 0, "error-path buffer must be empty")
        }
    }

    /// `wk_fetch_model` with a NULL `dest_dir` must surface
    /// `WK_INTERNAL` and not write the out-pointer to a stale value.
    func testFetchModelNullDestIsInternal() {
        var out: UnsafeMutablePointer<CChar>? = nil
        let result = wk_fetch_model(nil, nil, nil, nil, &out)
        XCTAssertEqual(result, WK_INTERNAL)
        XCTAssertNil(out)
    }

    // MARK: - Memory contract

    /// `wk_free_string(nil)` is a no-op (matches the EventKit / ZoomAx
    /// helpers' shared contract).
    func testFreeStringNilIsNoOp() {
        wk_free_string(nil)
    }

    // MARK: - WAV writer (used by the gated round-trip)

    /// The silent-WAV builder is only invoked from the gated live test,
    /// so without this hermetic check a regression in the byte writer
    /// would only surface when someone runs with a real model — months
    /// later. Validate the header against the canonical RIFF/WAV layout
    /// so a future Swift integer-endian-API change doesn't silently
    /// produce malformed audio.
    func testWriteSilentWavHeader() throws {
        let url = try Self.writeSilentWav(
            dir: FileManager.default.temporaryDirectory,
            seconds: 1
        )
        defer { try? FileManager.default.removeItem(at: url) }

        let data = try Data(contentsOf: url)
        // 44-byte canonical PCM-16 header + 1s @ 16 kHz mono * 2 bytes = 32_044.
        XCTAssertEqual(data.count, 44 + 16_000 * 2)
        XCTAssertEqual(data.subdata(in: 0..<4), Data("RIFF".utf8))
        XCTAssertEqual(data.subdata(in: 8..<12), Data("WAVE".utf8))
        XCTAssertEqual(data.subdata(in: 12..<16), Data("fmt ".utf8))
        XCTAssertEqual(data.subdata(in: 36..<40), Data("data".utf8))
        // Sample rate at offset 24, little-endian UInt32. Decode
        // byte-wise rather than via `UnsafeRawBufferPointer.load(as:)`
        // because the latter traps on misaligned access on some
        // platforms — the decoded buffer's alignment isn't guaranteed
        // by `Data.subdata`.
        let rate = UInt32(data[24]) | (UInt32(data[25]) << 8)
            | (UInt32(data[26]) << 16) | (UInt32(data[27]) << 24)
        XCTAssertEqual(rate, 16_000)
        // Bits per sample at offset 34, little-endian UInt16.
        let bps = UInt16(data[34]) | (UInt16(data[35]) << 8)
        XCTAssertEqual(bps, 16)
    }

    // MARK: - Happy-path round-trip (gated)

    /// Live model round-trip. Skipped unless the operator points
    /// `HERON_WK_LIVE_MODEL_DIR` at a folder containing a pre-downloaded
    /// WhisperKit model bundle (see `docs/archives/manual-test-matrix.md`
    /// for the canonical setup). The test creates a 5-second silent WAV
    /// in a temp dir, runs init + transcribe, and verifies that:
    ///
    ///   - `wk_init` returns `WK_OK`
    ///   - `wk_transcribe` returns `WK_OK` and writes a non-NULL buffer
    ///   - the buffer parses as either empty (silence yielded no segments)
    ///     or as one-JSONL-segment-per-line with the documented schema.
    ///
    /// This exercises the full async→sync bridge and the segment-flatten
    /// pass that downstream Rust depends on.
    func testTranscribeRoundTripWithLiveModel() throws {
        guard let modelDir = ProcessInfo.processInfo.environment["HERON_WK_LIVE_MODEL_DIR"],
              !modelDir.isEmpty
        else {
            throw XCTSkip("HERON_WK_LIVE_MODEL_DIR not set; skipping live model round-trip")
        }
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: modelDir, isDirectory: &isDir),
              isDir.boolValue
        else {
            throw XCTSkip("HERON_WK_LIVE_MODEL_DIR='\(modelDir)' is not a directory")
        }

        let initResult = modelDir.withCString { wk_init($0) }
        XCTAssertEqual(initResult, WK_OK, "wk_init failed against live model dir")

        // Synthesize a 5-second silent 16 kHz mono PCM-16 WAV. WhisperKit
        // accepts that shape; the silence keeps the transcription bounded
        // and deterministic (no segments, or a single low-confidence
        // segment depending on the model's silence handling).
        let wavURL = try Self.writeSilentWav(
            dir: FileManager.default.temporaryDirectory,
            seconds: 5
        )
        defer { try? FileManager.default.removeItem(at: wavURL) }

        var out: UnsafeMutablePointer<CChar>? = nil
        let transcribeResult = wavURL.path.withCString { p -> Int32 in
            wk_transcribe(p, nil, &out)
        }
        defer { if let buf = out { wk_free_string(buf) } }
        XCTAssertEqual(transcribeResult, WK_OK, "wk_transcribe failed on silent WAV")

        guard let buf = out else {
            XCTFail("wk_transcribe wrote NULL out-pointer on WK_OK")
            return
        }
        let payload = String(cString: buf)
        // Empty payload = no segments, which is a legal silence outcome.
        // Non-empty: every line must be a JSON object with the documented
        // keys. We don't assert on values — silence may yield phantom
        // segments depending on model variant.
        if !payload.isEmpty {
            for line in payload.split(separator: "\n") {
                let data = Data(line.utf8)
                let obj = try XCTUnwrap(
                    JSONSerialization.jsonObject(with: data) as? [String: Any],
                    "segment line is not a JSON object: \(line)"
                )
                // The Rust side parses each line as `{"start": f64, "end": f64,
                // "text": String}` — assert the types so a Swift-side schema
                // drift (e.g. WhisperKit changes Float → Float16, or a future
                // helper edit emits ints) trips this test instead of failing
                // silently in the Rust unmarshaller. Also verify timing
                // monotonicity: a segment with end < start is a real bug,
                // not a "silence yielded a phantom segment" artefact.
                let start = try XCTUnwrap(obj["start"] as? Double, "start is not a Double")
                let end = try XCTUnwrap(obj["end"] as? Double, "end is not a Double")
                _ = try XCTUnwrap(obj["text"] as? String, "text is not a String")
                XCTAssertGreaterThanOrEqual(end, start, "segment end < start: \(line)")
            }
        }
    }

    // MARK: - WAV synthesis helper

    /// Write a silent 16-bit PCM WAV (mono, 16 kHz) of `seconds` length
    /// to a UUID-named file under `dir`. Returns the full URL.
    ///
    /// Hand-rolled rather than going through AVAudioFile because the
    /// fixture only has to satisfy WhisperKit's audio loader, and the
    /// hand-rolled writer keeps the test self-contained — no dep on
    /// AVFoundation's runtime quirks under macOS 14.
    private static func writeSilentWav(dir: URL, seconds: Int) throws -> URL {
        let sampleRate: UInt32 = 16_000
        let channels: UInt16 = 1
        let bitsPerSample: UInt16 = 16
        let byteRate: UInt32 = sampleRate * UInt32(channels) * UInt32(bitsPerSample / 8)
        let blockAlign: UInt16 = channels * (bitsPerSample / 8)
        let numSamples: UInt32 = sampleRate * UInt32(seconds)
        let dataSize: UInt32 = numSamples * UInt32(blockAlign)
        let chunkSize: UInt32 = 36 + dataSize

        var buf = Data()
        // RIFF header
        buf.append(contentsOf: Array("RIFF".utf8))
        buf.append(uint32LE: chunkSize)
        buf.append(contentsOf: Array("WAVE".utf8))
        // fmt chunk
        buf.append(contentsOf: Array("fmt ".utf8))
        buf.append(uint32LE: 16) // PCM fmt size
        buf.append(uint16LE: 1)  // PCM format tag
        buf.append(uint16LE: channels)
        buf.append(uint32LE: sampleRate)
        buf.append(uint32LE: byteRate)
        buf.append(uint16LE: blockAlign)
        buf.append(uint16LE: bitsPerSample)
        // data chunk
        buf.append(contentsOf: Array("data".utf8))
        buf.append(uint32LE: dataSize)
        // Silent samples: zeroed PCM-16. Append in one shot for speed.
        buf.append(Data(count: Int(dataSize)))

        let url = dir.appendingPathComponent("heron-wk-silent-\(UUID().uuidString).wav")
        try buf.write(to: url)
        return url
    }
}

private extension Data {
    mutating func append(uint32LE v: UInt32) {
        var le = v.littleEndian
        // `Swift.withUnsafeBytes(of:_:)` disambiguates from the
        // `Data.withUnsafeBytes` instance method, which the compiler
        // would otherwise resolve to inside this extension scope.
        Swift.withUnsafeBytes(of: &le) { append(contentsOf: $0) }
    }
    mutating func append(uint16LE v: UInt16) {
        var le = v.littleEndian
        Swift.withUnsafeBytes(of: &le) { append(contentsOf: $0) }
    }
}
