/**
 * Home / Library — the post-onboarding landing surface.
 *
 * Shows the meetings list pulled from `useMeetingsStore` (which calls
 * `heron_list_meetings` against the in-process daemon). The
 * "Start recording" CTA preserves the existing PR-λ disk-space gate +
 * ConsentGate flow before navigating to /recording.
 *
 * Daemon-down handling: when the meetings list call returns
 * `{ kind: "unavailable" }`, the store flips `daemonDown = true` and
 * the shared `<DaemonDownBanner />` renders the retry UI. Settings
 * and Salvage routes are deliberately layout-mounted so they keep
 * working even when the meetings table is unreachable.
 */

import { useEffect, useRef, useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { Search } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { DaemonDownBanner } from "../components/DaemonDownBanner";
import {
  MeetingsTable,
  type StatusFilter,
  type TagFilter,
} from "../components/home/meetings-table";
import { UpcomingMeetings } from "../components/home/upcoming-meetings";
import { Button } from "../components/ui/button";
import {
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../components/ui/dialog";
import { invoke } from "../lib/invoke";
import { cn } from "../lib/cn";
import type { CalendarEvent, Platform } from "../lib/types";
import { useConsentStore } from "../store/consent";
import { useMeetingsStore } from "../store/meetings";
import { useRecordingStore } from "../store/recording";
import { useSettingsStore } from "../store/settings";

interface DiskWarning {
  freeMib: number;
  thresholdMib: number;
}

export default function Home() {
  const requestConsent = useConsentStore((s) => s.requestConsent);
  const startRecording = useRecordingStore((s) => s.start);
  const ensureLoaded = useSettingsStore((s) => s.ensureLoaded);
  const loadMeetings = useMeetingsStore((s) => s.load);
  const navigate = useNavigate();

  const [diskWarning, setDiskWarning] = useState<DiskWarning | null>(null);
  const [pendingCalendarEvent, setPendingCalendarEvent] =
    useState<CalendarEvent | null>(null);
  // Re-entrancy guard for the recording-start flow. Two pieces:
  //
  // - `startingRef` is the AUTHORITATIVE synchronous lock. A `useState`
  //   guard wouldn't be safe — two clicks fired in the same React tick
  //   both read the pre-update state and both pass. A ref is mutated
  //   in place, so the second click sees the lock immediately.
  // - `starting` (state) drives the render so buttons get a `disabled`
  //   attribute. State is fine HERE because the render only needs to
  //   reflect the lock, not enforce it.
  //
  // Both are set/cleared in lockstep at every entry/exit point.
  const startingRef = useRef(false);
  const [starting, setStarting] = useState(false);
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<StatusFilter>("all");
  // Tag filter is independent of `filter` (status). They compose AND
  // inside `MeetingsTable`. Three states:
  //
  //   - `null`        — no tag constraint (default).
  //   - `"untagged"`  — show only meetings with empty / missing `tags`.
  //   - any string    — show only meetings whose `tags` includes it.
  //
  // Single-select so the chip strip stays a flat segmented-control UX —
  // a multi-tag picker would need a popover, which is more weight than
  // this surface needs.
  const [tagFilter, setTagFilter] = useState<TagFilter>(null);

  useEffect(() => {
    void loadMeetings();
  }, [loadMeetings]);

  async function proceedToConsent(calendarEvent: CalendarEvent | null) {
    const decision = await requestConsent();
    if (decision !== "confirmed") {
      return;
    }
    // Gap #8: when the user clicked "Start with context" on a calendar
    // row, pre-stage the briefing before `start_capture`. The daemon
    // keys `pending_contexts` by `calendar_event_id`, so attaching
    // first means the orchestrator finds the context already in place
    // when the matching meeting arms. A failure here is non-fatal —
    // we surface the detail and continue starting the capture without
    // context, because losing the briefing is strictly better than
    // losing the recording.
    if (calendarEvent !== null) {
      try {
        const ack = await invoke("heron_attach_context", {
          request: {
            calendar_event_id: calendarEvent.id,
            context: {
              agenda: calendarEvent.title || null,
              attendees_known: calendarEvent.attendees,
              related_notes: [],
              prior_decisions: [],
              user_briefing: null,
            },
          },
        });
        if (ack.kind !== "ok") {
          toast.warning(`Context not attached: ${ack.detail}`);
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        toast.warning(`Context not attached: ${message}`);
      }
    }
    // Gap #7: ask the daemon to actually start a capture before we
    // navigate. Pre-PR the button only flipped local recording-store
    // state; now Start = `POST /v1/meetings`. The platform default is
    // Zoom — same escape-hatch behaviour as `heron-cli`. When the
    // start was triggered from a calendar row, infer the platform
    // from `meeting_url` and pass `calendar_event_id` so the
    // orchestrator pairs the capture with the context attached above.
    const platform = inferPlatform(calendarEvent?.meeting_url ?? null) ?? "zoom";
    let outcome;
    try {
      outcome = await invoke("heron_start_capture", {
        platform,
        calendarEventId: calendarEvent?.id ?? null,
      });
    } catch (err) {
      // Reaching here means the Tauri IPC bridge itself failed — the
      // daemon never even saw the request. Surface and stay; the
      // recording-store stays clean so a retry from the same button
      // works.
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not start recording: ${message}`);
      return;
    }
    if (outcome.kind !== "ok") {
      // Daemon-side failure (409 already-recording, 5xx
      // platform-not-running, transport error). The detail string
      // comes from the daemon's error envelope; surfacing it
      // verbatim lets the user act on it (e.g., "open Zoom").
      toast.error(`Could not start recording: ${outcome.detail}`);
      return;
    }
    startRecording(outcome.data.id);
    navigate("/recording");
  }

  // Both the disk-warning Cancel button and the dialog's outer
  // dismiss must clear the disk warning, the pending calendar event,
  // AND the in-flight `starting` guard so the next click starts from
  // a clean slate.
  function dismissDiskWarning() {
    setDiskWarning(null);
    setPendingCalendarEvent(null);
    startingRef.current = false;
    setStarting(false);
  }

  async function onStart(event: CalendarEvent | null = null) {
    // Synchronous re-entrancy gate. The ref read-and-set happens in a
    // single tick; a second click in the same tick sees `true` and
    // exits before any IPC fires.
    if (startingRef.current) return;
    startingRef.current = true;
    setStarting(true);
    let openedDiskWarning = false;
    // The calendar event is threaded through as a parameter rather
    // than read back from `pendingCalendarEvent` here. React state
    // updates scheduled inside an async event handler don't surface
    // until the next render, so the call site that just did
    // `setPendingCalendarEvent(evt)` would still see the stale
    // closure (`null`) when this function reads state. Pending state
    // is reserved for the disk-warning dialog branch below: the
    // dialog opens, the user later clicks Continue Anyway in a fresh
    // render, and `continueAnyway` reads the up-to-date state from
    // its own (newer) closure.
    try {
      try {
        const settingsPath = await invoke("heron_default_settings_path");
        const outcome = await invoke("heron_check_disk_for_recording", {
          settingsPath,
        });
        if (outcome.kind === "below_threshold") {
          setPendingCalendarEvent(event);
          setDiskWarning({
            freeMib: outcome.free_mib,
            thresholdMib: outcome.threshold_mib,
          });
          openedDiskWarning = true;
          return;
        }
      } catch (err) {
        // Pre-flight check is non-blocking on IPC failure.
        // eslint-disable-next-line no-console
        console.warn("[heron] disk pre-flight failed at start:", err);
      }
      await proceedToConsent(event);
    } finally {
      // Hand off to the dialog when the disk-warning branch fired —
      // `continueAnyway` / `dismissDiskWarning` clear the lock once
      // the user resolves the dialog.
      if (!openedDiskWarning) {
        startingRef.current = false;
        setStarting(false);
      }
    }
  }

  async function continueAnyway() {
    setDiskWarning(null);
    // `pendingCalendarEvent` was set by the prior `onStart(event)`
    // call before the dialog opened. By the time the user clicks
    // Continue Anyway, React has rendered the dialog and this
    // closure captures the up-to-date state — no staleness.
    const evt = pendingCalendarEvent;
    setPendingCalendarEvent(null);
    try {
      await proceedToConsent(evt);
    } finally {
      startingRef.current = false;
      setStarting(false);
    }
  }

  async function onStartWithContext(event: CalendarEvent) {
    // Thread the event through `onStart` as a parameter so the
    // no-disk-warning happy path doesn't depend on a state update
    // that won't surface until the next render.
    await onStart(event);
  }

  async function openVaultFolder() {
    try {
      const settings = await ensureLoaded();
      const target = settings?.vault_root ?? "";
      if (target) {
        toast.info(`Vault path: ${target}`, {
          description: "Open this folder in Finder and free up space.",
          duration: 12_000,
        });
      } else {
        toast.info("No vault path set yet — pick one in Settings → General.");
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not resolve vault path: ${message}`);
    }
  }

  return (
    <>
      <DaemonDownBanner />
      <main className="mx-auto w-full max-w-5xl px-8 py-10">
        <header className="mb-8">
          <p
            className="font-mono text-xs uppercase tracking-[0.12em]"
            style={{ color: "var(--color-ink-3)" }}
          >
            Library
          </p>
          <h1
            className="mt-1 font-serif text-[32px] leading-tight"
            style={{ color: "var(--color-ink)", letterSpacing: "-0.02em" }}
          >
            Welcome back
          </h1>
          <p
            className="mt-2 max-w-prose text-sm"
            style={{ color: "var(--color-ink-2)" }}
          >
            Start a recording from the tray, ⌘⇧R, or the button below. Past
            meetings show up here once the daemon finishes summarizing.
          </p>
          <div className="mt-4 flex items-center gap-2">
            <Button onClick={() => void onStart()} disabled={starting}>
              Start recording
            </Button>
          </div>
        </header>

        <UpcomingMeetings
          onStartWithContext={(evt) => void onStartWithContext(evt)}
          disabled={starting}
        />

        <div className="mb-4 flex flex-wrap items-center gap-3">
          <label
            className="relative flex flex-1 min-w-[240px] items-center"
            style={{ color: "var(--color-ink-3)" }}
          >
            <span className="sr-only">Search meetings</span>
            <Search
              size={14}
              aria-hidden="true"
              className="pointer-events-none absolute left-3"
            />
            <input
              type="text"
              aria-label="Search meetings"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder="Search title, platform, or attendee"
              className="w-full rounded border py-2 pl-9 pr-3 text-sm outline-none transition-shadow focus:shadow-[0_0_0_3px_var(--color-accent-soft)]"
              style={{
                background: "var(--color-paper)",
                borderColor: "var(--color-rule-2)",
                color: "var(--color-ink)",
              }}
            />
          </label>
          <FilterChips value={filter} onChange={setFilter} />
          <TagFilterChips value={tagFilter} onChange={setTagFilter} />
        </div>

        <MeetingsTable
          query={search}
          filter={filter}
          tagFilter={tagFilter}
          onTagClick={(tag) => setTagFilter(tag)}
        />
      </main>

      <Dialog.Root
        open={diskWarning !== null}
        onOpenChange={(next) => {
          if (!next) dismissDiskWarning();
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Low disk space</DialogTitle>
          </DialogHeader>
          {diskWarning && (
            <p className="text-sm">
              Only <strong>{diskWarning.freeMib} MiB</strong> free, threshold is{" "}
              <strong>{diskWarning.thresholdMib} MiB</strong>. Free up disk
              before recording.
            </p>
          )}
          <div className="mt-2 flex flex-wrap justify-end gap-2">
            <Button variant="ghost" onClick={dismissDiskWarning}>
              Cancel
            </Button>
            <Button variant="outline" onClick={() => void openVaultFolder()}>
              Open vault folder
            </Button>
            <Button onClick={() => void continueAnyway()}>
              Continue anyway
            </Button>
          </div>
        </DialogContent>
      </Dialog.Root>
    </>
  );
}

/**
 * Best-guess Platform from a calendar event's `meeting_url`. Returns
 * `null` when the URL is missing or unrecognised — caller falls back
 * to Zoom (the same default the manual Start button uses). Hostname
 * matching only; we don't try to parse meeting IDs.
 *
 * Each branch matches `host === root || host.endsWith("." + root)` so
 * a look-alike domain like `evilzoom.us` does NOT register as Zoom —
 * `endsWith` alone admits any suffix-collision string. Only the
 * scheme-validated `http:` / `https:` URLs flow through; `javascript:`
 * and other non-web schemes parse to an empty hostname and bail out.
 */
function inferPlatform(meetingUrl: string | null): Platform | null {
  if (!meetingUrl) return null;
  let parsed: URL;
  try {
    parsed = new URL(meetingUrl);
  } catch {
    return null;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
  const host = parsed.hostname.toLowerCase();
  const isHost = (root: string) => host === root || host.endsWith(`.${root}`);
  if (isHost("zoom.us") || isHost("zoomgov.com")) return "zoom";
  if (isHost("meet.google.com")) return "google_meet";
  if (isHost("teams.microsoft.com") || isHost("teams.live.com")) {
    return "microsoft_teams";
  }
  if (isHost("webex.com")) return "webex";
  return null;
}

function FilterChips({
  value,
  onChange,
}: {
  value: StatusFilter;
  onChange: (next: StatusFilter) => void;
}) {
  const options: { id: StatusFilter; label: string }[] = [
    { id: "all", label: "All" },
    { id: "active", label: "In progress" },
    { id: "done", label: "Done" },
  ];
  return (
    <div
      className="inline-flex overflow-hidden rounded border"
      style={{ borderColor: "var(--color-rule)" }}
    >
      {options.map((opt) => {
        const active = opt.id === value;
        return (
          <button
            type="button"
            key={opt.id}
            onClick={() => onChange(opt.id)}
            className={cn(
              "px-3 py-1.5 font-mono text-[10px] uppercase tracking-[0.12em] transition-colors",
            )}
            style={{
              background: active
                ? "var(--color-accent)"
                : "var(--color-paper)",
              color: active ? "var(--color-paper)" : "var(--color-ink-3)",
            }}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

/**
 * Tag-axis filter strip. Sits next to the status `FilterChips` and
 * applies orthogonally — selecting `Untagged` doesn't reset the
 * status filter, just constrains the tag axis.
 *
 * Two affordances:
 *
 *   1. The `All` / `Untagged` segmented pair toggles between "no tag
 *      constraint" and "only meetings with empty `tags`". Clicking
 *      `All` from any specific-tag state also clears down to `null`.
 *   2. A trailing pill appears when the user has filtered to a
 *      specific tag (via clicking a row chip — Home doesn't surface
 *      the full tag enumeration, since the LLM emits a long-tail
 *      vocabulary and a faceted picker would need product work). The
 *      pill shows `#tag ×` and clearing it returns to `All`.
 *
 * Same segmented-control geometry as `FilterChips` so the two strips
 * read as a unified filter bar.
 */
function TagFilterChips({
  value,
  onChange,
}: {
  value: TagFilter;
  onChange: (next: TagFilter) => void;
}) {
  const options: { id: "all" | "untagged"; label: string }[] = [
    { id: "all", label: "All tags" },
    { id: "untagged", label: "Untagged" },
  ];
  // The "active" state for the segmented pair: a specific-tag value
  // doesn't light either button — only the trailing pill below.
  const segmentValue: "all" | "untagged" | null =
    value === null ? "all" : value === "untagged" ? "untagged" : null;
  return (
    <div className="inline-flex items-center gap-2">
      <div
        className="inline-flex overflow-hidden rounded border"
        style={{ borderColor: "var(--color-rule)" }}
      >
        {options.map((opt) => {
          const active = opt.id === segmentValue;
          return (
            <button
              type="button"
              key={opt.id}
              onClick={() => onChange(opt.id === "all" ? null : "untagged")}
              className={cn(
                "px-3 py-1.5 font-mono text-[10px] uppercase tracking-[0.12em] transition-colors",
              )}
              style={{
                background: active
                  ? "var(--color-accent)"
                  : "var(--color-paper)",
                color: active ? "var(--color-paper)" : "var(--color-ink-3)",
              }}
            >
              {opt.label}
            </button>
          );
        })}
      </div>
      {typeof value === "string" && value !== "untagged" && (
        <button
          type="button"
          onClick={() => onChange(null)}
          className="inline-flex items-center gap-1 rounded-full border px-2 py-0.5 font-mono text-[10px] tracking-[0.04em] transition-colors"
          style={{
            background: "var(--color-accent)",
            color: "var(--color-paper)",
            borderColor: "var(--color-accent)",
          }}
          title="Clear tag filter"
        >
          <span>#{value}</span>
          <span aria-hidden="true">×</span>
          <span className="sr-only">Clear tag filter</span>
        </button>
      )}
    </div>
  );
}
