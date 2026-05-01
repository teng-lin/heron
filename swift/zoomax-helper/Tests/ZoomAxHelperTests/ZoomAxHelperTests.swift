// ZoomAxHelperTests — XCTest coverage for the Zoom AX bridge.
//
// We can't drive the live polling thread without Zoom installed and
// Accessibility permission granted (CI has neither), so the focus here
// is on:
//
//   1. The `parseTileDescription` regex — the single most drift-prone
//      surface, since it depends on Zoom's English locale wording. A
//      Zoom UI change silently degrades attribution without throwing;
//      a unit test on the canonical descriptions catches that.
//   2. The @_cdecl error paths that don't require a running Zoom — the
//      `target app not running` and `release with nothing registered`
//      branches.
//   3. The malloc/free contract on `ax_free_string`.

import XCTest
@testable import ZoomAxHelper

final class ZoomAxHelperTests: XCTestCase {
    // MARK: - Description parser (the drift-prone surface)

    /// Verbatim description from `fixtures/zoom/spike-triple/muted.json`
    /// at depth 2 (the participant tile). The fixture is the source of
    /// truth for the bridge regex; this test fails if a Zoom UI bump
    /// changes the wording. Refresh the fixture and update the strings
    /// here together — a fixture-only update would leave the regex
    /// drifting silently.
    func testParseTileDescriptionMutedWithVideoOff() throws {
        let parsed = parseTileDescription("Teng Lin, Computer audio muted, Video off")
        let unwrapped = try XCTUnwrap(parsed)
        XCTAssertEqual(unwrapped.name, "Teng Lin")
        XCTAssertEqual(unwrapped.state, ParticipantState(muted: true, videoOff: true))
    }

    /// Verbatim description from `fixtures/zoom/spike-triple/speaking.json`
    /// (the unmuted participant). Locks in the regex match against the
    /// audio-only-tile shape that Zoom emits when the camera is off.
    func testParseTileDescriptionUnmuted() throws {
        let parsed = parseTileDescription("Blackmyth, Computer audio unmuted")
        let unwrapped = try XCTUnwrap(parsed)
        XCTAssertEqual(unwrapped.name, "Blackmyth")
        XCTAssertEqual(unwrapped.state, ParticipantState(muted: false, videoOff: nil))
    }

    /// `, Video on` suffix variant. The fixtures don't capture this
    /// shape because the spike was recorded with cameras off, but the
    /// regex pattern explicitly handles it. Synthetic to exercise the
    /// other branch of the optional `(?:, Video (off|on))?` group.
    func testParseTileDescriptionUnmutedWithVideoOn() throws {
        let parsed = parseTileDescription("Bob Jones, Computer audio unmuted, Video on")
        let unwrapped = try XCTUnwrap(parsed)
        XCTAssertEqual(unwrapped.name, "Bob Jones")
        XCTAssertEqual(unwrapped.state, ParticipantState(muted: false, videoOff: false))
    }

    /// Audio-only layout: no `, Video …` suffix. Per the bridge comment,
    /// some Zoom layouts elide the video clause. The "muted" half of
    /// this case appears in the fixtures (`Blackmyth, Computer audio
    /// muted`); the "unmuted" half is synthetic so we cover both
    /// values of the mute capture group on the audio-only branch.
    func testParseTileDescriptionAudioOnlyUnmuted() throws {
        let parsed = parseTileDescription("Carol Lee, Computer audio unmuted")
        let unwrapped = try XCTUnwrap(parsed)
        XCTAssertEqual(unwrapped.name, "Carol Lee")
        XCTAssertEqual(unwrapped.state, ParticipantState(muted: false, videoOff: nil))
    }

    /// Names with commas embedded ("Last, First" display name) must
    /// still parse to a recognisable participant. The non-greedy
    /// `(.+?)` expands to the last viable boundary before
    /// `, Computer audio …`, so the comma-bearing name survives intact.
    /// Locking this in defends against a future regex tightening that
    /// would silently drop these participants.
    func testParseTileDescriptionNameWithComma() throws {
        let parsed = parseTileDescription("Doe, John, Computer audio muted")
        let unwrapped = try XCTUnwrap(parsed)
        XCTAssertEqual(unwrapped.name, "Doe, John")
        XCTAssertTrue(unwrapped.state.muted)
        XCTAssertNil(unwrapped.state.videoOff)
    }

    /// Garbled / transient descriptions must return nil rather than
    /// silently emitting a false-positive event. This is the regression
    /// guard for "Connecting…", localized strings, and partial captures.
    func testParseTileDescriptionRejectsGarbledInput() {
        XCTAssertNil(parseTileDescription(""))
        XCTAssertNil(parseTileDescription("Connecting..."))
        XCTAssertNil(parseTileDescription("Alice, Computeraudio aus")) // German
        XCTAssertNil(parseTileDescription("Alice, Computer audio loud")) // wrong word
        XCTAssertNil(parseTileDescription(", Computer audio muted")) // empty name
    }

    // MARK: - Common failure modes

    /// `ax_register_observer` against a bundle ID that isn't running
    /// must surface `AX_PROCESS_NOT_RUNNING`. We use a deliberately
    /// invalid bundle so the test is hermetic.
    func testRegisterObserverMissingProcess() {
        let result = "com.heron.test.nonexistent".withCString { cstr in
            ax_register_observer(cstr)
        }
        XCTAssertEqual(result, AX_PROCESS_NOT_RUNNING)
    }

    /// Same for `ax_dump_tree`.
    func testDumpTreeMissingProcess() {
        var out: UnsafeMutablePointer<CChar>? = nil
        let result = "com.heron.test.nonexistent".withCString { cstr in
            ax_dump_tree(cstr, 16, &out)
        }
        XCTAssertEqual(result, AX_PROCESS_NOT_RUNNING)
        // Out should not have been written on the early-return path.
        XCTAssertNil(out)
    }

    /// `ax_register_observer(nil)` must surface `AX_INTERNAL` rather
    /// than crash on the NULL deref. Defensive contract for the Rust
    /// shim that always passes a CString.
    func testRegisterObserverNilArgsAreInternal() {
        XCTAssertEqual(ax_register_observer(nil), AX_INTERNAL)
    }

    /// `ax_release_observer` must be idempotent: calling it with no
    /// observer registered returns `AX_OK`, mirroring the Rust shim's
    /// drop-on-drop expectation.
    func testReleaseObserverIdempotent() {
        XCTAssertEqual(ax_release_observer(), AX_OK)
        XCTAssertEqual(ax_release_observer(), AX_OK)
    }

    /// `ax_poll(nil)` must return AX_INTERNAL on a NULL out-pointer
    /// rather than dereferencing it.
    func testPollNilOutIsInternal() {
        XCTAssertEqual(ax_poll(nil), AX_INTERNAL)
    }

    /// `ax_free_string(nil)` is a no-op (matches the EventKit/WhisperKit
    /// helpers' contract).
    func testFreeStringNilIsNoOp() {
        ax_free_string(nil)
    }
}
