/**
 * Unit tests for `dispatchBridgeStatus` — the exported handler that
 * maps `heron://bridge-status` payloads onto `useMeetingsStore`.
 *
 * Follows the same headless-Zustand pattern as `salvage.test.ts` and
 * `onboarding.test.ts`: no jsdom, no render, just `getState()` /
 * `setState()`. The Tauri IPC layer (`@tauri-apps/api/core::invoke`)
 * is never reached because we stub `load` on the store before calling
 * the handler.
 */

import { afterEach, describe, expect, test } from "bun:test";

import { dispatchBridgeStatus } from "./useSseEvents";
import { useMeetingsStore } from "../store/meetings";

afterEach(() => {
  // Reset store between tests so state doesn't leak across cases.
  useMeetingsStore.setState({
    items: [],
    nextCursor: null,
    loading: false,
    daemonDown: false,
    error: null,
    summaries: {},
  });
});

describe("dispatchBridgeStatus", () => {
  test("down payload flips daemonDown to true immediately", () => {
    expect(useMeetingsStore.getState().daemonDown).toBe(false);

    dispatchBridgeStatus({ state: "down", reason: "auth_failed" });

    expect(useMeetingsStore.getState().daemonDown).toBe(true);
  });

  test("down payload with reconnect_exhausted reason also flips daemonDown", () => {
    dispatchBridgeStatus({ state: "down", reason: "reconnect_exhausted" });
    expect(useMeetingsStore.getState().daemonDown).toBe(true);
  });

  test("up payload triggers load() to clear daemonDown via normal success path", () => {
    let loadCalled = false;
    // Stub load so it doesn't try to reach Tauri IPC and marks
    // `daemonDown: false` as the real success path would.
    useMeetingsStore.setState({
      daemonDown: true,
      load: async () => {
        loadCalled = true;
        useMeetingsStore.setState({ daemonDown: false });
      },
    });

    dispatchBridgeStatus({ state: "up", reason: "connected" });

    // `load` is async but dispatched with `void` — give the microtask
    // queue one tick to flush.
    return Promise.resolve().then(() => {
      expect(loadCalled).toBe(true);
    });
  });

  test("down does not trigger a load()", () => {
    let loadCalled = false;
    useMeetingsStore.setState({
      load: async () => {
        loadCalled = true;
      },
    });

    dispatchBridgeStatus({ state: "down", reason: "stream_closed" });

    return Promise.resolve().then(() => {
      expect(loadCalled).toBe(false);
      // daemonDown must still be set even when load is stubbed.
      expect(useMeetingsStore.getState().daemonDown).toBe(true);
    });
  });
});
