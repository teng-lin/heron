/**
 * Right-rail participants list for the Recording page. Reads
 * `useSpeakerStore.activeByMeeting` to mark whichever participant
 * the daemon's AX bridge most recently flagged as the unmuted
 * talker. The active row gets a three-bar animated equalizer
 * (`@keyframes wave-bar` in styles.css) and a `speaking now`
 * sub-line; everyone else shows `listening`.
 *
 * AX caveat (carried from `useSpeakerStore`): Zoom's accessibility
 * tree exposes mute-state edges, not a true active-speaker frame.
 * In 1:1 calls the unmuted-remote heuristic is solid; in 3+ calls
 * it's a best-guess label. The component intentionally does NOT
 * say "potentially speaking" — the store already drops the active
 * slot on `started=false`, and the badge tracks the freshest edge.
 *
 * The active-speaker name match is case-insensitive + trimmed
 * because Zoom's display name and the meeting's persisted
 * participant list can drift on whitespace / capitalization (the
 * AX bridge reads the in-call display name, the persisted list
 * reads the ICS attendee).
 */

import { Avatar } from "../ui/avatar";
import type { Participant } from "../../lib/types";

interface ParticipantsRailProps {
  participants: Participant[];
  /** Active speaker name from `useSpeakerStore.activeByMeeting`, or `null`. */
  activeSpeaker: string | null;
}

export function ParticipantsRail({
  participants,
  activeSpeaker,
}: ParticipantsRailProps) {
  if (participants.length === 0) return null;
  const activeKey = activeSpeaker?.trim().toLowerCase() ?? null;
  return (
    <section aria-label="Participants">
      <p
        className="mb-3 font-mono text-[10px] uppercase tracking-[0.12em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        Participants
      </p>
      <ul className="flex flex-col">
        {participants.map((p, index) => {
          const isActive =
            activeKey !== null &&
            p.display_name.trim().toLowerCase() === activeKey;
          return (
            <li
              // `display_name` alone isn't unique — guests with the
              // same name (or two phones dialled in as "iPhone") would
              // collapse onto the same React key and lose their
              // active-speaker indicator on reorder. Compose with the
              // identifier-kind + index so duplicates stay distinct.
              key={`${p.display_name}::${p.identifier_kind}::${index}`}
              className="flex items-center gap-2.5 border-b py-2"
              style={{ borderColor: "var(--color-rule)" }}
            >
              <Avatar name={p.display_name} size={26} />
              <div className="min-w-0 flex-1">
                <div
                  className="truncate text-[13px] font-medium"
                  style={{ color: "var(--color-ink)" }}
                >
                  {p.display_name}
                </div>
                <div
                  className="font-mono text-[10.5px]"
                  style={{
                    color: isActive ? "var(--color-rec)" : "var(--color-ink-3)",
                  }}
                >
                  {isActive ? "speaking now" : "listening"}
                </div>
              </div>
              {isActive && <SpeakingEqualizer />}
            </li>
          );
        })}
      </ul>
    </section>
  );
}

/**
 * Three thin vertical bars that scale in counter-phase, driven by
 * the shared `wave-bar` keyframe. Hidden from `prefers-reduced-motion`
 * users — the `speaking now` text label already conveys the state.
 */
function SpeakingEqualizer() {
  return (
    <span
      aria-hidden="true"
      className="inline-flex h-3.5 items-center gap-px motion-reduce:hidden"
    >
      {[0, 1, 2].map((i) => (
        <span
          key={i}
          className="inline-block animate-[wave-bar_0.8s_ease-in-out_infinite]"
          style={{
            width: 2,
            height: 14,
            background: "var(--color-rec)",
            transformOrigin: "center",
            animationDelay: `${i * 0.15}s`,
          }}
        />
      ))}
    </span>
  );
}
