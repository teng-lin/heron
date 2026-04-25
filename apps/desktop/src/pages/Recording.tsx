/**
 * In-progress recording view — `/recording`.
 *
 * Phase 64 (PR-β) UI affordances only:
 *
 *   - Centred monospaced HH:MM:SS timer (driven by `recordingStart` in
 *     `store/recording.ts` + a `setInterval` tick).
 *   - Red blinking dot + "RECORDING · Microphone" label.
 *   - Pause button (UI-only stub — `togglePause` flips a flag we don't
 *     yet thread to the audio pipeline).
 *   - Stop & Save button — clears the recording state, navigates back
 *     to `/home`. Once `heron_stop_recording` exists the button will
 *     `invoke()` it before navigating; for now the TODO below carries
 *     the contract.
 *   - Waveform placeholder: an empty `bg-muted/20` block where the
 *     real WaveSurfer / `Visualizer` ships in phase 64.5.
 *
 * Layout aesthetic borrows from oh-my-whisper's `RecordingView.swift`:
 * dark accent, centred content, minimal chrome.
 */

import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";

import { Button } from "../components/ui/button";
import { useRecordingStore, formatElapsed } from "../store/recording";

export default function Recording() {
  const navigate = useNavigate();
  const recordingStart = useRecordingStore((s) => s.recordingStart);
  const paused = useRecordingStore((s) => s.paused);
  const togglePause = useRecordingStore((s) => s.togglePause);
  const stop = useRecordingStore((s) => s.stop);

  // Tick once per second to redraw the timer. We keep a `now` in
  // useState rather than reading `Date.now()` inside JSX so React
  // re-renders deterministically; the interval cleans itself up on
  // unmount.
  //
  // The Pause button is a stub: the label flips and `paused` is
  // tracked in the store, but the timer keeps incrementing because
  // the audio pipeline isn't yet wired through Tauri (so there's
  // nothing to actually pause). Once `heron_pause_recording` lands,
  // the FSM will own the elapsed-at-pause and this component will
  // read the freeze value from the backend.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const handle = setInterval(() => setNow(Date.now()), 1_000);
    return () => clearInterval(handle);
  }, []);

  // If a user lands on `/recording` directly without seeding the
  // store (e.g. a deep-link from the tray with no consent flow), bail
  // out to home. This keeps the timer from showing an obviously-stale
  // duration (or `Date.now() - 0`).
  useEffect(() => {
    if (recordingStart === null) {
      navigate("/home", { replace: true });
    }
  }, [recordingStart, navigate]);

  const elapsedMs = recordingStart === null ? 0 : now - recordingStart;

  const handleStop = () => {
    // TODO: once the Rust orchestrator exposes `heron_stop_recording`,
    // call it here via the typed `invoke()` wrapper before clearing
    // the local state. Today the audio pipeline isn't wired through
    // Tauri yet, so we just reset the UI.
    stop();
    navigate("/home");
  };

  return (
    <main className="min-h-screen bg-foreground text-background flex flex-col items-center justify-center gap-8 px-6 py-10">
      <div className="flex items-center gap-3 text-sm uppercase tracking-[0.2em] text-background/70">
        <span
          aria-hidden="true"
          className={
            "inline-block h-2.5 w-2.5 rounded-full bg-destructive " +
            (paused ? "" : "animate-pulse")
          }
        />
        <span>{paused ? "PAUSED · Microphone" : "RECORDING · Microphone"}</span>
      </div>

      {/* No `aria-live` — that would force a screen-reader
          announcement every second once the value updates. The
          implicit reading on focus + the "RECORDING" label above is
          sufficient for AT users; sighted users have the visible
          digits. */}
      <div
        className="font-mono text-6xl tabular-nums tracking-wide"
        aria-label={`Elapsed time ${formatElapsed(elapsedMs)}`}
      >
        {formatElapsed(elapsedMs)}
      </div>

      {/* TODO: waveform — phase 64.5 */}
      <div
        className="h-20 w-full max-w-md rounded-md bg-muted/20"
        aria-hidden="true"
      />

      <div className="flex gap-3">
        <Button
          variant="outline"
          onClick={togglePause}
          // The text + aria-pressed flip together; screen readers
          // announce the new state on click.
          aria-pressed={paused}
        >
          {paused ? "Resume" : "Pause"}
        </Button>
        <Button variant="destructive" onClick={handleStop}>
          Stop &amp; Save
        </Button>
      </div>
    </main>
  );
}
