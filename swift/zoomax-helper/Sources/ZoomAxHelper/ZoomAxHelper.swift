// ZoomAxHelper — Swift bridge for the §9 AXObserver integration.
// Same pattern as eventkit-helper / whisperkit-helper per
// docs/archives/swift-bridge-pattern.md.
//
// ============================================================
// MUTE-STATE ATTRIBUTION (Path C, post-§3.3 spike)
// ------------------------------------------------------------
// The week-0 spike (see fixtures/zoom/spike-triple/{muted,speaking}.json)
// established that Zoom 7.0.0 does NOT expose the active-speaker
// indicator (the colored frame around the speaking tile) via
// Accessibility. Every per-tile attribute that could carry "speaking
// now" state — subrole, identifier, value, selected — is null. The
// only signal Zoom surfaces is per-participant **mute state**, encoded
// in the AXDescription on each AXTabGroup at depth 2:
//
//     "<Name>, Computer audio (muted|unmuted)[, Video (off|on)]"
//
// This bridge enumerates those tiles, parses the description, and
// emits a SpeakerEvent on every transition (new participant joins,
// mute toggle, participant leaves). The aligner intersects these
// "potentially speaking" intervals with tap-audio energy turns to
// arrive at attribution. In a 1:1 call (the dominant case for client
// meetings) this is reliable: only one remote participant can be
// unmuted ↔ they're the speaker. In free-for-all 3+ calls it
// degrades to "best guess by overlap" — see docs/archives/implementation.md
// §20 risk-reducer for the documented `speaker: "them"` fallback.
//
// Polling-only: subscribing to AX notifications (kAXValueChanged,
// kAXTitleChanged) was considered but rejected because Zoom's
// notification firing on AXTabGroup tiles is unverified, polling at
// 250ms cadence is reliable + bounded CPU, and the aligner's
// 350ms default lag prior already absorbs the worst-case detection
// latency.
// ============================================================

import Foundation
#if canImport(ApplicationServices)
import ApplicationServices
#endif
#if canImport(AppKit)
import AppKit
#endif

// MARK: - Status codes (mirror ax_bridge.rs::AxStatus 1-for-1)

private let AX_OK: Int32 = 0
private let AX_NOT_IMPLEMENTED: Int32 = -1
private let AX_PROCESS_NOT_RUNNING: Int32 = -2
private let AX_NO_PERMISSION: Int32 = -3
private let AX_INTERNAL: Int32 = -4

// MARK: - Tree-walk bounds
//
// Same bounds as the original triple-based walk: 12 deep / 4096 nodes
// is enough for a fully-populated 49-tile Zoom gallery. The polling
// thread re-walks at 4 Hz so even a worst-case full traversal is
// ~16K nodes/sec — sub-millisecond CPU on Apple silicon.

private let MAX_DEPTH: Int = 12
private let MAX_NODES: Int = 4096
private let POLL_INTERVAL_SECONDS: TimeInterval = 0.25

// MARK: - Participant state

/// Parsed contents of a participant tile's AXDescription.
private struct ParticipantState: Equatable {
    let muted: Bool
    /// `nil` when the AX description doesn't include video state.
    /// Zoom seems to only append `, Video off`/`, Video on` when the
    /// tile has explicit video state; absent for camera-on participants
    /// in some layouts. Tracked for future use; not currently emitted.
    let videoOff: Bool?
}

// MARK: - Global observer state
//
// One polling thread per registration. The thread re-walks the AX
// tree every POLL_INTERVAL_SECONDS, diffs current participant state
// against the stored snapshot, and appends a SpeakerEvent JSONL line
// to `eventQueue` per transition. `ax_poll` drains.

private final class ObserverState {
    let pid: pid_t
    let thread: PollingThread
    /// Participant name → last known state. Protected by `stateLock`:
    /// the polling thread writes it from `PollingThread.main` and
    /// `pollOnce`, and `ax_release_observer` reads/clears it during
    /// teardown. The mutation pattern is "lock, clone-or-replace,
    /// unlock" so the lock isn't held across the (longer) tree walks
    /// that produce the new map.
    var states: [String: ParticipantState]

    init(pid: pid_t, thread: PollingThread) {
        self.pid = pid
        self.thread = thread
        self.states = [:]
    }
}

private let stateLock = NSLock()
private var currentState: ObserverState?

private let queueLock = NSLock()
private var eventQueue: [String] = []

// MARK: - Description parsing
//
// Zoom's per-tile AXDescription has the canonical shape
//     "<Name>, Computer audio (muted|unmuted)[, Video (off|on)]"
// (verified against fixtures/zoom/spike-triple/). The regex is
// anchored end-to-end so a garbled description (e.g. a transient
// "Connecting..." state) doesn't get parsed into a silent false
// positive.
//
// English-only: this regex assumes the host's locale is English.
// On a non-English Zoom client the AXDescription text is localized
// (e.g. "<Name>, Computeraudio aus") and tiles silently fall through
// the parser. The aligner's 30s ATTRIBUTION_GAP_THRESHOLD will
// surface the resulting silence as `Event::AttributionDegraded`,
// which is the right operator-visible signal — but a future
// enhancement should localize this regex (or read mute state from
// a non-string AX attribute, if one exists in a later Zoom build).
// Tracked in `fixtures/zoom/spike-triple/README.md`.

/// Compiled once at first use: the pattern is a compile-time literal
/// verified against `fixtures/zoom/spike-triple/`, so an
/// `NSRegularExpression(pattern:)` failure here is a programmer
/// error, not a runtime condition. `try!` makes that explicit;
/// hoisting out of `parseTileDescription` avoids re-compiling the
/// pattern on every tile (called 4 Hz × N participants per second).
private let tileDescriptionRegex: NSRegularExpression = {
    // swiftlint:disable:next force_try
    try! NSRegularExpression(
        pattern: #"^(.+?), Computer audio (muted|unmuted)(?:, Video (off|on))?$"#
    )
}()

/// Parsed tile description. `nil` when the description doesn't match
/// the known Zoom shape — the caller skips that tile silently. See
/// the section comment above on the English-only assumption.
private func parseTileDescription(_ s: String) -> (name: String, state: ParticipantState)? {
    // Capture groups: 1 = name, 2 = mute state, 3 = video state (optional).
    let nsRange = NSRange(s.startIndex..<s.endIndex, in: s)
    guard let match = tileDescriptionRegex.firstMatch(in: s, options: [], range: nsRange),
          match.numberOfRanges >= 3,
          let nameRange = Range(match.range(at: 1), in: s),
          let stateRange = Range(match.range(at: 2), in: s)
    else { return nil }

    let name = String(s[nameRange])
    let muted = s[stateRange] == "muted"
    let videoOff: Bool? = match.numberOfRanges >= 4
        ? Range(match.range(at: 3), in: s).map { String(s[$0]) == "off" }
        : nil
    return (name, ParticipantState(muted: muted, videoOff: videoOff))
}

// MARK: - AX attribute helpers

private func stringAttr(_ element: AXUIElement, _ key: String) -> String? {
    var raw: CFTypeRef?
    let err = AXUIElementCopyAttributeValue(element, key as CFString, &raw)
    if err != .success { return nil }
    return raw as? String
}

private func childrenAttr(_ element: AXUIElement) -> [AXUIElement] {
    var raw: CFTypeRef?
    let err = AXUIElementCopyAttributeValue(element, kAXChildrenAttribute as CFString, &raw)
    if err != .success { return [] }
    return (raw as? [AXUIElement]) ?? []
}

// MARK: - Tile enumeration

/// Walk the AX tree under `root` and call `onTile` for each
/// participant tile (AXTabGroup at depth 2 with a parseable
/// `Computer audio …` description).
///
/// Depth 2 is what the spike fixture pinned: depth 0 = application,
/// depth 1 = standard window, depth 2 = participant tile. Walking
/// every depth would catch the participants-panel sidebar entries
/// too (which carry the same name) and double-count.
private func enumerateParticipantTiles(
    root: AXUIElement,
    onTile: (String, ParticipantState) -> Void
) {
    var visited = 0
    func walk(_ node: AXUIElement, depth: Int) {
        if visited >= MAX_NODES || depth > MAX_DEPTH { return }
        visited += 1
        if depth == 2,
           stringAttr(node, kAXRoleAttribute as String) == "AXTabGroup",
           let desc = stringAttr(node, kAXDescriptionAttribute as String),
           let parsed = parseTileDescription(desc)
        {
            onTile(parsed.name, parsed.state)
        }
        for child in childrenAttr(node) {
            walk(child, depth: depth + 1)
        }
    }
    walk(root, depth: 0)
}

// MARK: - Event emission

/// Append one SpeakerEvent JSONL line to `eventQueue`. The
/// `t = 0.0` placeholder is overwritten by the Rust polling worker
/// with `clock.elapsed_secs()` on receipt — Swift has no session-clock
/// reference, and the worst-case 250ms polling lag is well within the
/// aligner's 350ms default `event_lag` prior.
///
/// Schema matches `heron_types::SpeakerEvent` (snake_case):
///     { t, name, started, view_mode, own_tile }
///
/// Semantics for mute-state attribution:
///   started=true  ↔ participant transitioned to UNMUTED (potentially speaker)
///   started=false ↔ participant transitioned to MUTED (no longer speaker)
///
/// `own_tile` is always `false` here — the bridge can't distinguish
/// self from remote without the user's display name. The orchestrator
/// applies that filter downstream once it has access to settings.
private func emitMuteTransition(name: String, muted: Bool) {
    // Bail before doing any work if our calling thread (the polling
    // thread) was cancelled — `ax_release_observer` flips the flag
    // and then drains `eventQueue` under `queueLock`, so an emit
    // that completes after that drain would leak a stale event into
    // the next registration's `ax_poll`.
    if Thread.current.isCancelled { return }

    let speakerEvent: [String: Any] = [
        "t": 0.0,
        "name": name,
        "started": !muted,
        "view_mode": "active_speaker",
        "own_tile": false,
    ]
    guard let data = try? JSONSerialization.data(withJSONObject: speakerEvent, options: []),
          let json = String(data: data, encoding: .utf8)
    else { return }

    queueLock.lock()
    defer { queueLock.unlock() }
    // Re-check after acquiring `queueLock`: `ax_release_observer`
    // holds this same lock through cancel + drain, so by the time
    // we get here on the cancellation race, `isCancelled` is true
    // AND the drain has already run.
    if Thread.current.isCancelled { return }
    eventQueue.append(json)
}

// MARK: - Polling thread

private final class PollingThread: Thread {
    let pid: pid_t
    let appElement: AXUIElement
    /// `cancel()` flips `isCancelled` which the loop checks each
    /// iteration. The `ready` semaphore signals that the thread has
    /// at least entered the loop, so `ax_register_observer` knows it
    /// can rely on the polling-thread state being live before
    /// returning success to the Rust side.
    let ready = DispatchSemaphore(value: 0)

    init(pid: pid_t, appElement: AXUIElement) {
        self.pid = pid
        self.appElement = appElement
        super.init()
        self.name = "heron.zoomax.polling"
    }

    override func main() {
        ready.signal()
        // Initial enumeration: emit `started=!muted` for every
        // participant currently in the tree so the aligner sees a
        // SpeakingInterval start for every unmuted participant from
        // session t=0. Without this, a participant who's been
        // unmuted since before recording started would never appear
        // in the aligner's pending_starts and their turns would fall
        // back to channel attribution.
        var initialStates: [String: ParticipantState] = [:]
        enumerateParticipantTiles(root: appElement) { name, state in
            initialStates[name] = state
            emitMuteTransition(name: name, muted: state.muted)
        }
        stateLock.lock()
        currentState?.states = initialStates
        stateLock.unlock()

        while !isCancelled {
            Thread.sleep(forTimeInterval: POLL_INTERVAL_SECONDS)
            if isCancelled { break }
            pollOnce()
        }
    }

    /// One polling tick: re-walk the tree, diff against the stored
    /// state map, emit SpeakerEvents for every transition, and
    /// update the stored state.
    ///
    /// Same-name collision: the `observed` and stored `states` maps
    /// are keyed by parsed display name. Two participants sharing a
    /// name (uncommon in client meetings, possible with first-name-
    /// only displays — e.g. two "Alex"es) collapse into one entry,
    /// and only one of their mute transitions is observable. The
    /// dominant 1:1 case isn't affected; full disambiguation lands
    /// alongside `own_tile` detection (see `emitMuteTransition`'s
    /// header comment) by switching to a (name, occurrence-index)
    /// composite key.
    private func pollOnce() {
        // Bail before walking the tree if release fired since we
        // last slept — saves the AX-call bill on a doomed tick.
        if isCancelled { return }

        var observed: [String: ParticipantState] = [:]
        enumerateParticipantTiles(root: appElement) { name, state in
            observed[name] = state
        }

        // Bail before touching shared state if release fired during
        // the walk. Combined with the queue-lock drain in
        // `ax_release_observer` and the per-emit guard in
        // `emitMuteTransition`, this keeps stale events out of the
        // next registration's queue.
        if isCancelled { return }

        stateLock.lock()
        let prior = currentState?.states ?? [:]
        currentState?.states = observed
        stateLock.unlock()

        // Emit transitions:
        //  - new participant: emit current mute state as a transition
        //    (so the aligner picks them up mid-session).
        //  - mute state changed: emit the new state.
        //  - participant left: emit `started=false` so any open
        //    SpeakingInterval in the aligner closes cleanly.
        for (name, state) in observed {
            if let priorState = prior[name] {
                if priorState.muted != state.muted {
                    emitMuteTransition(name: name, muted: state.muted)
                }
            } else {
                emitMuteTransition(name: name, muted: state.muted)
            }
        }
        for (name, _) in prior where observed[name] == nil {
            emitMuteTransition(name: name, muted: true)
        }
    }
}

// MARK: - @_cdecl entry points

@_cdecl("ax_register_observer")
public func ax_register_observer(_ bundle_id: UnsafePointer<CChar>?) -> Int32 {
    guard let bundle_id = bundle_id else { return AX_INTERNAL }
    let bundleId = String(cString: bundle_id)

    // 1. Find pid by bundle id.
    let apps = NSRunningApplication.runningApplications(withBundleIdentifier: bundleId)
    guard let app = apps.first else { return AX_PROCESS_NOT_RUNNING }
    let pid = app.processIdentifier

    // 2. Check Accessibility permission. We pass an empty options
    // dict so we *don't* prompt the user — that's the orchestrator's
    // job during onboarding (§5.5). Trust must already be granted.
    let opts: CFDictionary = [:] as CFDictionary
    if !AXIsProcessTrustedWithOptions(opts) {
        return AX_NO_PERMISSION
    }

    // Hold stateLock for the whole registration. Without this, two
    // concurrent callers can both pass the `currentState != nil`
    // check and race to allocate threads, leaking the loser's
    // resources when the assignment below overwrites them.
    stateLock.lock()
    defer { stateLock.unlock() }

    if currentState != nil {
        return AX_INTERNAL
    }

    // 3. Build the application AX element + spawn the polling
    // thread. The thread does its own initial enumeration on entry,
    // so by the time `ready` signals we've emitted the t=0 baseline
    // events for every participant currently in the tree.
    let appElement = AXUIElementCreateApplication(pid)
    let thread = PollingThread(pid: pid, appElement: appElement)
    thread.start()
    if thread.ready.wait(timeout: .now() + .seconds(5)) == .timedOut {
        thread.cancel()
        return AX_INTERNAL
    }

    currentState = ObserverState(pid: pid, thread: thread)
    return AX_OK
}

@_cdecl("ax_poll")
public func ax_poll(_ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) -> Int32 {
    guard let out = out else { return AX_INTERNAL }

    // Drain one event from the queue (FIFO).
    queueLock.lock()
    let next: String? = eventQueue.isEmpty ? nil : eventQueue.removeFirst()
    queueLock.unlock()

    let payload = next ?? ""
    // `withCString` hands us a NUL-terminated buffer the runtime owns
    // for the duration of the closure. We `memcpy` it into a malloc'd
    // buffer the Rust side will free via `ax_free_string`.
    return payload.withCString { src -> Int32 in
        let count = strlen(src)
        guard let buf = malloc(count + 1)?.assumingMemoryBound(to: CChar.self) else {
            out.pointee = nil
            return AX_INTERNAL
        }
        memcpy(buf, src, count + 1) // includes the trailing NUL
        out.pointee = buf
        return AX_OK
    }
}

@_cdecl("ax_release_observer")
public func ax_release_observer() -> Int32 {
    stateLock.lock()
    let state = currentState
    currentState = nil
    stateLock.unlock()

    guard let state = state else {
        // Idempotent: nothing registered → still AX_OK.
        return AX_OK
    }

    // Cancel and drain under `queueLock`: this serialises with
    // `emitMuteTransition`'s lock-then-recheck-isCancelled pattern,
    // so any in-flight `pollOnce` either appended before our drain
    // (we clear it) or sees `isCancelled == true` once it acquires
    // the lock (and skips the append). Without this serialisation,
    // a `pollOnce` already past its own `isCancelled` check at
    // teardown could append between our `cancel()` and our drain,
    // leaking a stale event into the next registration's `ax_poll`.
    queueLock.lock()
    defer { queueLock.unlock() }
    state.thread.cancel()
    eventQueue.removeAll()

    return AX_OK
}

@_cdecl("ax_free_string")
public func ax_free_string(_ p: UnsafeMutablePointer<CChar>?) {
    if let p = p { free(p) }
}

// MARK: - Tree-dump helper for the docs/archives/plan.md §3.3 spike
//
// Retained from the original spike workflow: `heron ax-dump` walks
// the AX tree and emits a JSON document per node. Useful for re-
// verifying the AXDescription contract against future Zoom versions
// (if Zoom changes the description format, polling stops parsing
// names and the live test catches it via the missing-event timeout).

private func describeNode(_ node: AXUIElement, depth: Int) -> [String: Any] {
    var entry: [String: Any] = ["depth": depth]
    if let role = stringAttr(node, kAXRoleAttribute as String) {
        entry["role"] = role
    }
    if let subrole = stringAttr(node, kAXSubroleAttribute as String) {
        entry["subrole"] = subrole
    }
    if let ident = stringAttr(node, kAXIdentifierAttribute as String) {
        entry["identifier"] = ident
    }
    if let title = stringAttr(node, kAXTitleAttribute as String) {
        entry["title"] = title
    }
    if let desc = stringAttr(node, kAXDescriptionAttribute as String) {
        entry["description"] = desc
    }
    if let help = stringAttr(node, kAXHelpAttribute as String) {
        entry["help"] = help
    }
    if let value = anyAttr(node, kAXValueAttribute as String) {
        entry["value"] = value
    }
    if let selected = anyAttr(node, kAXSelectedAttribute as String) {
        entry["selected"] = selected
    }
    return entry
}

/// Permissive variant of [`stringAttr`] that handles non-string CF
/// types so the spike dumper can diff `bool`/`number`/`AXValue`
/// attributes between captures, not just strings.
private func anyAttr(_ element: AXUIElement, _ key: String) -> Any? {
    var raw: CFTypeRef?
    let err = AXUIElementCopyAttributeValue(element, key as CFString, &raw)
    if err != .success { return nil }
    guard let raw = raw else { return nil }
    if let s = raw as? String { return s }
    if CFGetTypeID(raw) == CFBooleanGetTypeID() {
        return (raw as? Bool) ?? false
    }
    if let n = raw as? NSNumber { return n }
    if let desc = CFCopyDescription(raw) as String? { return desc }
    return nil
}

@_cdecl("ax_dump_tree")
public func ax_dump_tree(
    _ bundle_id: UnsafePointer<CChar>?,
    _ max_nodes: Int32,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let bundle_id = bundle_id, let out = out else { return AX_INTERNAL }
    let bundleId = String(cString: bundle_id)

    let apps = NSRunningApplication.runningApplications(withBundleIdentifier: bundleId)
    guard let app = apps.first else { return AX_PROCESS_NOT_RUNNING }
    let pid = app.processIdentifier

    let opts: CFDictionary = [:] as CFDictionary
    if !AXIsProcessTrustedWithOptions(opts) {
        return AX_NO_PERMISSION
    }

    let appElement = AXUIElementCreateApplication(pid)

    // Cap walks at the smaller of the caller-requested max_nodes and
    // our internal MAX_NODES so a buggy caller can't ask us to walk
    // an unbounded tree.
    let cap: Int = max_nodes <= 0 ? MAX_NODES : min(Int(max_nodes), MAX_NODES)
    var entries: [[String: Any]] = []
    var visited = 0

    func walk(_ node: AXUIElement, depth: Int) {
        if visited >= cap || depth > MAX_DEPTH { return }
        visited += 1
        entries.append(describeNode(node, depth: depth))
        for child in childrenAttr(node) {
            walk(child, depth: depth + 1)
        }
    }
    walk(appElement, depth: 0)

    let payload: [String: Any] = [
        "bundle_id": bundleId,
        "pid": Int(pid),
        "node_count": entries.count,
        "node_cap": cap,
        "max_depth": MAX_DEPTH,
        "nodes": entries,
    ]
    guard let data = try? JSONSerialization.data(withJSONObject: payload, options: []),
          let json = String(data: data, encoding: .utf8)
    else {
        out.pointee = nil
        return AX_INTERNAL
    }

    return json.withCString { src -> Int32 in
        let count = strlen(src)
        guard let buf = malloc(count + 1)?.assumingMemoryBound(to: CChar.self) else {
            out.pointee = nil
            return AX_INTERNAL
        }
        memcpy(buf, src, count + 1)
        out.pointee = buf
        return AX_OK
    }
}
