// Algorithm adapted from @vexaai/transcript-rendering, © Vexa AI, Apache-2.0
// Source: https://github.com/Vexa-ai/vexa/tree/main/packages/transcript-rendering
//
// We port the speaker-grouping algorithm rather than vendor the npm package
// because Heron writes the transcript into a static `.md` after summarize —
// no WebSocket dedup / two-map state needed. The algorithm we keep is
// `groupSegments`: walk segments in order, merge consecutive same-speaker
// entries, split overlong groups at segment boundaries.
//
// The exact license text from `@vexaai/transcript-rendering`'s LICENSE
// file is reproduced under apps/desktop/THIRD_PARTY_NOTICES.md.

/**
 * One transcript line parsed from a Heron `.md` body.
 *
 * `time` is the absolute clock string as written in the .md
 * (`HH:MM:SS`); we keep it as text for display and do not need a
 * `Date` because we only render — we do not dedup.
 *
 * `is_low_confidence` is the §3.4 UI rule expressed against the
 * markdown body: when the rendered .md only carries the speaker name
 * (not the full JSONL `speaker_source` / `confidence` triple), the
 * canonical low-confidence marker is `speaker == "them"` — the
 * channel-fallback name produced by the Zoom aligner when AX didn't
 * fire or overlap-confidence was below the §3.4 floor. We default to
 * `false` for all other speakers since we can't observe `speaker_source`
 * here. PR-κ documents this trade-off; richer channel-fallback signal
 * (e.g. emitting `speaker_source` into the body) is a future PR.
 */
export interface TranscriptSegment {
  time: string;
  speaker: string;
  text: string;
  is_low_confidence: boolean;
}

/**
 * Consecutive same-speaker run.
 *
 * `is_low_confidence` is true when at least
 * `LOW_CONFIDENCE_GROUP_FRACTION` of the run's segments are
 * low-confidence. We pick a 50% majority because a single stray
 * channel-fallback inside an otherwise AX-confident run shouldn't
 * italicize the whole bubble, but a run that's mostly `them` should
 * read as low-confidence at a glance.
 */
export interface SpeakerGroup {
  speaker: string;
  /** First-segment timestamp, used as the row's clock label. */
  startTime: string;
  /** All segments in this run, joined by a space when rendered. */
  segments: TranscriptSegment[];
  /** Pre-joined text for convenience; respects the maxChars split. */
  combinedText: string;
  /**
   * Whether the run as a whole should render in italics per §3.4.
   * Computed by [`groupBySpeaker`] from the underlying segments.
   */
  isLowConfidence: boolean;
}

const TRANSCRIPT_LINE = /^>\s+(\d{1,2}:\d{2}(?::\d{2})?)\s+([^:]+?):\s*(.*)$/;
const DEFAULT_MAX_CHARS = 512;
/**
 * Fraction of low-confidence segments needed to flip a [`SpeakerGroup`]
 * into the italicized rendering. ≥0.5 reads as "majority"; we use `≥`
 * (not `>`) so a single-segment `them` group still italicizes.
 */
export const LOW_CONFIDENCE_GROUP_FRACTION = 0.5;
/**
 * Canonical channel-fallback speaker name from `crates/heron-zoom`'s
 * aligner — turns where AX didn't fire (or fell below the §3.4
 * confidence floor) are rewritten to `speaker = "them"`. Compared
 * case-insensitively because the markdown body is human-edited and a
 * stray capitalization shouldn't suppress the low-confidence cue.
 */
const CHANNEL_FALLBACK_SPEAKER = "them";

/**
 * Apply the §3.4 channel-fallback heuristic to a single speaker name.
 *
 * Exposed so tests and the sidebar can reuse the same rule without
 * re-deriving it.
 */
export function isLowConfidenceSpeaker(name: string): boolean {
  return name.trim().toLowerCase() === CHANNEL_FALLBACK_SPEAKER;
}

/**
 * Parse `> HH:MM:SS Speaker: text` lines from a markdown body.
 *
 * Lines that don't match are dropped. The regex tolerates either
 * `MM:SS` or `HH:MM:SS` so transcripts shorter than an hour still
 * render. Speaker names with colons (rare) are not supported — the
 * first colon is the separator.
 */
export function parseTranscriptLines(markdown: string): TranscriptSegment[] {
  const out: TranscriptSegment[] = [];
  for (const raw of markdown.split("\n")) {
    const line = raw.trimEnd();
    const m = TRANSCRIPT_LINE.exec(line);
    if (!m) continue;
    const [, time, speaker, text] = m;
    const trimmedText = text.trim();
    if (!trimmedText) continue;
    const trimmedSpeaker = speaker.trim();
    out.push({
      time,
      speaker: trimmedSpeaker,
      text: trimmedText,
      is_low_confidence: isLowConfidenceSpeaker(trimmedSpeaker),
    });
  }
  return out;
}

/**
 * Group consecutive same-speaker segments.
 *
 * Mirrors `groupSegments` from @vexaai/transcript-rendering:
 * - walk in encounter order (the .md is already chronological)
 * - merge runs of the same speaker into one group
 * - split a group when the combined text would exceed `maxChars`,
 *   so a long monologue renders as multiple bubbles instead of one
 *   wall of text.
 */
export function groupBySpeaker(
  segments: TranscriptSegment[],
  maxChars: number = DEFAULT_MAX_CHARS,
): SpeakerGroup[] {
  const groups: SpeakerGroup[] = [];
  let current: SpeakerGroup | null = null;
  for (const seg of segments) {
    if (current && current.speaker === seg.speaker) {
      const candidate = current.combinedText
        ? `${current.combinedText} ${seg.text}`
        : seg.text;
      if (candidate.length > maxChars) {
        groups.push(finalizeGroup(current));
        current = newGroup(seg);
      } else {
        current.segments.push(seg);
        current.combinedText = candidate;
      }
    } else {
      if (current) groups.push(finalizeGroup(current));
      current = newGroup(seg);
    }
  }
  if (current) groups.push(finalizeGroup(current));
  return groups;
}

function newGroup(seg: TranscriptSegment): SpeakerGroup {
  return {
    speaker: seg.speaker,
    startTime: seg.time,
    segments: [seg],
    combinedText: seg.text,
    // Recomputed in `finalizeGroup` once all segments are in.
    isLowConfidence: false,
  };
}

/**
 * Recompute `isLowConfidence` from the segment population. We do this
 * at finalize time (rather than incrementally) so the §3.4 majority
 * rule reflects the whole run; an early stray `them` segment in an
 * otherwise AX-confident run shouldn't italicize prematurely if the
 * group later overflows the maxChars threshold and gets split.
 */
function finalizeGroup(group: SpeakerGroup): SpeakerGroup {
  if (group.segments.length === 0) {
    group.isLowConfidence = false;
    return group;
  }
  const lowConfidenceCount = group.segments.reduce(
    (acc, seg) => acc + (seg.is_low_confidence ? 1 : 0),
    0,
  );
  group.isLowConfidence =
    lowConfidenceCount / group.segments.length >= LOW_CONFIDENCE_GROUP_FRACTION;
  return group;
}

/**
 * Stable color picker for a speaker name.
 *
 * Uses a small palette (8 entries) and a deterministic hash so the
 * same speaker always renders with the same accent. The palette is
 * Tailwind-friendly (uses oklch values from the design tokens) but
 * we return raw hex/oklch strings so callers can drop them straight
 * into a `style={{ background }}`.
 */
export function speakerColor(name: string): string {
  // Eight gentle hues at constant chroma + lightness so they read
  // legibly on both light and dark backgrounds.
  const palette = [
    "oklch(0.78 0.12 25)", // warm coral
    "oklch(0.78 0.12 80)", // amber
    "oklch(0.78 0.12 140)", // green
    "oklch(0.78 0.12 200)", // cyan
    "oklch(0.78 0.12 260)", // blue
    "oklch(0.78 0.12 310)", // magenta
    "oklch(0.78 0.12 0)", // red
    "oklch(0.78 0.12 60)", // gold
  ];
  let h = 0;
  for (let i = 0; i < name.length; i += 1) {
    h = (h * 31 + name.charCodeAt(i)) >>> 0;
  }
  return palette[h % palette.length];
}

/**
 * One- or two-character avatar initial for a speaker.
 *
 * Splits on whitespace and takes the first letter of the first one
 * or two tokens — `"Alice Smith"` → `AS`, `"Alice"` → `A`. Empty
 * names render as `?`.
 */
export function speakerInitial(name: string): string {
  const tokens = name.trim().split(/\s+/).filter(Boolean);
  if (tokens.length === 0) return "?";
  if (tokens.length === 1) return tokens[0].charAt(0).toUpperCase();
  return (tokens[0].charAt(0) + tokens[1].charAt(0)).toUpperCase();
}

/**
 * Parse a `HH:MM:SS` or `MM:SS` clock string into seconds.
 *
 * Returns `null` when the string is malformed so callers can ignore
 * stray timestamps without throwing — the click handler in the Review
 * route degrades to a no-op rather than an error toast.
 *
 * Used by PR-ε (phase 67) to wire transcript-row clicks → audio
 * playback seeks. Not used by the read-only transcript renderer (PR-γ),
 * which keeps the clock as text only.
 */
export function parseClockToSeconds(clock: string): number | null {
  const m = /^(\d{1,2}):(\d{2})(?::(\d{2}))?$/.exec(clock);
  if (!m) return null;
  const a = Number(m[1]);
  const b = Number(m[2]);
  const c = m[3] === undefined ? null : Number(m[3]);
  // Two-segment form is `MM:SS`; three-segment is `HH:MM:SS`.
  if (c === null) {
    return a * 60 + b;
  }
  return a * 3600 + b * 60 + c;
}
