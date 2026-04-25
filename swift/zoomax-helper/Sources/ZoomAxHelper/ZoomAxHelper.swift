// ZoomAxHelper — Swift bridge scaffold for the §9 AXObserver
// integration. Same pattern as eventkit-helper / whisperkit-helper
// per docs/swift-bridge-pattern.md.
//
// v0 ships the @_cdecl surface only. Each entry point returns a
// stub status; the real impl in week 6 / §9 hooks into Apple's
// AXObserver API to fire on Zoom's speaker-indicator changes and
// emits one JSONL line per change via `out`.

import Foundation
#if canImport(ApplicationServices)
import ApplicationServices
#endif

/// Status codes returned by the bridge. Mirrors
/// `crates/heron-zoom/src/ax_bridge.rs::AxStatus` 1-for-1.
private let AX_OK: Int32 = 0
private let AX_NOT_IMPLEMENTED: Int32 = -1
private let AX_PROCESS_NOT_RUNNING: Int32 = -2
private let AX_NO_PERMISSION: Int32 = -3
private let AX_INTERNAL: Int32 = -4

// Register an AXObserver against the running process whose
// `bundle_id` matches the (NUL-terminated UTF-8) argument and whose
// front window contains a speaker-indicator element. Returns 0 on
// success and stashes a global handle the caller releases via
// `ax_release_observer`. v0 returns AX_NOT_IMPLEMENTED.
//
// Real impl (week 6): NSRunningApplication.runningApplications →
// pid → AXUIElementCreateApplication → walk the AX tree for the
// (role, subrole, identifier) triple recorded in the §3.3 spike →
// AXObserverCreate + AXObserverAddNotification on speaker-indicator
// state changes.
@_cdecl("ax_register_observer")
public func ax_register_observer(_ bundle_id: UnsafePointer<CChar>?) -> Int32 {
    if let bundle_id = bundle_id {
        _ = String(cString: bundle_id)
    }
    return AX_NOT_IMPLEMENTED
}

// Poll the registered observer for the next speaker change and
// write a JSONL line into `*out`. Returns AX_OK + a malloc'd buffer
// on a real change, AX_NOT_IMPLEMENTED in v0.
//
// The polling-vs-callback split per §9.1 / §9.2 is deliberate: the
// Rust side runs an async loop calling `ax_poll` so back-pressure
// is in *its* hands rather than fighting AXObserver's RunLoop.
//
// Memory contract: caller frees `*out` via `ax_free_string`.
@_cdecl("ax_poll")
public func ax_poll(_ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) -> Int32 {
    if let out = out {
        // v0 stub writes empty string for compatibility with the
        // future-impl wire shape; same malloc + memcpy + NUL pattern
        // as the other bridges so an embedded NUL in a future
        // speaker name doesn't truncate the JSON.
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
    return AX_NOT_IMPLEMENTED
}

// Release the observer registered via `ax_register_observer`. Idempotent;
// safe to call when no observer is registered.
@_cdecl("ax_release_observer")
public func ax_release_observer() -> Int32 {
    AX_OK
}

// Free a string previously returned via the `out` parameter of
// `ax_poll`. Convention matches the other bridges.
@_cdecl("ax_free_string")
public func ax_free_string(_ p: UnsafeMutablePointer<CChar>?) {
    if let p = p { free(p) }
}
