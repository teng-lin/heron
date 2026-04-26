/**
 * Render the transcript section of a Heron `.md` body.
 *
 * The grouping algorithm is a port of @vexaai/transcript-rendering's
 * `groupSegments` (Apache-2.0) — see
 * `apps/desktop/src/lib/transcript.ts` for the attribution and
 * `apps/desktop/THIRD_PARTY_NOTICES.md` for the long-form notice.
 *
 * Each speaker run renders as a row with:
 * - a colored avatar bubble showing the speaker's initial(s)
 * - the speaker name + the run's start clock
 * - the joined text
 *
 * For PR-γ this was read-only. PR-ε (phase 67) adds an optional
 * `onSeek` callback — when provided, the row's clock becomes a
 * keyboard-focusable button that seeks the playback bar to that
 * timestamp. The grouping logic itself is unchanged.
 */

import { cn } from "../lib/cn";
import {
  groupBySpeaker,
  parseClockToSeconds,
  parseTranscriptLines,
  speakerColor,
  speakerInitial,
} from "../lib/transcript";

interface TranscriptViewProps {
  /** Raw markdown body. We re-parse on every change rather than
   * caching — the document is small (~hundreds of lines max). */
  markdown: string;
  /**
   * Optional click-to-seek callback. When omitted (PR-γ behavior),
   * the clock renders as plain text. When provided (PR-ε), the clock
   * renders as a focusable button and clicking it calls
   * `onSeek(seconds)` so the parent can move the audio playhead.
   *
   * Receives the timestamp in seconds, parsed via
   * [`parseClockToSeconds`] from the row's leading clock string.
   * Malformed clocks (regex didn't match) are treated as
   * unclickable — the parent never sees a `NaN`.
   */
  onSeek?: (seconds: number) => void;
}

export function TranscriptView({ markdown, onSeek }: TranscriptViewProps) {
  const segments = parseTranscriptLines(markdown);
  if (segments.length === 0) {
    return (
      <div className="text-sm text-muted-foreground italic">
        No transcript lines found in this note.
      </div>
    );
  }
  const groups = groupBySpeaker(segments);
  return (
    <div className="space-y-4">
      {groups.map((g, i) => {
        const color = speakerColor(g.speaker);
        const initial = speakerInitial(g.speaker);
        const seekSeconds = onSeek ? parseClockToSeconds(g.startTime) : null;
        return (
          <div
            key={`${g.speaker}-${g.startTime}-${i}`}
            className="flex gap-3 items-start"
          >
            <div
              className="w-8 h-8 shrink-0 rounded-full flex items-center justify-center text-xs font-semibold text-foreground"
              style={{ background: color }}
              aria-hidden="true"
            >
              {initial}
            </div>
            <div className="min-w-0 flex-1">
              <div className="flex items-baseline gap-2 text-xs text-muted-foreground">
                <span
                  className={cn(
                    "font-semibold",
                    // §3.4: low-confidence speakers render italic + a
                    // muted color so the cue is visible without
                    // dimming the transcript text itself. The text
                    // body keeps the default foreground color (see
                    // `<p>` below) so the user can still read every
                    // word — only the speaker label is de-emphasized.
                    g.isLowConfidence
                      ? "italic text-muted-foreground"
                      : "text-foreground",
                  )}
                  title={
                    g.isLowConfidence
                      ? "Low-confidence speaker attribution"
                      : undefined
                  }
                >
                  {g.speaker}
                  {g.isLowConfidence && (
                    // Screen-reader-only annotation. `title` alone is
                    // not announced reliably across AT software, so
                    // we surface the cue as visually-hidden text the
                    // reader walks past after the speaker name. The
                    // leading space keeps speech natural ("Alice
                    // (low-confidence)"); the parens are punctuation,
                    // which most readers tone-shift on.
                    <span className="sr-only"> (low-confidence)</span>
                  )}
                </span>
                {onSeek && seekSeconds !== null ? (
                  <button
                    type="button"
                    onClick={() => onSeek(seekSeconds)}
                    className="font-mono tabular-nums hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary rounded-sm"
                    title={`Jump to ${g.startTime}`}
                    aria-label={`Jump audio playback to ${g.startTime}`}
                  >
                    {g.startTime}
                  </button>
                ) : (
                  <span className="font-mono tabular-nums">{g.startTime}</span>
                )}
              </div>
              <p className="text-sm whitespace-pre-wrap">{g.combinedText}</p>
            </div>
          </div>
        );
      })}
    </div>
  );
}
