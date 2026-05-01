/**
 * "What's happening" right-rail checklist for the Recording page —
 * a three-row honest status of where the user's audio is in the
 * pipeline. Rows:
 *
 *   1. Audio captured via Core Audio process tap.
 *   2. Transcribed locally on Apple Neural Engine.
 *   3. Summary generated when you stop. (Deferred / future.)
 *
 * Pure presentational — the parent computes the booleans from
 * `useAudioLevelStore` (any envelope received & not paused) and
 * `useTranscriptStore` (at least one segment present). The third
 * row is always rendered as a deferred dot because summarization
 * only fires post-stop.
 */

interface StatusChecklistProps {
  audioCaptured: boolean;
  transcribed: boolean;
}

export function StatusChecklist({
  audioCaptured,
  transcribed,
}: StatusChecklistProps) {
  return (
    <section aria-label="What's happening">
      <p
        className="mb-2.5 font-mono text-[10px] uppercase tracking-[0.12em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        What&rsquo;s happening
      </p>
      <ul className="flex flex-col text-[12.5px] leading-[1.6]">
        <Row
          ok={audioCaptured}
          label="Audio captured via Core Audio process tap"
        />
        <Row
          ok={transcribed}
          // STT backend is configurable (`Settings.stt_backend`), so
          // claiming "Apple Neural Engine" specifically would lie when
          // the user has switched to a different transcriber. Stay
          // backend-neutral; the existence of a transcript segment is
          // what matters at this surface.
          label="Transcribed locally"
        />
        <Row
          ok={false}
          deferred
          // "can be" rather than "will be" — auto_summarize is opt-in
          // and the summarizer can fail or be skipped. The honest
          // posture is "this can happen after stop, not that it
          // definitely will."
          label="Summary can be generated after stop"
        />
      </ul>
    </section>
  );
}

function Row({
  ok,
  deferred,
  label,
}: {
  ok: boolean;
  deferred?: boolean;
  label: string;
}) {
  // Three states: deferred (future, ◯ ink-4) · waiting (◯ warn) · ok (● ok).
  // Waiting fires when a row that should be live isn't yet — e.g., the
  // transcript pipeline hasn't emitted its first segment. The same dot
  // shape (◯) reads as "not yet" honestly, while the colour shifts.
  const state = deferred ? "deferred" : ok ? "ok" : "waiting";
  const dot = state === "ok" ? "●" : "○";
  const color =
    state === "ok"
      ? "var(--color-ok)"
      : state === "waiting"
        ? "var(--color-warn)"
        : "var(--color-ink-4)";
  // Map state → screen-reader label suffix. The visible glyph is
  // `aria-hidden`, so without an explicit textual status a screen
  // reader would hear only the row label and have no idea whether
  // the step is complete, in flight, or deferred.
  const srStatus =
    state === "ok" ? "complete" : state === "waiting" ? "waiting" : "pending";
  return (
    <li
      className="flex items-baseline gap-2 py-1.5"
      style={{
        color: state === "deferred" ? "var(--color-ink-3)" : "var(--color-ink-2)",
      }}
    >
      <span
        aria-hidden="true"
        className="font-mono text-[12px]"
        style={{ color }}
      >
        {dot}
      </span>
      <span>
        {label}
        <span className="sr-only"> ({srStatus})</span>
      </span>
    </li>
  );
}
