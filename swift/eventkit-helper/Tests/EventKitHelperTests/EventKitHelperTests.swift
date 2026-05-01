// EventKitHelperTests — XCTest coverage for the @_cdecl bridge surface.
//
// The TCC permission prompt for `requestFullAccessToEvents()` is unsafe to
// drive from unattended CI (it either blocks on the user dialog or fails
// closed without a human there to grant). We therefore avoid calling
// `ek_request_access` here and stick to the surfaces that work without
// prompting — symbol loading, the event-window read on a denied/empty
// store (returns 0 events on a CI runner with no calendar access), and
// the malloc/free contract.

import XCTest
@testable import EventKitHelper

final class EventKitHelperTests: XCTestCase {
    // MARK: - Initialization / common failure modes

    /// `ek_free_string(nil)` must be a no-op rather than crash. The Rust
    /// side relies on this when an FFI call returned a NULL out-pointer
    /// and the wrapper tries to free defensively. Doubling the call also
    /// proves it's idempotent. Successful symbol resolution at link time
    /// is implicit — the test wouldn't run otherwise.
    func testFreeStringNilIsNoOp() {
        ek_free_string(nil)
        ek_free_string(nil) // idempotent
    }

    /// Reading an empty time window must not crash even when the process
    /// has no calendar access. macOS returns an empty event list in that
    /// case; we should still see a valid (empty JSON array) buffer that
    /// the caller can free without UB.
    func testReadWindowEmptyDoesNotCrash() {
        var out: UnsafeMutablePointer<CChar>? = nil
        // Same instant for start and end → no events match regardless of
        // whether TCC has granted access.
        let count = ek_read_window_json(0, 0, &out)
        XCTAssertGreaterThanOrEqual(count, 0)
        // The Swift side always writes a malloc'd buffer (possibly the
        // empty JSON array `[]` or a bigger payload if calendars exist).
        // Free it back via the paired symbol — this exercises the
        // malloc/free contract.
        ek_free_string(out)
    }

    // MARK: - Happy-path round-trip

    /// Round-trip a known time window: parse the returned JSON and
    /// confirm it's a valid array. We don't assert non-emptiness — CI
    /// runners have no calendar database — but the JSON shape is the
    /// stable wire contract Rust unmarshals against.
    func testReadWindowReturnsValidJSONArray() throws {
        var out: UnsafeMutablePointer<CChar>? = nil
        // Pick a 24h window in 2026; doesn't matter whether it has events.
        let start: Int64 = 1_767_225_600 // 2026-01-01 00:00:00 UTC
        let end: Int64 = start + 86_400
        let count = ek_read_window_json(start, end, &out)
        XCTAssertGreaterThanOrEqual(count, 0)

        guard let buf = out else {
            // Allowed (malloc failure, OOM); but not on a healthy runner.
            // Surface the diagnostic rather than crash.
            XCTFail("ek_read_window_json returned NULL out-pointer")
            return
        }
        defer { ek_free_string(buf) }

        let s = String(cString: buf)
        // Empty store on CI → JSONSerialization writes "[]".
        let data = Data(s.utf8)
        let parsed = try JSONSerialization.jsonObject(with: data, options: [])
        XCTAssertNotNil(parsed as? [Any], "wire contract: top-level array")
    }
}
