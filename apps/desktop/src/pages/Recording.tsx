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

import { useEffect, useMemo, useRef, useState } from "react";
import { Mic } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { DaemonDownBanner } from "../components/DaemonDownBanner";
import { Avatar } from "../components/ui/avatar";
import { Button } from "../components/ui/button";
import { invoke } from "../lib/invoke";
import type { Meeting, MeetingId, TranscriptSegment } from "../lib/types";
import { useAudioLevelStore } from "../store/audioLevel";
import { useMeetingsStore } from "../store/meetings";
import { formatElapsed, useRecordingStore } from "../store/recording";
import { useSettingsStore } from "../store/settings";
import { useSpeakerStore } from "../store/speaker";
import { useTranscriptStore } from "../store/transcript";

// Channels exposed to the renderer. Raw `Mic` is intentionally
// not surfaced (Tier 3 #15 — only `mic_clean` after the chain).
type LiveChannel = "mic_clean" | "tap";

const CHANNEL_LABELS: Record<LiveChannel, string> = {
  mic_clean: "Microphone",
  tap: "System audio",
};

// Depth of the rolling waveform / equaliser history. ~6 seconds at the
// daemon's ~20 Hz envelope cadence — enough motion to look alive
// without making the canvas scroll feel slow.
const HISTORY_DEPTH = 120;
// Number of equaliser bars rendered. Coarser than the waveform so the
// bars stay legibly wide at this card width.
const EQ_BARS = 24;
// dBFS values are floored at -100 by the daemon; clamp the renderer
// to the same range so a missing reading doesn't blow up the meter.
const DBFS_FLOOR = -100;
const DBFS_CEIL = 0;

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
  const pauseTogglingRef = useRef(false);

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

  // Resolve the meeting whose live signals (audio levels, active
  // speaker) we should bind to. Same fallback chain as `stopTargetId`
  // below — prefer the id we got back from `heron_start_capture`,
  // then fall back to the meetings store's active meeting (CLI /
  // detector path).
  const liveMeetingId: MeetingId | null =
    recordingMeetingId ?? activeMeeting?.id ?? null;
  const activeSpeaker = useSpeakerStore((s) =>
    liveMeetingId ? (s.activeByMeeting[liveMeetingId] ?? null) : null,
  );

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
    if (pauseTogglingRef.current) return;
    if (stopTargetId === null) {
      toast.error("No active meeting to pause.");
      return;
    }
    pauseTogglingRef.current = true;
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
      pauseTogglingRef.current = false;
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
            {activeSpeaker !== null && (
              <NowSpeakingPill name={activeSpeaker} />
            )}
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

        {liveMeetingId !== null && (
          <LiveMeterPanel meetingId={liveMeetingId} paused={paused} />
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
    // The live dBFS meter / waveform / equaliser panel renders just
    // above this pane (see <LiveMeterPanel /> in Recording above), so
    // the empty-transcript state stays a small typographic pulse —
    // the audio-presence affordance lives in the meter, not here.
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

/**
 * Compact "Now speaking" badge driven by `useSpeakerStore`. Hidden by
 * the parent when the store has no active speaker for the meeting —
 * this component is intentionally unconditional so the AX caveat
 * (mute-state proxy, not a true speaker frame) is documented in one
 * place. We label it "speaking" rather than "potentially speaking"
 * because the daemon-side store already drops `started=false` /
 * cleared the slot when the active speaker mutes; a labelled name in
 * the badge means "AX last reported them as the unmuted talker."
 */
function NowSpeakingPill({ name }: { name: string }) {
  return (
    <div className="mt-2">
      <span
        className="inline-flex items-center gap-2 rounded-full border px-2.5 py-1 text-xs"
        style={{
          background: "var(--color-paper-2)",
          borderColor: "var(--color-rule)",
          color: "var(--color-ink-2)",
        }}
        aria-live="polite"
      >
        <span
          aria-hidden="true"
          className="inline-block animate-[pulse-rec_1.4s_ease-in-out_infinite]"
          style={{
            width: 6,
            height: 6,
            borderRadius: "50%",
            background: "var(--color-rec)",
          }}
        />
        <span
          className="font-mono text-[10px] uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          Now speaking
        </span>
        <Avatar name={name} size={16} />
        <span className="font-medium" style={{ color: "var(--color-ink)" }}>
          {name}
        </span>
      </span>
    </div>
  );
}

/**
 * Per-channel rolling history of `audio.level` envelopes. We sample
 * only the latest envelope per channel — the daemon already coalesces
 * to ~20 Hz max-peak / max-rms inside the tick window, so pushing each
 * fresh value into a fixed-length ring is plenty for a sparkline.
 *
 * Returns `peak` (waveform / EQ source) and the latest reading
 * (numeric meter source). `peak` is a plain array — copies on every
 * push, ~120 numbers per channel; cheap enough that a ring-buffer
 * implementation isn't worth the complexity at this depth.
 */
function useAudioLevelHistory(meetingId: MeetingId, channel: LiveChannel) {
  const latest = useAudioLevelStore(
    (s) => s.latestByMeeting[meetingId]?.[channel] ?? null,
  );
  const [history, setHistory] = useState<number[]>(() => []);

  useEffect(() => {
    // Reset the ring whenever the meeting flips so a stale tail from a
    // previous session doesn't bleed into the new sparkline.
    setHistory([]);
  }, [meetingId, channel]);

  useEffect(() => {
    if (latest === null) return;
    setHistory((prev) => {
      const next = prev.length >= HISTORY_DEPTH ? prev.slice(1) : prev.slice();
      next.push(clampDbfs(latest.peak_dbfs));
      return next;
    });
  }, [latest]);

  return { latest, history };
}

function clampDbfs(value: number): number {
  if (Number.isNaN(value)) return DBFS_FLOOR;
  if (value < DBFS_FLOOR) return DBFS_FLOOR;
  if (value > DBFS_CEIL) return DBFS_CEIL;
  return value;
}

/** Map a dBFS value into a 0..1 fill ratio. -100 → 0, 0 → 1. */
function dbfsToRatio(value: number): number {
  const clamped = clampDbfs(value);
  return (clamped - DBFS_FLOOR) / (DBFS_CEIL - DBFS_FLOOR);
}

/**
 * Pick a colour for a dBFS reading. Loud (>-6 dBFS) is `--color-rec`
 * (clipping risk), warm signal (>-24 dBFS) is `--color-warn`, normal
 * speech sits at `--color-ok`, and a near-floor reading (no signal)
 * fades into `--color-ink-4` so a paused / silent capture doesn't
 * look identical to a hot one.
 */
function dbfsColor(value: number): string {
  if (value <= -90) return "var(--color-ink-4)";
  if (value > -6) return "var(--color-rec)";
  if (value > -24) return "var(--color-warn)";
  return "var(--color-ok)";
}

function formatDbfs(value: number | null): string {
  if (value === null) return "--";
  return `${clampDbfs(value).toFixed(1)} dB`;
}

function LiveMeterPanel({
  meetingId,
  paused,
}: {
  meetingId: MeetingId;
  paused: boolean;
}) {
  return (
    <section
      className="mb-6 rounded border"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
      }}
      aria-label="Live audio levels"
    >
      <div className="flex items-center justify-between px-4 pt-3">
        <p
          className="font-mono text-[10px] uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          Live signal
        </p>
        {paused && (
          <p
            className="font-mono text-[10px] uppercase tracking-[0.12em]"
            style={{ color: "var(--color-warn)" }}
          >
            Paused — daemon is not writing frames
          </p>
        )}
      </div>
      <div className="grid gap-4 px-4 py-3 md:grid-cols-2">
        <ChannelMeter meetingId={meetingId} channel="mic_clean" />
        <ChannelMeter meetingId={meetingId} channel="tap" />
      </div>
    </section>
  );
}

function ChannelMeter({
  meetingId,
  channel,
}: {
  meetingId: MeetingId;
  channel: LiveChannel;
}) {
  const { latest, history } = useAudioLevelHistory(meetingId, channel);
  const peak = latest?.peak_dbfs ?? null;
  const rms = latest?.rms_dbfs ?? null;
  const peakColor = dbfsColor(peak ?? DBFS_FLOOR);
  const rmsColor = dbfsColor(rms ?? DBFS_FLOOR);

  return (
    <div
      className="rounded border p-3"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
      }}
    >
      <div className="mb-2 flex items-center justify-between">
        <p
          className="font-mono text-[10px] uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          {CHANNEL_LABELS[channel]}
        </p>
        <p
          className="font-mono text-[10px] tabular-nums"
          style={{ color: "var(--color-ink-3)" }}
          aria-label={`Peak ${formatDbfs(peak)}, RMS ${formatDbfs(rms)}`}
        >
          peak {formatDbfs(peak)} · rms {formatDbfs(rms)}
        </p>
      </div>
      <DbfsBar label="peak" value={peak} color={peakColor} />
      <div className="h-1" />
      <DbfsBar label="rms" value={rms} color={rmsColor} />
      <div className="mt-3">
        <Equalizer history={history} />
      </div>
      <div className="mt-3">
        <Waveform history={history} />
      </div>
    </div>
  );
}

function DbfsBar({
  label,
  value,
  color,
}: {
  label: string;
  value: number | null;
  color: string;
}) {
  const ratio = value === null ? 0 : dbfsToRatio(value);
  return (
    <div
      className="relative h-2 overflow-hidden rounded"
      style={{ background: "var(--color-paper-3)" }}
      role="meter"
      aria-label={label}
      aria-valuemin={DBFS_FLOOR}
      aria-valuemax={DBFS_CEIL}
      aria-valuenow={value === null ? undefined : clampDbfs(value)}
    >
      <div
        className="h-full transition-[width] duration-75 ease-out"
        style={{ width: `${ratio * 100}%`, background: color }}
      />
    </div>
  );
}

/**
 * Animated bar-graph EQ. We don't have an FFT — the daemon emits a
 * single coalesced peak per tick — so we synthesise an EQ-feel by
 * sampling the trailing `EQ_BARS` peaks and rendering each as a
 * vertical column. Read it as "the recent shape of the signal,"
 * not as a frequency-domain breakdown.
 */
function Equalizer({ history }: { history: number[] }) {
  const recent = history.slice(-EQ_BARS);
  // Pad on the left so a fresh meter doesn't render as a single right-
  // aligned spike — the bars grow in from the right as samples arrive.
  const padded =
    recent.length < EQ_BARS
      ? [...new Array<number>(EQ_BARS - recent.length).fill(DBFS_FLOOR), ...recent]
      : recent;
  return (
    <div
      className="flex h-8 items-end gap-[2px]"
      aria-hidden="true"
    >
      {padded.map((value, i) => {
        const ratio = dbfsToRatio(value);
        return (
          <div
            key={i}
            className="flex-1 rounded-sm transition-[height] duration-75 ease-out"
            style={{
              height: `${Math.max(2, ratio * 100)}%`,
              background: dbfsColor(value),
              opacity: value <= DBFS_FLOOR + 1 ? 0.2 : 1,
            }}
          />
        );
      })}
    </div>
  );
}

/**
 * Sparkline-style waveform. Plots the last `HISTORY_DEPTH` peak
 * readings as an SVG polyline. Not a forensic visualiser — just
 * enough motion to confirm "audio is flowing on this channel."
 */
function Waveform({ history }: { history: number[] }) {
  const width = 320;
  const height = 36;
  if (history.length < 2) {
    return (
      <svg
        viewBox={`0 0 ${width} ${height}`}
        preserveAspectRatio="none"
        className="block h-9 w-full"
        aria-hidden="true"
      >
        <line
          x1={0}
          y1={height / 2}
          x2={width}
          y2={height / 2}
          stroke="var(--color-rule)"
          strokeWidth={1}
        />
      </svg>
    );
  }
  const stepX = width / (HISTORY_DEPTH - 1);
  // Right-align so the freshest sample is always at the right edge,
  // matching how meters typically scroll.
  const offset = HISTORY_DEPTH - history.length;
  const points = history
    .map((value, i) => {
      const x = (i + offset) * stepX;
      const y = height - dbfsToRatio(value) * height;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  const last = history[history.length - 1] ?? DBFS_FLOOR;
  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      className="block h-9 w-full"
      aria-hidden="true"
    >
      <polyline
        points={points}
        fill="none"
        stroke={dbfsColor(last)}
        strokeWidth={1.25}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}

