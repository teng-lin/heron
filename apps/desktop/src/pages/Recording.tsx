/**
 * In-progress recording view — `/recording`.
 *
 * UI revamp PR 4: shows the live transcript pane (driven by SSE
 * events through `useTranscriptStore`) plus participants from the
 * currently-recording meeting (if any). The chrome's REC pill in
 * the TitleBar replaces the page-level "RECORDING · Microphone"
 * label that lived here before.
 *
 * Recording capture itself is not yet wired from the desktop UI
 * (Gap #7 in `docs/archives/codebase-gaps.md`); when the daemon
 * emits `meeting.started` for any path (CLI start, future Gap #7
 * resolution), this page populates from the SSE stream.
 */

import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";

import { DaemonDownBanner } from "../components/DaemonDownBanner";
import { Avatar } from "../components/ui/avatar";
import { Button } from "../components/ui/button";
import type { Meeting, TranscriptSegment } from "../lib/types";
import { useMeetingsStore } from "../store/meetings";
import { formatElapsed, useRecordingStore } from "../store/recording";
import { useTranscriptStore } from "../store/transcript";

export default function Recording() {
  const navigate = useNavigate();
  const recordingStart = useRecordingStore((s) => s.recordingStart);
  const paused = useRecordingStore((s) => s.paused);
  const togglePause = useRecordingStore((s) => s.togglePause);
  const stop = useRecordingStore((s) => s.stop);

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
  const segments = useTranscriptStore((s) =>
    activeMeeting ? (s.segments[activeMeeting.id] ?? []) : [],
  );

  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (recordingStart === null) return;
    const id = setInterval(() => setNow(Date.now()), 1_000);
    return () => clearInterval(id);
  }, [recordingStart]);

  // Deeplink case: user lands on /recording with no local
  // recordingStart. We need to wait for the meetings store to resolve
  // before deciding to redirect — otherwise we'd race the SSE-driven
  // load and bounce them home moments before activeMeeting populates.
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

  // If neither the local timer nor a daemon-side meeting is active,
  // bail back to home so the user can start a recording from there.
  useEffect(() => {
    if (recordingStart !== null) return;
    if (!meetingsSettled) return;
    if (activeMeeting === null) {
      navigate("/home", { replace: true });
    }
  }, [recordingStart, meetingsSettled, activeMeeting, navigate]);

  const elapsedMs =
    recordingStart === null ? 0 : Math.max(0, now - recordingStart);

  const handleStop = () => {
    // TODO Gap #7: invoke `heron_stop_recording` once it exists.
    stop();
    navigate("/home");
  };

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
          <Button variant="outline" onClick={togglePause} aria-pressed={paused}>
            {paused ? "Resume" : "Pause"}
          </Button>
          <Button variant="destructive" onClick={handleStop}>
            Stop &amp; save
          </Button>
        </div>
      </main>
    </>
  );
}

function TranscriptPane({ segments }: { segments: TranscriptSegment[] }) {
  if (segments.length === 0) {
    return (
      <div
        className="rounded border px-6 py-12 text-center"
        style={{
          background: "var(--color-paper-2)",
          borderColor: "var(--color-rule)",
          color: "var(--color-ink-3)",
        }}
      >
        <p
          className="font-serif text-lg"
          style={{ color: "var(--color-ink-2)" }}
        >
          Listening…
        </p>
        <p className="mt-1 text-xs">
          Live captions appear here once the daemon emits transcript
          segments.
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
