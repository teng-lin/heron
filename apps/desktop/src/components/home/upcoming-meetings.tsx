/**
 * "Coming up" band for the Home page.
 *
 * Reads `useCalendarStore` (which proxies `GET /v1/calendar/upcoming`
 * via Tauri) and renders the user's next ~7-day window. The first
 * imminent event renders as a "featured" card — big date stamp +
 * serif title + primed badge + Record now / View prep notes — and
 * the rest fall through to compact rows with a per-event Auto-record
 * toggle.
 *
 * Each row exposes a "Start with context" affordance: the parent owns
 * the actual orchestration (attach_context + start_capture + navigate)
 * so the consent / disk-check pre-flight stays in one place. This
 * component is purely presentational + a single onClick callback.
 *
 * Calendar permission is the daemon's responsibility — when EventKit
 * is denied the proxy collapses to `unavailable` and the store flips
 * `daemonDown`. The shared `<DaemonDownBanner />` already covers that
 * case; the band still renders an inline error here so a
 * calendar-only failure (the meetings store may be fine) is visible.
 */

import { useEffect, useMemo, useState, type ReactNode } from "react";
import { CalendarDays, Mic, NotebookText, Sparkles } from "lucide-react";
import { toast } from "sonner";

import { Avatar } from "../ui/avatar";
import { Button } from "../ui/button";
import { Switch } from "../ui/switch";
import { useCalendarStore } from "../../store/calendar";
import type { AttendeeContext, CalendarEvent } from "../../lib/types";

interface ComingUpBandProps {
  /** Fired when the user clicks "Record now" / "Start with context". */
  onStartWithContext: (event: CalendarEvent) => void;
  /** Disable per-row Start buttons (e.g., a recording-start is in flight). */
  disabled?: boolean;
}

export function ComingUpBand({
  onStartWithContext,
  disabled,
}: ComingUpBandProps) {
  const items = useCalendarStore((s) => s.items);
  const loading = useCalendarStore((s) => s.loading);
  const daemonDown = useCalendarStore((s) => s.daemonDown);
  const error = useCalendarStore((s) => s.error);
  const load = useCalendarStore((s) => s.load);
  const ensureFresh = useCalendarStore((s) => s.ensureFresh);
  const setEventAutoRecord = useCalendarStore((s) => s.setEventAutoRecord);
  const [pendingAutoRecordIds, setPendingAutoRecordIds] = useState<Set<string>>(
    () => new Set(),
  );

  useEffect(() => {
    void ensureFresh();
  }, [ensureFresh]);

  // Refetch on window focus — calendar state can change while the app
  // is backgrounded (a quick edit in Calendar.app, a colleague adding
  // an attendee). The TTL guard inside `ensureFresh` keeps this cheap
  // when the user is just alt-tabbing rapidly.
  useEffect(() => {
    const onFocus = () => {
      void ensureFresh();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [ensureFresh]);

  const rows = useMemo(() => filterImminent(items), [items]);

  async function onAutoRecordChange(evt: CalendarEvent, enabled: boolean) {
    setPendingAutoRecordIds((ids) => new Set(ids).add(evt.id));
    let ok = false;
    try {
      ok = await setEventAutoRecord(evt.id, enabled);
    } finally {
      setPendingAutoRecordIds((ids) => {
        const next = new Set(ids);
        next.delete(evt.id);
        return next;
      });
    }
    if (!ok) {
      toast.error(
        `Could not ${enabled ? "enable" : "disable"} auto-record for ${
          evt.title || "this meeting"
        }.`,
      );
    }
  }

  if (daemonDown) {
    // `DaemonDownBanner` only watches the meetings store, so a
    // calendar-only failure (EventKit denied, /v1/calendar/upcoming
    // 5xx, daemon up but vault unconfigured) wouldn't surface
    // anywhere if we returned `null` here. Render a small inline
    // error with a retry instead.
    return (
      <BandShell>
        <BandHeader count={null} />
        <RailMessage>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <span>
              Couldn&rsquo;t load your calendar
              {error ? (
                <span
                  className="ml-2 font-mono text-[10px]"
                  style={{ color: "var(--color-ink-3)" }}
                >
                  ({error})
                </span>
              ) : null}
            </span>
            <Button size="sm" variant="outline" onClick={() => void load()}>
              Retry
            </Button>
          </div>
        </RailMessage>
      </BandShell>
    );
  }

  if (loading && items.length === 0) {
    return (
      <BandShell>
        <BandHeader count={null} />
        <RailMessage>Loading calendar…</RailMessage>
      </BandShell>
    );
  }

  if (rows.length === 0) {
    return (
      <BandShell>
        <BandHeader count={0} />
        <RailMessage>
          No meetings on your calendar in the next week.
        </RailMessage>
      </BandShell>
    );
  }

  const [next, ...rest] = rows;
  const minutesUntilNext = minutesUntil(next.start);

  return (
    <BandShell>
      <BandHeader count={rows.length} minutesUntilNext={minutesUntilNext} />

      <FeaturedCard
        event={next}
        minutesUntil={minutesUntilNext}
        disabled={disabled}
        onRecord={() => onStartWithContext(next)}
      />

      {rest.length > 0 && (
        <ul className="mt-3 flex flex-col">
          {rest.map((evt) => (
            <RestRow
              key={evt.id}
              evt={evt}
              pending={pendingAutoRecordIds.has(evt.id)}
              startDisabled={disabled}
              onAutoRecordChange={(enabled) =>
                void onAutoRecordChange(evt, enabled)
              }
              onStartWithContext={() => onStartWithContext(evt)}
            />
          ))}
        </ul>
      )}
    </BandShell>
  );
}

/**
 * Outer wrapper for the band — paper-2 background, top/bottom rules,
 * 56px horizontal padding to match the rest of the Home page.
 */
function BandShell({ children }: { children: ReactNode }) {
  return (
    <section
      className="border-b px-14 pt-7 pb-7"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
      }}
    >
      {children}
    </section>
  );
}

function BandHeader({
  count,
  minutesUntilNext,
}: {
  count: number | null;
  minutesUntilNext?: number | null;
}) {
  // The count is the only piece that may be unknown (loading /
  // daemon-down). Format the meta line accordingly so we never render
  // a literal `null events`.
  let meta: string | null = null;
  if (count !== null) {
    const eventsLabel = count === 1 ? "event" : "events";
    if (
      typeof minutesUntilNext === "number" &&
      Number.isFinite(minutesUntilNext) &&
      minutesUntilNext >= 0
    ) {
      meta =
        minutesUntilNext < 60
          ? `${count} ${eventsLabel} · next in ${Math.max(1, Math.round(minutesUntilNext))} min`
          : `${count} ${eventsLabel}`;
    } else {
      meta = `${count} ${eventsLabel}`;
    }
  }
  return (
    <header className="mb-4 flex items-baseline gap-3">
      <h2
        className="m-0 font-serif text-[18px] font-medium leading-tight"
        style={{ color: "var(--color-ink)", letterSpacing: "-0.005em" }}
      >
        Coming up
      </h2>
      {meta && (
        <span
          className="font-mono text-[11px]"
          style={{ color: "var(--color-ink-3)" }}
        >
          {meta}
        </span>
      )}
      <span className="flex-1" />
      <button
        type="button"
        onClick={() =>
          toast.info(
            "Calendar deep-link is coming with a Tauri shell command — open Calendar.app from the Dock for now.",
          )
        }
        className="inline-flex items-center gap-1.5 rounded px-2 py-1 text-[11.5px] transition-colors hover:bg-paper-3"
        style={{ color: "var(--color-ink-2)" }}
        title="Calendar deep-link coming soon"
      >
        <CalendarDays size={12} aria-hidden="true" />
        Calendar
      </button>
    </header>
  );
}

function FeaturedCard({
  event,
  minutesUntil,
  disabled,
  onRecord,
}: {
  event: CalendarEvent;
  minutesUntil: number | null;
  disabled?: boolean;
  onRecord: () => void;
}) {
  const start = new Date(event.start);
  const valid = !Number.isNaN(start.getTime());
  return (
    <div
      className="grid items-center gap-6 rounded-lg border p-5"
      style={{
        gridTemplateColumns: "92px 1fr auto",
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
        boxShadow: "0 1px 0 rgba(0,0,0,0.02)",
      }}
    >
      <DateStamp start={start} valid={valid} minutesUntil={minutesUntil} />
      <div className="min-w-0">
        <p
          className="mb-1 font-mono text-[10.5px] uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          {valid ? formatTimeRange(event.start, event.end) : "—"}
        </p>
        <h3
          className="m-0 mb-1.5 font-serif text-[18px] font-normal leading-[1.3]"
          style={{ color: "var(--color-ink)" }}
        >
          {event.title || "Untitled meeting"}
        </h3>
        <div className="flex flex-wrap items-center gap-1.5">
          {event.attendees.slice(0, 4).map((a) => (
            <AttendeeChip key={a.email ?? a.name} attendee={a} />
          ))}
          {event.primed && <PrimedHint />}
        </div>
      </div>
      <div className="flex flex-col items-stretch gap-1.5">
        <Button
          onClick={onRecord}
          disabled={disabled}
          variant="destructive"
          className="justify-center"
        >
          <span
            aria-hidden="true"
            className="inline-block"
            style={{
              width: 7,
              height: 7,
              borderRadius: "50%",
              background: "white",
            }}
          />
          Record now
        </Button>
        <Button
          variant="ghost"
          size="sm"
          className="justify-center"
          onClick={() => toast.info("Prep notes are coming in a follow-up.")}
        >
          <NotebookText size={11} aria-hidden="true" />
          View prep notes
        </Button>
      </div>
    </div>
  );
}

function DateStamp({
  start,
  valid,
  minutesUntil,
}: {
  start: Date;
  valid: boolean;
  minutesUntil: number | null;
}) {
  return (
    <div
      className="border-r pr-6"
      style={{ borderColor: "var(--color-rule)" }}
    >
      <div
        className="font-serif text-[36px] font-normal leading-none"
        style={{
          color: "var(--color-ink)",
          fontVariantNumeric: "tabular-nums",
        }}
      >
        {valid ? start.getDate() : "—"}
      </div>
      <div
        className="mt-1 font-mono text-[10.5px] uppercase tracking-[0.06em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        {valid ? DATE_STAMP_FORMATTER.format(start) : ""}
      </div>
      {valid &&
        typeof minutesUntil === "number" &&
        Number.isFinite(minutesUntil) &&
        minutesUntil >= 0 &&
        minutesUntil < 60 * 24 && (
          <div
            className="mt-2 font-mono text-[10px]"
            style={{ color: "var(--color-accent)" }}
          >
            {minutesUntil <= 1
              ? "starting now"
              : minutesUntil < 60
                ? `in ${Math.round(minutesUntil)} min`
                : `in ${Math.round(minutesUntil / 60)}h`}
          </div>
        )}
    </div>
  );
}

function RestRow({
  evt,
  pending,
  startDisabled,
  onAutoRecordChange,
  onStartWithContext,
}: {
  evt: CalendarEvent;
  pending: boolean;
  /** Mirrors ComingUpBand's `disabled` so all per-row Start buttons
   *  freeze together while a recording-start is in flight. */
  startDisabled: boolean | undefined;
  onAutoRecordChange: (enabled: boolean) => void;
  /**
   * Manual start-with-context for this row. The featured card is the
   * primary affordance for the imminent next event, but every other
   * row keeps a small ghost CTA so the previous "every row had a
   * Start with context button" capability is preserved — Auto-record
   * alone wouldn't cover the ad-hoc workflow of recording a future
   * event before its scheduled start.
   */
  onStartWithContext: () => void;
}) {
  const start = new Date(evt.start);
  const valid = !Number.isNaN(start.getTime());
  return (
    <li
      className="grid items-center gap-6 border-t px-5 py-2.5"
      style={{
        gridTemplateColumns: "92px 1fr auto",
        borderColor: "var(--color-rule)",
      }}
    >
      <div
        className="flex items-baseline gap-2 border-r pr-6"
        style={{ borderColor: "var(--color-rule)" }}
      >
        <span
          className="min-w-[40px] font-mono text-[11.5px] uppercase tracking-[0.04em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          {valid ? RELATIVE_DAY_FORMATTER.format(start).toUpperCase() : "—"}
        </span>
        <span
          className="font-mono text-[10.5px]"
          style={{ color: "var(--color-ink-4)" }}
        >
          {valid ? CLOCK_FORMATTER.format(start) : ""}
        </span>
      </div>
      <div className="flex min-w-0 items-center gap-3.5">
        <span
          className="truncate text-[13.5px]"
          style={{ color: "var(--color-ink)" }}
        >
          {evt.title || "Untitled meeting"}
        </span>
        {evt.attendees.length > 0 && (
          <span className="flex shrink-0">
            {evt.attendees.slice(0, 3).map((a, idx) => (
              <span
                key={a.email ?? a.name}
                style={{ marginLeft: idx === 0 ? 0 : -5 }}
              >
                <Avatar name={a.name || "?"} size={16} />
              </span>
            ))}
          </span>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-3">
        <label
          className="flex items-center gap-2 text-[11px]"
          style={{ color: "var(--color-ink-3)" }}
          title="Start recording automatically when this event begins"
        >
          <Mic size={11} aria-hidden="true" />
          <span className="font-mono uppercase tracking-[0.08em]">
            Auto-record
          </span>
          <Switch
            checked={evt.auto_record}
            disabled={pending}
            aria-label={`Auto-record ${evt.title || "untitled meeting"}`}
            onCheckedChange={onAutoRecordChange}
          />
        </label>
        <Button
          size="sm"
          variant="ghost"
          onClick={onStartWithContext}
          disabled={startDisabled}
          title={`Record ${evt.title || "this meeting"} now with context`}
        >
          Start
        </Button>
      </div>
    </li>
  );
}

function AttendeeChip({ attendee }: { attendee: AttendeeContext }) {
  return (
    <span
      className="inline-flex items-center gap-1.5 rounded-full border py-0.5 pl-0.5 pr-2 text-[11px]"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
        color: "var(--color-ink-2)",
      }}
    >
      <Avatar name={attendee.name || "?"} size={16} />
      <span>{attendee.name || "guest"}</span>
    </span>
  );
}

function PrimedHint() {
  return (
    <span
      className="ml-1.5 inline-flex items-center gap-1 font-mono text-[10.5px]"
      style={{ color: "var(--color-ink-3)" }}
      title="Pre-meeting context staged"
    >
      <Sparkles size={11} aria-hidden="true" />
      primed
    </span>
  );
}

/** Empty-state wrapper; same chrome as the live band. */
function RailMessage({ children }: { children: ReactNode }) {
  return (
    <div
      className="rounded border px-4 py-6 text-sm"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
        color: "var(--color-ink-3)",
      }}
    >
      {children}
    </div>
  );
}

/**
 * Drop events that already ended and sort the rest by start time.
 * `list_upcoming_calendar` is a window query, not a "future-only" or
 * "sorted" query — an event that ended five minutes ago is still
 * inside `[now, now+7d]`, and EventKit's read order isn't a
 * documented contract. Filtering + sorting here rather than
 * server-side keeps the daemon contract simple and the band stable
 * across daemon refactors.
 */
function filterImminent(items: CalendarEvent[]): CalendarEvent[] {
  const now = Date.now();
  return items
    .filter((e) => {
      const end = Date.parse(e.end);
      return Number.isFinite(end) ? end > now : true;
    })
    .slice()
    .sort((a, b) => Date.parse(a.start) - Date.parse(b.start));
}

function minutesUntil(startIso: string): number | null {
  const start = Date.parse(startIso);
  if (Number.isNaN(start)) return null;
  return (start - Date.now()) / 60_000;
}

// Hoisted to module scope so the band doesn't allocate a fresh
// `Intl.DateTimeFormat` for every row on every render — formatter
// construction reads ICU locale data and is the expensive part. The
// instances are locale-stable for the app's lifetime; if we ever
// expose per-user locale overrides, swap these for `useMemo` keyed
// on the chosen locale.
const TIME_RANGE_FORMATTER = new Intl.DateTimeFormat(undefined, {
  weekday: "short",
  hour: "numeric",
  minute: "2-digit",
});
const DATE_STAMP_FORMATTER = new Intl.DateTimeFormat(undefined, {
  month: "short",
  weekday: "short",
});
const RELATIVE_DAY_FORMATTER = new Intl.DateTimeFormat(undefined, {
  weekday: "short",
});
const CLOCK_FORMATTER = new Intl.DateTimeFormat(undefined, {
  hour: "numeric",
  minute: "2-digit",
});

/**
 * Render the time range as either a short relative form ("in 12 min")
 * for upcoming events within the next hour, or a weekday + clock time
 * + duration otherwise. Mirrors the meetings-table's terse-by-default
 * tone — the full datetime is one click away on the detail page.
 */
function formatTimeRange(startIso: string, endIso: string): string {
  const start = new Date(startIso);
  const end = new Date(endIso);
  if (Number.isNaN(start.getTime())) return startIso;
  const dur =
    Number.isNaN(end.getTime()) || end <= start
      ? ""
      : ` · ${Math.round((end.getTime() - start.getTime()) / 60_000)} MIN`;
  return `${TIME_RANGE_FORMATTER.format(start).toUpperCase()}${dur}`;
}

