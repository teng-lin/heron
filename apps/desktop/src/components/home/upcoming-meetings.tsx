/**
 * Upcoming-meetings rail for the Home page.
 *
 * Reads `useCalendarStore` (which proxies `GET /v1/calendar/upcoming`
 * via Tauri) and renders the user's next ~7-day window. Each row
 * exposes a "Start with context" affordance: the parent owns the
 * actual orchestration (attach_context + start_capture + navigate)
 * so the consent / disk-check pre-flight stays in one place. This
 * component is purely presentational + a single onClick callback.
 *
 * Calendar permission is the daemon's responsibility — when EventKit
 * is denied the proxy collapses to `unavailable` and the store flips
 * `daemonDown`. The shared `<DaemonDownBanner />` already covers that
 * case; the rail just renders an empty state and stays out of the way.
 */

import { useEffect, useMemo, useState, type ReactNode } from "react";
import { toast } from "sonner";

import { Button } from "../ui/button";
import { Switch } from "../ui/switch";
import { useCalendarStore } from "../../store/calendar";
import type { AttendeeContext, CalendarEvent } from "../../lib/types";

interface UpcomingMeetingsProps {
  /** Fired when the user clicks "Start with context" on a row. */
  onStartWithContext: (event: CalendarEvent) => void;
  /** Disable the per-row Start buttons (e.g., a recording-start is in flight). */
  disabled?: boolean;
}

export function UpcomingMeetings({
  onStartWithContext,
  disabled,
}: UpcomingMeetingsProps) {
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
      // Always clear the pending flag — if the IPC throws, the row
      // would otherwise stay disabled until a reload.
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
    // error with a retry instead so the rail's failure mode is
    // visible to the user.
    return (
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
    );
  }

  if (loading && items.length === 0) {
    return <RailMessage>Loading calendar…</RailMessage>;
  }

  if (rows.length === 0) {
    return <RailMessage>No meetings on your calendar in the next week.</RailMessage>;
  }

  return (
    <section className="mb-8">
      <RailHeader />
      <ul
        className="divide-y rounded border"
        style={{
          background: "var(--color-paper)",
          borderColor: "var(--color-rule)",
        }}
      >
        {rows.map((evt) => (
          <li
            key={evt.id}
            className="flex flex-wrap items-center gap-3 px-4 py-3"
          >
            <div className="min-w-0 flex-1">
              <div
                className="flex flex-wrap items-center gap-2 font-serif text-base"
                style={{ color: "var(--color-ink)" }}
              >
                <span className="min-w-0 flex-1 truncate">
                  {evt.title || "Untitled meeting"}
                </span>
                {evt.primed && <PrimedBadge />}
              </div>
              <div
                className="mt-0.5 flex flex-wrap items-center gap-x-3 text-xs"
                style={{ color: "var(--color-ink-3)" }}
              >
                <span className="font-mono uppercase tracking-[0.08em]">
                  {formatTimeRange(evt.start, evt.end)}
                </span>
                {evt.attendees.length > 0 && (
                  <span>{summarizeAttendees(evt.attendees)}</span>
                )}
              </div>
            </div>
            <div
              className="flex shrink-0 items-center gap-2 text-xs"
              style={{ color: "var(--color-ink-3)" }}
              title="Start recording automatically when this event begins"
            >
              <span className="font-mono uppercase tracking-[0.08em]">
                Auto
              </span>
              <Switch
                checked={evt.auto_record}
                disabled={pendingAutoRecordIds.has(evt.id)}
                aria-label={`Auto-record ${evt.title || "untitled meeting"}`}
                onCheckedChange={(checked) =>
                  void onAutoRecordChange(evt, checked)
                }
              />
            </div>
            <Button
              size="sm"
              variant="outline"
              onClick={() => onStartWithContext(evt)}
              disabled={disabled}
            >
              Start with context
            </Button>
          </li>
        ))}
      </ul>
    </section>
  );
}

/**
 * Tiny inline indicator: pre-meeting context (attendees, related
 * notes once RAG lands) is already staged for this event. The store
 * fans `heron_prepare_context` out automatically after every
 * successful `load()`; per-event store patches arrive as each Tauri
 * call settles, so an unprimed event flips its badge a few hundred
 * milliseconds after the rail first renders.
 */
function PrimedBadge() {
  return (
    <span
      title="Pre-meeting context staged"
      className="inline-flex shrink-0 items-center rounded-full border px-1.5 py-px font-mono text-[10px] uppercase tracking-[0.08em]"
      style={{
        color: "var(--color-ink-3)",
        borderColor: "var(--color-rule)",
        background: "var(--color-paper-2)",
      }}
    >
      primed
    </span>
  );
}

function RailHeader() {
  return (
    <header className="mb-2">
      <p
        className="font-mono text-xs uppercase tracking-[0.12em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        Upcoming
      </p>
    </header>
  );
}

/** Empty-state wrapper (loading + no-rows). Same chrome as the live rail. */
function RailMessage({ children }: { children: ReactNode }) {
  return (
    <section className="mb-8">
      <RailHeader />
      <div
        className="rounded border px-4 py-6 text-sm"
        style={{
          background: "var(--color-paper-2)",
          borderColor: "var(--color-rule)",
          color: "var(--color-ink-3)",
        }}
      >
        {children}
      </div>
    </section>
  );
}

/**
 * Drop events that already ended and sort the rest by start time.
 * `list_upcoming_calendar` is a window query, not a "future-only" or
 * "sorted" query — an event that ended five minutes ago is still
 * inside `[now, now+7d]`, and EventKit's read order isn't a
 * documented contract. Filtering + sorting here rather than
 * server-side keeps the daemon contract simple and the rail
 * stable across daemon refactors.
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

// Hoisted to module scope so the rail doesn't allocate a fresh
// `Intl.DateTimeFormat` for every row on every render — formatter
// construction reads ICU locale data and is the expensive part. The
// instance is locale-stable for the app's lifetime; if we ever expose
// per-user locale overrides, swap this for a `useMemo` keyed on the
// chosen locale.
const TIME_RANGE_FORMATTER = new Intl.DateTimeFormat(undefined, {
  weekday: "short",
  hour: "numeric",
  minute: "2-digit",
});

/**
 * Render the time range as either a short relative form ("in 12 min")
 * for upcoming events within the next hour, or a weekday + clock time
 * otherwise. Mirrors the meetings-table's terse-by-default tone — the
 * full datetime is one click away on the detail page.
 */
function formatTimeRange(startIso: string, endIso: string): string {
  const start = new Date(startIso);
  const end = new Date(endIso);
  if (Number.isNaN(start.getTime())) return startIso;
  const minutesUntil = Math.round((start.getTime() - Date.now()) / 60_000);
  if (minutesUntil >= 0 && minutesUntil < 60) {
    return minutesUntil <= 1 ? "starting now" : `in ${minutesUntil} min`;
  }
  const dur =
    Number.isNaN(end.getTime()) || end <= start
      ? ""
      : ` · ${Math.round((end.getTime() - start.getTime()) / 60_000)} min`;
  return `${TIME_RANGE_FORMATTER.format(start)}${dur}`;
}

function summarizeAttendees(attendees: AttendeeContext[]): string {
  if (attendees.length === 1) return attendees[0].name;
  if (attendees.length === 2) {
    return `${attendees[0].name} & ${attendees[1].name}`;
  }
  return `${attendees[0].name} +${attendees.length - 1}`;
}
