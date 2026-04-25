// EventKitHelper — Swift bridge for calendar access. See
// docs/implementation.md §5.4 (canonical bridge pattern) and
// docs/swift-bridge-pattern.md for the cross-bridge conventions.

import EventKit
import Foundation

private let store = EKEventStore()

// Mutable container the detached Task writes through. Swift 6 strict
// concurrency rejects capturing a `var Int32` in a `Task.detached`
// closure; the class reference is captured immutably and the field
// is mutated, which is fine because the semaphore enforces
// happens-before with the reading thread.
private final class Int32Box: @unchecked Sendable {
    var value: Int32 = 0
}

// Returns 1 if the user grants full calendar access, 0 if denied or
// the request errors. Synchronous to keep the Rust side ergonomic;
// blocks the caller's thread on a semaphore. The continuation runs on
// a detached task so it cannot deadlock the caller.
@_cdecl("ek_request_access")
public func ek_request_access() -> Int32 {
    let result = Int32Box()
    let sem = DispatchSemaphore(value: 0)
    Task.detached {
        do {
            let granted = try await store.requestFullAccessToEvents()
            result.value = granted ? 1 : 0
        } catch {
            result.value = 0
        }
        sem.signal()
    }
    sem.wait()
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

    let serialized = events.map { event -> [String: Any] in
        [
            "title": event.title as Any,
            "start": event.startDate.timeIntervalSince1970,
            "end":   event.endDate.timeIntervalSince1970,
            "attendees": (event.attendees ?? []).map { p -> [String: Any] in
                ["name": p.name as Any, "email": p.url.absoluteString]
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
