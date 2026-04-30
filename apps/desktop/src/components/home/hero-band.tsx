/**
 * Editorial hero for the Home page.
 *
 * The "Patient. Watchful. Local." display copy and the right-aligned
 * three-stat strip come straight from the prototype's
 * `view-home.jsx`. Eyebrow tracks the active companion mode so a user
 * who has flipped to Athena/Pollux sees `HERON · ATHENA MODE` instead
 * of the bare `Library` header that this band replaces.
 *
 * Stats are passed in by the caller — the band stays presentational so
 * the same component renders against live store values in `Home.tsx`
 * and against fixed mocks in `Styleguide.tsx` without a parallel data
 * path.
 */

import { useActiveMode } from "../../store/mode";

import { HeronMark } from "../ui/heron-mark";

import type { ActiveMode } from "../../lib/invoke";

interface HeroBandProps {
  /** Total meetings in the local library. */
  meetingsCount: number;
  /** Hours captured this calendar period — caller decides the window. */
  hoursCaptured: number;
  /**
   * Audio-uploaded count. Heron's product posture is "audio never
   * leaves the machine," so this is `0` today. The shape is a number
   * (rather than a hardcoded `"0"`) so a future opt-in upload setting
   * can flip it without touching this component.
   */
  audioUploaded: number;
}

const MODE_LABEL: Record<ActiveMode, string> = {
  clio: "CLIO",
  athena: "ATHENA",
  pollux: "POLLUX",
};

export function HeroBand({
  meetingsCount,
  hoursCaptured,
  audioUploaded,
}: HeroBandProps) {
  const mode = useActiveMode();
  return (
    <section
      className="border-b px-14 pt-10 pb-6"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
      }}
    >
      <div className="flex flex-wrap items-end gap-9">
        <div className="min-w-0 flex-1 basis-[460px] max-w-[620px]">
          <p
            className="mb-3.5 inline-flex items-center gap-2 font-mono text-[10.5px] uppercase tracking-[0.12em]"
            style={{ color: "var(--color-ink-3)" }}
          >
            <HeronMark size={11} />
            <span>HERON · {MODE_LABEL[mode]} MODE</span>
          </p>
          <h1
            className="m-0 font-serif text-[36px] font-normal leading-[1.1]"
            style={{ color: "var(--color-ink)", letterSpacing: "-0.02em" }}
          >
            Patient. Watchful.{" "}
            <em
              className="not-italic"
              style={{ color: "var(--color-accent)", fontStyle: "italic" }}
            >
              Local.
            </em>
          </h1>
          <p
            className="mt-3 max-w-[520px] text-[13.5px] leading-[1.55]"
            style={{ color: "var(--color-ink-2)" }}
          >
            Your meetings record on this machine. Audio never leaves.
            Transcripts go to your vault as plain markdown — yours to keep,
            search, and forget.
          </p>
        </div>

        <div className="grid grid-cols-3 gap-7 pb-1">
          <Stat
            label="meetings"
            value={formatInteger(meetingsCount)}
            sub="this month"
          />
          <Stat
            label="hours captured"
            value={formatHours(hoursCaptured)}
            sub="all local"
          />
          <Stat
            label="audio uploaded"
            value={formatInteger(audioUploaded)}
            sub="never"
            accent
          />
        </div>
      </div>
    </section>
  );
}

function Stat({
  label,
  value,
  sub,
  accent,
}: {
  label: string;
  value: string;
  sub: string;
  accent?: boolean;
}) {
  return (
    <div>
      <div
        className="mb-1 font-mono text-[9.5px] uppercase tracking-[0.12em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        {label}
      </div>
      <div
        className="font-serif text-[28px] font-normal leading-none"
        style={{
          color: accent ? "var(--color-accent)" : "var(--color-ink)",
          fontVariantNumeric: "tabular-nums",
        }}
      >
        {value}
      </div>
      <div
        className="mt-1 font-mono text-[10px] tracking-[0.04em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        {sub}
      </div>
    </div>
  );
}

function formatInteger(n: number): string {
  // Guard against `NaN` / `Infinity` so a future computed stat
  // doesn't render literal "NaN" in the hero. `Math.round(NaN)` is
  // `NaN` and `(NaN).toLocaleString()` returns `"NaN"`.
  if (!Number.isFinite(n)) return "0";
  return Math.round(n).toLocaleString();
}

/**
 * Hours with one decimal — `38.2` reads more honest than `38` for a
 * stat that's near the round number, and keeps trailing zero on
 * round-hour values (`24.0`) so the column doesn't visually jitter as
 * the user records more meetings.
 */
function formatHours(hours: number): string {
  if (!Number.isFinite(hours) || hours <= 0) return "0";
  return hours.toFixed(1);
}
