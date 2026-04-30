/**
 * In-progress recording view — `/recording`.
 *
 * UI revamp PR 4: shows the live transcript pane (driven by SSE
 * events through `useTranscriptStore`) plus participants from the
 * currently-recording meeting (if any). The chrome's REC pill in
 * the TitleBar replaces the page-level "RECORDING · Microphone"
 * label that lived here before.
 *
 * Gap #7 (this PR): the Stop & Save button now actually ends the
 * daemon-side capture via `heron_end_meeting`. The Start path lives
 * on the Home page; this page accepts arrivals from either Start or
 * an existing CLI/detector-driven meeting (in which case
 * `useRecordingStore.meetingId` is null and we fall back to the
 * meetings store's active meeting id).
 */

import { useEffect, useMemo, useState } from "react";
import { Mic } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { DaemonDownBanner } from "../components/DaemonDownBanner";
import { Avatar } from "../components/ui/avatar";
import { Button } from "../components/ui/button";
import { invoke } from "../lib/invoke";
import type { Meeting, TranscriptSegment } from "../lib/types";
import { useMeetingsStore } from "../store/meetings";
import { formatElapsed, useRecordingStore } from "../store/recording";
import { useSettingsStore } from "../store/settings";
import { useTranscriptStore } from "../store/transcript";

// Stable empty-segments sentinel. See the selector in Recording for
// why we can't `?? []` inline.
const EMPTY_SEGMENTS: TranscriptSegment[] = [];

export default function Recording() {
  const navigate = useNavigate();
  const recordingStart = useRecordingStore((s) => s.recordingStart);
  const recordingMeetingId = useRecordingStore((s) => s.meetingId);
  const paused = useRecordingStore((s) => s.paused);
  const togglePause = useRecordingStore((s) => s.togglePause);
  const stop = useRecordingStore((s) => s.stop);
  const settingsPath = useSettingsStore((s) => s.settingsPath);
  const [stopping, setStopping] = useState(false);
  const [pauseToggling, setPauseToggling] = useState(false);

  const meetings = useMeetingsStore((s) => s.items);
  const loadMeetings = useMeetingsStore((s) => s.load);
  const activeMeeting = useMemo<Meeting | null>(
    () =>
      meetings.find((m) => m.status === "recording" || m.status === "armed") ??
      null,
    [meetings],
  );
  // Track whether the meetings store has settled at least once on this
  // mount, so we don't bounce the user back to /home before we know
  // whether the daemon has a live meeting (deeplink case).
  const [meetingsSettled, setMeetingsSettled] = useState(false);
  // NB: don't `?? []` inside the selector — that returns a new array
  // ref every call, which makes zustand's useSyncExternalStore see a
  // different snapshot on every render and triggers an infinite
  // re-render loop. Keep the selector pure and substitute the empty
  // sentinel outside.
  const meetingSegments = useTranscriptStore((s) =>
    activeMeeting ? s.segments[activeMeeting.id] : undefined,
  );
  const segments = meetingSegments ?? EMPTY_SEGMENTS;

  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (recordingStart === null) return;
    const id = setInterval(() => setNow(Date.now()), 1_000);
    return () => clearInterval(id);
  }, [recordingStart]);

  // Deeplink / sidebar-click case: user opens /recording without an
  // active recording. Kick off a meetings load so the SSE-driven
  // `meeting.started` cache is fresh, then render the "nothing live"
  // empty state below — we don't bounce them anywhere because that
  // looked broken (page flickered before snapping back to /home).
  useEffect(() => {
    if (recordingStart !== null) return;
    let cancelled = false;
    void loadMeetings().finally(() => {
      if (!cancelled) setMeetingsSettled(true);
    });
    return () => {
      cancelled = true;
    };
  }, [recordingStart, loadMeetings]);

  const isLive = recordingStart !== null || activeMeeting !== null;

  const elapsedMs =
    recordingStart === null ? 0 : Math.max(0, now - recordingStart);

  // Resolve which meeting Stop should end. Prefer the id we got back
  // from `heron_start_capture` (set on the Home page's Start button).
  // Fall back to the meetings store's active meeting — covers the
  // CLI / external-detector path where the user navigated to
  // /recording without our Home button starting the session. The
  // button is disabled when neither is available.
  const stopTargetId = recordingMeetingId ?? activeMeeting?.id ?? null;

  const handleTogglePause = async () => {
    // Tier 3 #16: the Pause/Resume button used to be a local-only
    // flag — the daemon kept writing frames while the user thought
    // they were paused. Now we hit the daemon's pause/resume HTTP
    // endpoints; only on a successful 204 do we flip the local flag.
    // On daemon error we surface a toast and leave the local flag
    // alone so the button accurately reflects the daemon's view.
    if (pauseToggling) return;
    if (stopTargetId === null) {
      toast.error("No active meeting to pause.");
      return;
    }
    setPauseToggling(true);
    try {
      const command = paused ? "heron_resume_meeting" : "heron_pause_meeting";
      const outcome = await invoke(command, { meetingId: stopTargetId });
      if (outcome.kind !== "ok") {
        toast.error(
          `Could not ${paused ? "resume" : "pause"} recording: ${outcome.detail}`,
        );
        return;
      }
      togglePause();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(
        `Could not ${paused ? "resume" : "pause"} recording: ${message}`,
      );
    } finally {
      setPauseToggling(false);
    }
  };

  const handleStop = async () => {
    if (stopping) return;
    if (stopTargetId === null) {
      // Defence in depth: the button is `disabled` when this is
      // null, but a hotkey or programmatic click could still race.
      toast.error("No active meeting to stop.");
      return;
    }
    setStopping(true);
    try {
      const outcome = await invoke("heron_end_meeting", {
        meetingId: stopTargetId,
      });
      if (outcome.kind !== "ok") {
        // Surface the daemon's error and stay on the page — clearing
        // local state would lie about the daemon's view of the
        // session. The user can retry; if the daemon really is gone,
        // the daemon-down banner takes over.
        toast.error(`Could not stop recording: ${outcome.detail}`);
        return;
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not stop recording: ${message}`);
      return;
    } finally {
      setStopping(false);
    }
    // Capture before `stop()` resets the recording-store state.
    const savedTitle = activeMeeting?.title ?? null;
    const savedId = stopTargetId;
    stop();
    toast.success("Recording saved", {
      description: savedTitle ?? undefined,
      action: settingsPath
        ? {
            label: "Reveal vault in Finder",
            onClick: () => {
              void invoke("heron_open_vault_folder", { settingsPath }).catch(
                (err: unknown) => {
                  const message =
                    err instanceof Error ? err.message : String(err);
                  toast.error(`Could not open vault folder: ${message}`);
                },
              );
            },
          }
        : undefined,
      duration: 8_000,
    });
    navigate(`/review/${encodeURIComponent(savedId)}`);
  };

  if (!isLive) {
    return (
      <>
        <DaemonDownBanner />
        <main className="mx-auto w-full max-w-5xl px-8 py-10">
          <header className="mb-8">
            <p
              className="font-mono text-xs uppercase tracking-[0.12em]"
              style={{ color: "var(--color-ink-3)" }}
            >
              In progress
            </p>
            <h1
              className="mt-1 font-serif text-[32px] leading-tight"
              style={{
                color: "var(--color-ink)",
                letterSpacing: "-0.02em",
              }}
            >
              No recording right now
            </h1>
            <p
              className="mt-2 max-w-prose text-sm"
              style={{ color: "var(--color-ink-2)" }}
            >
              {meetingsSettled
                ? "Start a recording from Home, the tray, or ⌘⇧R. Live captions and participants will appear here once a meeting is detected or you press record."
                : "Checking for an active session…"}
            </p>
            <div className="mt-4 flex items-center gap-2">
              <Button onClick={() => navigate("/home")}>
                <Mic size={14} aria-hidden="true" />
                Go to Home
              </Button>
            </div>
          </header>
        </main>
      </>
    );
  }

  return (
    <>
      <DaemonDownBanner />
      <main className="mx-auto w-full max-w-5xl px-8 py-8">
        <header className="mb-6 flex items-end justify-between">
          <div>
            <p
              className="font-mono text-xs uppercase tracking-[0.12em]"
              style={{ color: "var(--color-ink-3)" }}
            >
              Live · {paused ? "Paused" : "Recording"}
            </p>
            <h1
              className="mt-1 font-serif text-[28px] leading-tight"
              style={{ color: "var(--color-ink)", letterSpacing: "-0.02em" }}
            >
              {activeMeeting?.title ?? "Untitled meeting"}
            </h1>
          </div>
          <div
            className="font-mono text-3xl tabular-nums"
            style={{ color: "var(--color-ink-2)" }}
            aria-label={`Elapsed time ${formatElapsed(elapsedMs)}`}
          >
            {formatElapsed(elapsedMs)}
          </div>
        </header>

        {activeMeeting && activeMeeting.participants.length > 0 && (
          <section className="mb-6">
            <p
              className="mb-2 font-mono text-[10px] uppercase tracking-[0.12em]"
              style={{ color: "var(--color-ink-3)" }}
            >
              In the room
            </p>
            <div className="flex flex-wrap items-center gap-3">
              {activeMeeting.participants.map((p) => (
                <span
                  key={p.display_name}
                  className="inline-flex items-center gap-2 rounded-full border px-2 py-0.5 text-xs"
                  style={{
                    background: "var(--color-paper-2)",
                    borderColor: "var(--color-rule)",
                    color: "var(--color-ink-2)",
                  }}
                >
                  <Avatar name={p.display_name} size={16} />
                  {p.display_name}
                </span>
              ))}
            </div>
          </section>
        )}

        <TranscriptPane segments={segments} />

        <div className="mt-6 flex gap-3">
          <Button
            variant="outline"
            onClick={() => void handleTogglePause()}
            aria-pressed={paused}
            disabled={pauseToggling || stopTargetId === null}
            aria-busy={pauseToggling}
          >
            {pauseToggling
              ? paused
                ? "Resuming…"
                : "Pausing…"
              : paused
                ? "Resume"
                : "Pause"}
          </Button>
          <Button
            variant="destructive"
            onClick={() => void handleStop()}
            disabled={stopping || stopTargetId === null}
            aria-busy={stopping}
          >
            {stopping ? "Stopping…" : "Stop & save"}
          </Button>
        </div>
      </main>
    </>
  );
}

function TranscriptPane({ segments }: { segments: TranscriptSegment[] }) {
  if (segments.length === 0) {
    // TODO: replace with the real dBFS waveform now that the daemon
    // emits `audio.level` envelopes (Tier 3 #15 — the backend ships
    // the data via `useAudioLevelStore`; the renderer pass is a
    // pure-UI follow-up).
    return (
      <div
        className="rounded border px-6 py-12 text-center"
        style={{
          background: "var(--color-paper-2)",
          borderColor: "var(--color-rule)",
          color: "var(--color-ink-3)",
        }}
        role="status"
        aria-live="polite"
      >
        <p
          className="inline-flex items-center justify-center gap-2 font-mono text-[11px] uppercase tracking-[0.12em]"
          style={{ color: "var(--color-rec)" }}
        >
          <span
            aria-hidden="true"
            className="inline-block animate-[pulse-rec_1.4s_ease-in-out_infinite]"
            style={{
              width: 8,
              height: 8,
              borderRadius: "50%",
              background: "var(--color-rec)",
            }}
          />
          Capturing audio
        </p>
        <p
          className="mt-3 font-serif text-base"
          style={{ color: "var(--color-ink-2)" }}
        >
          Listening for the first words…
        </p>
        <p className="mt-1 text-xs" style={{ color: "var(--color-ink-4)" }}>
          Live captions appear here once WhisperKit emits the first
          transcript segment — typically 5–10 seconds in.
        </p>
      </div>
    );
  }
  return (
    <div
      className="overflow-hidden rounded border"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
      }}
    >
      <div
        className="max-h-[60vh] overflow-y-auto p-4"
        // Live regions are noisy with screen readers; the visible
        // ticker is enough for sighted users and the Review page
        // hosts the canonical transcript for AT users.
      >
        {segments.map((seg, i) => (
          <Line key={`${seg.start_secs}-${i}`} segment={seg} />
        ))}
      </div>
    </div>
  );
}

function Line({ segment }: { segment: TranscriptSegment }) {
  return (
    <div className="mb-3 flex gap-3">
      <Avatar name={segment.speaker.display_name} size={20} />
      <div className="min-w-0 flex-1">
        <div
          className="font-mono text-[10px] uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          {segment.speaker.display_name}
          <span className="ml-2">{formatStamp(segment.start_secs)}</span>
        </div>
        <p
          className={
            segment.is_final
              ? "text-sm leading-relaxed"
              : "text-sm leading-relaxed italic opacity-70"
          }
          style={{ color: "var(--color-ink)" }}
        >
          {segment.text}
        </p>
      </div>
    </div>
  );
}

function formatStamp(secs: number): string {
  const total = Math.max(0, Math.floor(secs));
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
}
