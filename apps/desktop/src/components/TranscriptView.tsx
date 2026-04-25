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
 * For PR-γ this is read-only — edits happen in the TipTap editor
 * above. PR-γ′ (audio playback) will wire each row's clock to a
 * seek into the recording.
 */

import {
  groupBySpeaker,
  parseTranscriptLines,
  speakerColor,
  speakerInitial,
} from "../lib/transcript";

interface TranscriptViewProps {
  /** Raw markdown body. We re-parse on every change rather than
   * caching — the document is small (~hundreds of lines max). */
  markdown: string;
}

export function TranscriptView({ markdown }: TranscriptViewProps) {
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
                <span className="font-semibold text-foreground">
                  {g.speaker}
                </span>
                <span>{g.startTime}</span>
              </div>
              <p className="text-sm whitespace-pre-wrap">{g.combinedText}</p>
            </div>
          </div>
        );
      })}
    </div>
  );
}
