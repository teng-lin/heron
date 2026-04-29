/**
 * Listen for daemon → frontend events forwarded by the Tauri-side
 * SSE bridge (`apps/desktop/src-tauri/src/events_bridge.rs`).
 *
 * Mounts once at the app shell level. Multiple components reading
 * the same store derive their state from the same stream; there's
 * no per-component subscription. Calls `heron_subscribe_events` on
 * mount to ensure the bridge task is running — idempotent, so
 * mounting twice (StrictMode) is fine. Cleanup on unmount only
 * detaches the JS listener; the Rust task keeps running until
 * `RunEvent::Exit` fires.
 */

import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";

import { invoke } from "../lib/invoke";
import type { EventEnvelope } from "../lib/types";
import { useMeetingsStore } from "../store/meetings";
import { useSpeakerStore } from "../store/speaker";
import { useTranscriptStore } from "../store/transcript";

const FRONTEND_EVENT = "heron://event";
const BRIDGE_STATUS_EVENT = "heron://bridge-status";

/** Shape emitted by the Rust bridge on `BRIDGE_STATUS_EVENT`. */
interface BridgeStatusPayload {
  state: "up" | "down";
  reason: "connected" | "auth_failed" | "reconnect_exhausted" | "stream_closed";
}

export function useSseEvents() {
  useEffect(() => {
    let unlistenEvent: (() => void) | null = null;
    let unlistenStatus: (() => void) | null = null;
    let cancelled = false;

    void (async () => {
      try {
        await invoke("heron_subscribe_events");
      } catch (err) {
        // eslint-disable-next-line no-console
        console.warn("[heron] SSE subscribe failed:", err);
        // Fall through anyway — the listener attaches and a future
        // retry from elsewhere can re-trigger the subscribe.
      }
      if (cancelled) return;

      try {
        const handle = await listen<EventEnvelope>(FRONTEND_EVENT, (event) => {
          dispatch(event.payload);
        });
        if (cancelled) {
          handle();
          return;
        }
        unlistenEvent = handle;
      } catch (err) {
        // eslint-disable-next-line no-console
        console.warn("[heron] SSE listen() failed:", err);
      }

      try {
        const handle = await listen<BridgeStatusPayload>(
          BRIDGE_STATUS_EVENT,
          (event) => {
            dispatchBridgeStatus(event.payload);
          }
        );
        if (cancelled) {
          handle();
          return;
        }
        unlistenStatus = handle;
      } catch (err) {
        // eslint-disable-next-line no-console
        console.warn("[heron] SSE bridge-status listen() failed:", err);
      }
    })();

    return () => {
      cancelled = true;
      if (unlistenEvent) unlistenEvent();
      if (unlistenStatus) unlistenStatus();
    };
  }, []);
}

/**
 * Handle a bridge-status payload from `BRIDGE_STATUS_EVENT`. Kept
 * outside the hook so it can be unit-tested directly.
 *
 * On `down`: flip `daemonDown` immediately so the Home banner renders
 * without waiting for the next `load()` call. On `up`: re-run `load()`
 * so the store re-fetches and clears `daemonDown` via the normal
 * success path.
 */
export function dispatchBridgeStatus(payload: BridgeStatusPayload) {
  if (payload.state === "down") {
    // Mirror the invariant established by `load()`'s failure path: when the
    // daemon is unreachable, clear items/pagination/summaries so the
    // DaemonDownBanner renders against a consistent empty-store shape.
    useMeetingsStore.setState({
      items: [],
      nextCursor: null,
      loading: false,
      daemonDown: true,
      summaries: {},
      error: payload.reason,
    });
  } else {
    void useMeetingsStore.getState().load();
  }
}

/**
 * Fan a parsed envelope out into the relevant Zustand stores. Kept
 * outside the hook so it's straightforward to unit-test by calling
 * directly with a synthetic envelope.
 */
export function dispatch(envelope: EventEnvelope) {
  switch (envelope.event_type) {
    case "transcript.partial":
    case "transcript.final": {
      const meetingId = envelope.meeting_id;
      if (meetingId) {
        useTranscriptStore.getState().append(meetingId, envelope.data);
      }
      return;
    }
    case "meeting.started":
    case "meeting.armed":
    case "meeting.detected": {
      // Refresh the meetings list so the Home table picks up the
      // new state. Cheap because the store coalesces in-flight
      // calls, and the daemon's vault-scan is fast.
      void useMeetingsStore.getState().load();
      return;
    }
    case "meeting.ended":
    case "meeting.completed": {
      // The AX bridge stops emitting once the recording phase ends,
      // so any unflushed `speaker.changed { started: true }` would
      // leak into the post-meeting review view as a stale badge.
      // Clear on BOTH ended (recording stopped, transcribe still
      // running) and completed (terminal) so the gap between those
      // two states never displays a phantom active speaker.
      const meetingId = envelope.meeting_id;
      if (meetingId) {
        useSpeakerStore.getState().clear(meetingId);
      }
      void useMeetingsStore.getState().load();
      return;
    }
    case "speaker.changed": {
      const meetingId = envelope.meeting_id;
      if (meetingId) {
        useSpeakerStore.getState().apply(meetingId, envelope.data);
      }
      return;
    }
    case "summary.ready": {
      const meetingId = envelope.meeting_id;
      if (meetingId) {
        // Drop any cached preview so the next hover refetches the
        // freshly-ready summary instead of showing the prior empty
        // state.
        useMeetingsStore.setState((state) => {
          const { [meetingId]: _, ...rest } = state.summaries;
          return { summaries: rest };
        });
      }
      void useMeetingsStore.getState().load();
      return;
    }
    case "action_items.ready":
    case "meeting.participant_joined":
    case "doctor.warning":
    case "daemon.error":
      // No-op for now. Toasts wired in PR 5+.
      return;
  }
}
