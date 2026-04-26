// EventKitHelper — Swift bridge for calendar access. See
// docs/archives/implementation.md §5.4 (canonical bridge pattern) and
// docs/archives/swift-bridge-pattern.md for the cross-bridge conventions.

import EventKit
import Foundation

private let store = EKEventStore()

/// Status codes returned by `ek_request_access`. Mirrors
/// `crates/heron-vault/src/calendar.rs::EkAccessStatus` 1-for-1; the
/// numbering follows the WhisperKit bridge convention so a future
/// shared constants module can fold both bridges.
private let EK_ACCESS_GRANTED: Int32 = 1
private let EK_ACCESS_DENIED: Int32 = 0
private let EK_TIMEOUT: Int32 = -4

/// Per-call deadline for the async→sync semaphore bridge in
/// `ek_request_access`. The TCC permission prompt is user-driven, so
/// the bound is generous — long enough for someone to read it and
/// click through, short enough that a wedged TCC daemon eventually
/// surfaces as a recoverable `EK_TIMEOUT` instead of pinning the Rust
/// `spawn_blocking` worker forever. Matches the watchdog rationale in
/// `swift/whisperkit-helper/.../WhisperKitHelper.swift`.
private let EK_REQUEST_TIMEOUT: DispatchTimeInterval = .seconds(60)

// Mutable container the detached Task writes through. Swift 6 strict
// concurrency rejects capturing a `var Int32` in a `Task.detached`
// closure; the class reference is captured immutably and the field
// is mutated, which is fine because the semaphore enforces
// happens-before with the reading thread.
private final class Int32Box: @unchecked Sendable {
    var value: Int32 = 0
}

/// Run an async `body` from a synchronous C entry point with a
/// deadline. Returns `true` if the work finished before the deadline,
/// `false` on timeout. On timeout the spawned Task is **not**
/// cancelled — Swift `Task` cancellation is cooperative and EventKit's
/// permission API has no abort hook. The Task may still complete in
/// the background and write to its captured variables; callers must
/// therefore not read those variables after a timeout.
///
/// Mirrors the `runWithTimeout` helper in
/// `swift/whisperkit-helper/.../WhisperKitHelper.swift` so the helper
/// crates share one audited shape for the async→sync bridge.
private func runWithTimeout(
    _ timeout: DispatchTimeInterval,
    _ body: @escaping () async -> Void
) -> Bool {
    let sem = DispatchSemaphore(value: 0)
    Task.detached {
        await body()
        sem.signal()
    }
    return sem.wait(timeout: .now() + timeout) == .success
}

@_cdecl("ek_request_access")
public func ek_request_access() -> Int32 {
    let result = Int32Box()
    let finished = runWithTimeout(EK_REQUEST_TIMEOUT) {
        do {
            let granted = try await store.requestFullAccessToEvents()
            result.value = granted ? EK_ACCESS_GRANTED : EK_ACCESS_DENIED
        } catch {
            result.value = EK_ACCESS_DENIED
        }
    }
    if !finished {
        return EK_TIMEOUT
    }
    return result.value
}

// Reads calendar events between [start_unix, end_unix] (Unix
// timestamps in seconds), serializes them as a JSON array, and writes
// the malloc'd C string into `*out`. Returns the number of events.
//
// Caller must release the buffer with `ek_free_string` to give
// ownership back to the Swift side. Convention: every `@_cdecl` that
// returns a heap-allocated string ships a paired `_free_string`.
@_cdecl("ek_read_window_json")
public func ek_read_window_json(
    _ start_unix: Int64,
    _ end_unix: Int64,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
) -> Int32 {
    let s = Date(timeIntervalSince1970: TimeInterval(start_unix))
    let e = Date(timeIntervalSince1970: TimeInterval(end_unix))
    let predicate = store.predicateForEvents(withStart: s, end: e, calendars: nil)
    let events = store.events(matching: predicate)

    // JSONSerialization requires every value to be a JSON-bridgeable
    // type — `Optional.none` does NOT bridge to NSNull and would make
    // the whole call return nil. Default any nil string field to "".
    let serialized = events.map { event -> [String: Any] in
        [
            "title": event.title ?? "",
            "start": event.startDate.timeIntervalSince1970,
            "end":   event.endDate.timeIntervalSince1970,
            "attendees": (event.attendees ?? []).map { p -> [String: Any] in
                ["name": p.name ?? "", "email": p.url.absoluteString]
            },
        ]
    }
    let json = (try? JSONSerialization.data(withJSONObject: serialized)) ?? Data()
    // Use malloc + memcpy + explicit NUL terminator rather than
    // strndup: strndup stops at the first 0 byte, so a calendar
    // entry containing a NUL (rare but legal in EKEvent titles)
    // would silently truncate the JSON we hand back to Rust. The
    // Rust side frees this with ek_free_string.
    let count = json.count
    if let buf = malloc(count + 1)?.assumingMemoryBound(to: CChar.self) {
        json.withUnsafeBytes { bp in
            if let base = bp.baseAddress, count > 0 {
                memcpy(buf, base, count)
            }
        }
        buf[count] = 0
        out.pointee = buf
    } else {
        out.pointee = nil
    }
    return Int32(events.count)
}

@_cdecl("ek_free_string")
public func ek_free_string(_ p: UnsafeMutablePointer<CChar>?) {
    if let p = p { free(p) }
}
