/**
 * Listen for `nav:<target>` events emitted by the menubar tray (or by
 * `heron_open_window`) and translate them into `react-router`
 * navigations.
 *
 * Phase 64 (PR-β). The Rust side can't push a route directly because
 * the router lives in JS; instead, the tray menu and the
 * `heron_open_window` Tauri command emit a Tauri event which this
 * hook bridges into `useNavigate()`.
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
      const results = await Promise.all(
        Object.entries(EVENT_TO_PATH).map(async ([event, path]) => {
          const unlisten = await listen(event, () => {
            navigateRef.current(path);
          });
          return unlisten;
        }),
      );
      if (cancelled) {
        for (const u of results) {
          u();
        }
        return;
      }
      unlisteners.push(...results);
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
