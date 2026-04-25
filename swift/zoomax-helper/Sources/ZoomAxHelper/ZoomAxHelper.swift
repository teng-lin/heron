// ZoomAxHelper — Swift bridge for the §9 AXObserver integration.
// Same pattern as eventkit-helper / whisperkit-helper per
// docs/swift-bridge-pattern.md.
//
// ============================================================
// PLACEHOLDER (role, subrole, identifier) TRIPLE — SEE BELOW
// ------------------------------------------------------------
// The `SPEAKER_INDICATOR_TRIPLE` constant below is a *guess*. The
// real values must be captured against a live Zoom call using
// Xcode's Accessibility Inspector, per docs/plan.md §3.3 (week-0
// spike). Until that fixture lands, `ax_register_observer` may
// succeed at registration but never fire callbacks (because the
// triple won't match anything real in the Zoom AX tree).
//
// See `docs/manual-test-matrix.md` → "Zoom AX observer
// (heron-zoom)" for the capture procedure.
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

// MARK: - Speaker-indicator AX triple (PLACEHOLDER)
//
// TODO(spike-fixture): replace these with the values captured via
// Xcode Accessibility Inspector against a live Zoom call in the
// docs/plan.md §3.3 spike. The current values are a *plausible
// guess only*, picked so the tree-walk has a concrete shape to
// match against; they will not actually fire in production.
//
// Capture procedure: open Zoom in a meeting → launch Xcode →
// Open Developer Tool → Accessibility Inspector → target Zoom →
// hover the speaker indicator (the yellow/green frame around the
// active tile) → record `Role`, `Subrole`, `Identifier` from the
// Basic panel.
//
// The notification kind also needs verification. Speaker-indicator
// state is most likely surfaced via either kAXValueChangedNotification
// (if the indicator's value attribute toggles) or
// kAXSelectedChildrenChangedNotification (if the active-speaker tile
// is tracked as a "selected child" of the participant grid). The
// week-0 spike must pin which.
private let SPEAKER_INDICATOR_ROLE: String = "AXButton"
private let SPEAKER_INDICATOR_SUBROLE: String = "AXSpeakerIndicator"
private let SPEAKER_INDICATOR_IDENTIFIER: String = "speaker-indicator"
// TODO(spike-fixture): confirm this is the right notification.
private let SPEAKER_INDICATOR_NOTIFICATION: CFString = kAXValueChangedNotification as CFString

// MARK: - Global observer state
//
// AX requires its observer to be driven from a CFRunLoop. We
// dedicate a background thread that owns its own CFRunLoop and
// runs CFRunLoopRun() so the Rust side never has to touch
// run loops. Callbacks append to `eventQueue` under `queueLock`;
// `ax_poll` drains.

private final class ObserverState {
    let pid: pid_t
    let observer: AXObserver
    let element: AXUIElement
    let notification: CFString
    let thread: Thread
    var runLoop: CFRunLoop?  // captured by the worker thread on entry

    init(
        pid: pid_t,
        observer: AXObserver,
        element: AXUIElement,
        notification: CFString,
        thread: Thread
    ) {
        self.pid = pid
        self.observer = observer
        self.element = element
        self.notification = notification
        self.thread = thread
    }
}

private let stateLock = NSLock()
private var currentState: ObserverState?

private let queueLock = NSLock()
private var eventQueue: [String] = []

// MARK: - AX callback
//
// Fired on the worker thread's run loop. We don't yet know the
// real shape of the speaker-indicator AX value (TODO spike), so
// we emit a JSONL line shaped exactly like
// `heron_types::SpeakerEvent` with placeholder values. Once the
// spike pins how to read the speaker name + state from the
// element, replace the body with real attribute reads.
private func axObserverCallback(
    observer: AXObserver,
    element: AXUIElement,
    notification: CFString,
    refcon: UnsafeMutableRawPointer?
) {
    // TODO(spike-fixture): read participant display name from the
    // element (AXTitle on the tile? AXValue on the indicator?) and
    // the active/inactive state. For now, emit a placeholder event
    // so the wire shape can be exercised end-to-end on a developer
    // box once the triple is correct.
    let speakerEvent: [String: Any] = [
        "t": 0.0,
        "name": "unknown",
        "started": true,
        "view_mode": "active_speaker",
        "own_tile": false,
    ]
    guard let data = try? JSONSerialization.data(withJSONObject: speakerEvent, options: []),
          let json = String(data: data, encoding: .utf8)
    else { return }

    queueLock.lock()
    eventQueue.append(json)
    queueLock.unlock()
}

// MARK: - AX tree walk
//
// Depth-first walk looking for the (role, subrole, identifier)
// triple. Returns the first match (or nil if none). Bounds the
// search to MAX_DEPTH/MAX_NODES so a pathological tree can't hang
// the registration thread.
private let MAX_DEPTH: Int = 12
private let MAX_NODES: Int = 4096

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

private func findSpeakerIndicator(root: AXUIElement) -> AXUIElement? {
    var visited = 0
    func walk(_ node: AXUIElement, depth: Int) -> AXUIElement? {
        if visited >= MAX_NODES || depth > MAX_DEPTH { return nil }
        visited += 1

        let role = stringAttr(node, kAXRoleAttribute as String)
        let subrole = stringAttr(node, kAXSubroleAttribute as String)
        let ident = stringAttr(node, kAXIdentifierAttribute as String)

        if role == SPEAKER_INDICATOR_ROLE
            && subrole == SPEAKER_INDICATOR_SUBROLE
            && ident == SPEAKER_INDICATOR_IDENTIFIER
        {
            return node
        }

        for child in childrenAttr(node) {
            if let hit = walk(child, depth: depth + 1) { return hit }
        }
        return nil
    }
    return walk(root, depth: 0)
}

// MARK: - Run-loop worker thread

private final class ObserverThread: Thread {
    let observer: AXObserver
    let ready = DispatchSemaphore(value: 0)
    var capturedRunLoop: CFRunLoop?

    init(observer: AXObserver) {
        self.observer = observer
        super.init()
        self.name = "heron.zoomax.observer"
    }

    override func main() {
        let rl = CFRunLoopGetCurrent()
        capturedRunLoop = rl
        let source = AXObserverGetRunLoopSource(observer)
        CFRunLoopAddSource(rl, source, .defaultMode)
        ready.signal()

        // Run until ax_release_observer() removes our source and
        // calls CFRunLoopStop. Using a defaultMode loop with
        // returnAfterSourceHandled=false keeps the observer hot.
        CFRunLoopRun()
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

    // Reject double-registration (no leak; ax_release first).
    stateLock.lock()
    if currentState != nil {
        stateLock.unlock()
        return AX_INTERNAL
    }
    stateLock.unlock()

    // 3. Build the application AX element.
    let appElement = AXUIElementCreateApplication(pid)

    // 4. Walk for the (role, subrole, identifier) triple.
    //
    // TODO(spike-fixture): the placeholder triple at the top of
    // this file will not match anything real — registration may
    // therefore "succeed" (we hand back AX_OK once the observer
    // is wired) but the callback will never fire. Once the spike
    // pins the real values, this call returns the actual indicator
    // node and AX_OK has its full meaning.
    let target = findSpeakerIndicator(root: appElement) ?? appElement

    // 5. Create the observer.
    var observerOpt: AXObserver?
    let createErr = AXObserverCreate(pid, axObserverCallback, &observerOpt)
    guard createErr == .success, let observer = observerOpt else {
        return AX_INTERNAL
    }

    // 6. Subscribe to the notification.
    let addErr = AXObserverAddNotification(
        observer,
        target,
        SPEAKER_INDICATOR_NOTIFICATION,
        nil
    )
    if addErr != .success {
        // Observer is held by ARC; nothing to free explicitly.
        return AX_INTERNAL
    }

    // 7. Spin up a thread that owns a CFRunLoop and adds the
    // observer's source to it. We block until that thread reports
    // `ready` so we can safely stash the runLoop ref for shutdown.
    let thread = ObserverThread(observer: observer)
    thread.start()
    _ = thread.ready.wait(timeout: .now() + .seconds(5))

    let state = ObserverState(
        pid: pid,
        observer: observer,
        element: target,
        notification: SPEAKER_INDICATOR_NOTIFICATION,
        thread: thread
    )
    state.runLoop = thread.capturedRunLoop

    stateLock.lock()
    currentState = state
    stateLock.unlock()

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

    // Remove our notification (best-effort; the observer is about
    // to die anyway).
    _ = AXObserverRemoveNotification(state.observer, state.element, state.notification)

    // Stop the worker thread's run loop. Touching `runLoop` from
    // another thread is documented-safe for CFRunLoopStop; the
    // worker exits CFRunLoopRun() and then ObserverThread.main
    // returns, releasing the thread.
    if let rl = state.runLoop {
        CFRunLoopStop(rl)
    }

    // Drain any leftover events so a fresh registration starts
    // clean.
    queueLock.lock()
    eventQueue.removeAll()
    queueLock.unlock()

    return AX_OK
}

@_cdecl("ax_free_string")
public func ax_free_string(_ p: UnsafeMutablePointer<CChar>?) {
    if let p = p { free(p) }
}
