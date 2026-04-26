/**
 * Listen for `nav:<target>` events emitted by the menubar tray (or by
 * `heron_open_window`) and translate them into `react-router`
 * navigations.
 *
 * Phase 64 (PR-β) shipped the static `nav:settings` / `nav:recording`
 * routes. Phase 69 (PR-η) extended the bridge with `nav:review`
 * (payload `{ sessionId: string }`) so the tray's "Open last note…"
 * item can navigate to `/review/<sessionId>`.
 *
 * Phase 75 (PR-ν). The previous `nav:no_last_note` event + Sonner
 * toast were replaced by a native macOS notification fired directly
 * from the tray dispatch (see `tray.rs::notify_no_last_note`). The
 * frontend listener and toast were removed because a toast that
 * appeared in the React tree only when the focused window happened to
 * be heron was the wrong surface — a real notification lands in
 * Notification Center regardless of focus state. This hook now only
 * handles route-style nav events.
 *
 * Mounted exactly once at the app shell level. The hook returns
 * nothing — its only side effect is the listener wiring.
 */

import { useEffect, useRef } from "react";
import { useNavigate } from "react-router-dom";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Map from event name (Rust side) to React-router path. */
const EVENT_TO_PATH: Readonly<Record<string, string>> = {
  "nav:settings": "/settings",
  "nav:recording": "/recording",
};

/** Payload shape for `nav:review`. Matches the Rust `ReviewPayload`. */
interface ReviewPayload {
  sessionId?: string;
}

export function useTrayNav() {
  const navigate = useNavigate();
  // `useNavigate()`'s identity is *typically* stable in react-router 7,
  // but routing-context changes can swap it. Threading the latest
  // value through a ref means the listener registration runs exactly
  // once for the lifetime of the component without us having to
  // depend on react-router's stability guarantees.
  const navigateRef = useRef(navigate);
  navigateRef.current = navigate;

  useEffect(() => {
    // `listen` is async, so we collect unlisteners as they resolve and
    // cancel them all on unmount. If the component unmounts before
    // the registration finishes, we still cancel via the `cancelled`
    // flag below — otherwise we'd leak a Tauri listener on every
    // hot-reload (and double-fire navigations under React 18+ strict
    // mode's mount/unmount/mount cycle).
    let cancelled = false;
    const unlisteners: UnlistenFn[] = [];

    const subscribe = async () => {
      // Register all listeners in parallel. `listen` returns the
      // unlistener after the runtime's IPC ack, and the order in
      // which entries register doesn't matter — running them
      // concurrently halves the registration latency without changing
      // semantics.
      const staticResults = await Promise.all(
        Object.entries(EVENT_TO_PATH).map(async ([event, path]) => {
          const unlisten = await listen(event, () => {
            navigateRef.current(path);
          });
          return unlisten;
        }),
      );

      // `nav:review` carries `{ sessionId }`. Validate the payload
      // shape before navigating so a malformed event can't push us
      // to a bogus URL.
      const reviewUnlisten = await listen<ReviewPayload>(
        "nav:review",
        (event) => {
          const sessionId = event.payload?.sessionId;
          if (typeof sessionId === "string" && sessionId.length > 0) {
            navigateRef.current(`/review/${encodeURIComponent(sessionId)}`);
          }
        },
      );

      const allResults: UnlistenFn[] = [...staticResults, reviewUnlisten];
      if (cancelled) {
        for (const u of allResults) {
          u();
        }
        return;
      }
      unlisteners.push(...allResults);
    };

    void subscribe();

    return () => {
      cancelled = true;
      for (const u of unlisteners) {
        u();
      }
    };
    // Empty deps: we want exactly one registration for the component's
    // lifetime; the navigate ref above keeps the closure fresh.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
