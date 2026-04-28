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
import { useTranscriptStore } from "../store/transcript";

const FRONTEND_EVENT = "heron://event";

export function useSseEvents() {
  useEffect(() => {
    let unlisten: (() => void) | null = null;
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

      const handle = await listen<EventEnvelope>(FRONTEND_EVENT, (event) => {
        dispatch(event.payload);
      });
      if (cancelled) {
        handle();
        return;
      }
      unlisten = handle;
    })();

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);
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
    case "meeting.ended":
    case "meeting.detected": {
      // Refresh the meetings list so the Home table picks up the
      // new state. Cheap because the store coalesces in-flight
      // calls, and the daemon's vault-scan is fast.
      void useMeetingsStore.getState().load();
      return;
    }
    case "meeting.completed": {
      void useMeetingsStore.getState().load();
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
