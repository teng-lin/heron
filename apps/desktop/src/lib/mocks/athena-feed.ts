/**
 * Mock Athena feed lifted from `.design/data.jsx:115`. Lives here
 * (not in `lib/types.ts`) because it's a pure UI fixture for the
 * Athena stub page — there is no daemon-side AthenaFeed yet.
 *
 * When Athena ships (week ~11 per `docs/heron-implementation.md`)
 * the daemon will publish suggestion events on the bus and this
 * fixture goes away.
 */

export type AthenaSuggestionKind = "fact" | "reply" | "flag";

export interface AthenaSuggestion {
  kind: AthenaSuggestionKind;
  /** Timestamp like `"00:01:09"`. */
  at: string;
  title: string;
  body: string;
  source?: string;
}

export const ATHENA_FEED: AthenaSuggestion[] = [
  {
    kind: "fact",
    at: "00:01:09",
    title: "From your vault — HAL plug-in notes",
    body: 'Mar 14 architecture doc: "Two-installer approach was rejected on UX grounds in v0.3 planning. The split-signing concern was raised but deferred."',
    source: "vault/heron/03-14 arch.md",
  },
  {
    kind: "reply",
    at: "00:01:24",
    title: "Suggested reply",
    body: "Apple's CoreAudio extension docs do allow daemon-driven plug-in updates without re-prompting for kernel access — last clarified in the WWDC23 audio session.",
  },
  {
    kind: "flag",
    at: "00:01:52",
    title: "Trigger: external dependency",
    body: "Iris flagged a blocker on legal sign-off for Pollux consent copy. This is outside your briefing — consider scheduling a follow-up.",
  },
  {
    kind: "fact",
    at: "00:02:14",
    title: "Calendar — Friday",
    body: "Your Friday is currently 60% booked. Three open slots in the morning if you need to follow up with Jonas.",
  },
];
