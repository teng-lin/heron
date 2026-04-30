/**
 * Home / Library — the post-onboarding landing surface.
 *
 * The redesigned shell renders five bands top-to-bottom:
 *
 *   1. `<HeroBand />` — editorial display copy + activity stats.
 *   2. `<ComingUpBand />` — featured next event + rest-of-day rows;
 *      reads `useCalendarStore` and exposes per-row auto-record +
 *      "Start with context" affordances.
 *   3. `<SpacesStrip />` — the Spaces tab IA, stub-mocked until the
 *      sharing backend lands.
 *   4. Toolbar (search + status filter + tag filter + manual Start
 *      recording CTA) and the existing `<MeetingsTable />`.
 *   5. `<HomeFooterNote />` — privacy reminder.
 *
 * `<AskBar />` is pinned at the bottom of the route area outside the
 * scrolling region so it stays visible regardless of how long the
 * meeting list grows. Cross-vault Ask isn't wired yet — the bar
 * renders its chrome and toasts a friendly stub on submit.
 *
 * The recording-start flow (disk-space gate → ConsentGate →
 * heron_attach_context → heron_start_capture → navigate) is preserved
 * verbatim from the previous Home; only the surrounding chrome
 * changed.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { Search } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { DaemonDownBanner } from "../components/DaemonDownBanner";
import { AskBar } from "../components/home/ask-bar";
import { ComingUpBand } from "../components/home/upcoming-meetings";
import { HeroBand } from "../components/home/hero-band";
import { HomeFooterNote } from "../components/home/footer-note";
import { SpacesStrip } from "../components/home/spaces-strip";
import {
  filterMeetings,
  MeetingsTable,
  type StatusFilter,
  type TagFilter,
} from "../components/home/meetings-table";
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
  const items = useMeetingsStore((s) => s.items);
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
  // inside `MeetingsTable`. Discriminated union (see `TagFilter`):
  //
  //   - `{ kind: "all" }`        — no tag constraint (default).
  //   - `{ kind: "untagged" }`   — show only meetings with empty `tags`.
  //   - `{ kind: "tag"; value }` — show only meetings whose `tags` has it.
  //
  // The discriminator avoids the `"untagged"` magic-string collision
  // with an LLM-emitted tag literally named `"untagged"`.
  const [tagFilter, setTagFilter] = useState<TagFilter>({ kind: "all" });

  useEffect(() => {
    void loadMeetings();
  }, [loadMeetings]);

  // Calendar-month roll-up for HeroBand stats. Recomputed when the
  // meetings list changes (or month boundary crosses, but the boundary
  // case is fine to wait for the next refresh — the user would have
  // had to leave the app open across midnight on the 1st).
  const monthStats = useMemo(() => {
    const startOfMonth = new Date();
    startOfMonth.setDate(1);
    startOfMonth.setHours(0, 0, 0, 0);
    const cutoff = startOfMonth.getTime();
    let count = 0;
    let secs = 0;
    for (const m of items) {
      const ts = Date.parse(m.started_at);
      if (Number.isFinite(ts) && ts >= cutoff) {
        count += 1;
        secs += m.duration_secs ?? 0;
      }
    }
    return { count, hours: secs / 3600 };
  }, [items]);

  const visibleCount = useMemo(
    () => filterMeetings(items, search, filter, tagFilter).length,
    [items, search, filter, tagFilter],
  );

  async function proceedToConsent(calendarEvent: CalendarEvent | null) {
    const decision = await requestConsent();
    if (decision !== "confirmed") {
      return;
    }
    // When the user clicked "Start with context" on a calendar row,
    // pre-stage the briefing before `start_capture`. The daemon keys
    // `pending_contexts` by `calendar_event_id`, so attaching first
    // means the orchestrator finds the context already in place when
    // the matching meeting arms. A failure here is non-fatal — losing
    // the briefing is strictly better than losing the recording.
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
    // Ask the daemon to start a capture before navigating. The
    // platform default is Zoom — same escape-hatch as `heron-cli`.
    // When the start was triggered from a calendar row, infer the
    // platform from `meeting_url` and pass `calendar_event_id` so the
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

  function dismissDiskWarning() {
    setDiskWarning(null);
    setPendingCalendarEvent(null);
    startingRef.current = false;
    setStarting(false);
  }

  async function onStart(event: CalendarEvent | null = null) {
    if (startingRef.current) return;
    startingRef.current = true;
    setStarting(true);
    let openedDiskWarning = false;
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
      if (!openedDiskWarning) {
        startingRef.current = false;
        setStarting(false);
      }
    }
  }

  async function continueAnyway() {
    setDiskWarning(null);
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
      {/*
        Layout: a full-height flex column whose middle child scrolls
        and whose AskBar pin sits at the visual bottom. The
        DaemonDownBanner is a flex sibling rather than a sibling of
        the column — keeping it outside the column would push the
        scroll region (and AskBar) below the visible viewport when
        the banner renders, breaking the pin. `min-h-0` on the
        scroll region defeats the default `min-height: auto` that
        flex items inherit; without it the inner content's intrinsic
        min-height would prevent shrinking and `overflow-auto` would
        never activate.
      */}
      <div className="flex h-full flex-col">
        <DaemonDownBanner />
        <div className="min-h-0 flex-1 overflow-auto">
          <HeroBand
            meetingsCount={monthStats.count}
            hoursCaptured={monthStats.hours}
            audioUploaded={0}
          />
          <ComingUpBand
            onStartWithContext={(evt) => void onStartWithContext(evt)}
            disabled={starting}
          />
          <SpacesStrip />
          <Toolbar
            search={search}
            setSearch={setSearch}
            filter={filter}
            setFilter={setFilter}
            tagFilter={tagFilter}
            setTagFilter={setTagFilter}
            visibleCount={visibleCount}
            starting={starting}
            onStart={() => void onStart()}
          />
          <div className="px-14 pb-6">
            <MeetingsTable
              query={search}
              filter={filter}
              tagFilter={tagFilter}
              onTagClick={(tag) => setTagFilter({ kind: "tag", value: tag })}
            />
          </div>
          <HomeFooterNote />
        </div>
        <AskBar />
      </div>

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
 * Toolbar row above the meetings table — search input, status filter,
 * tag filter, row-count meta, and the manual "Start recording" CTA.
 *
 * The Start button lives here (rather than the hero) so it's always
 * adjacent to the meetings list — the empty-state copy in
 * `MeetingsTable` literally references "the button above," and that
 * affordance has to remain reachable when the calendar rail is empty
 * (no `<ComingUpBand />` Record-now button to fall back on).
 */
function Toolbar({
  search,
  setSearch,
  filter,
  setFilter,
  tagFilter,
  setTagFilter,
  visibleCount,
  starting,
  onStart,
}: {
  search: string;
  setSearch: (value: string) => void;
  filter: StatusFilter;
  setFilter: (value: StatusFilter) => void;
  tagFilter: TagFilter;
  setTagFilter: (value: TagFilter) => void;
  visibleCount: number;
  starting: boolean;
  onStart: () => void;
}) {
  return (
    <div
      className="flex flex-wrap items-center gap-3 border-b px-14 py-3.5"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
      }}
    >
      <label
        className="relative flex flex-1 min-w-[240px] items-center"
        style={{ color: "var(--color-ink-3)" }}
      >
        <span className="sr-only">Search meetings</span>
        <Search
          size={13}
          aria-hidden="true"
          className="pointer-events-none absolute left-3"
        />
        <input
          type="text"
          aria-label="Search meetings"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder="Search transcripts, summaries, people…"
          className="w-full rounded border py-1.5 pl-8 pr-3 text-[13px] outline-none transition-shadow focus:shadow-[0_0_0_3px_var(--color-accent-soft)]"
          style={{
            background: "var(--color-paper)",
            borderColor: "var(--color-rule-2)",
            color: "var(--color-ink)",
          }}
        />
      </label>
      <FilterChips value={filter} onChange={setFilter} />
      <TagFilterChips value={tagFilter} onChange={setTagFilter} />
      <span
        className="font-mono text-[11px]"
        style={{ color: "var(--color-ink-3)" }}
      >
        {visibleCount} {visibleCount === 1 ? "meeting" : "meetings"} · sorted by
        date
      </span>
      <Button onClick={onStart} disabled={starting} size="sm">
        Start recording
      </Button>
    </div>
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
 *      constraint" and "only meetings with empty `tags`".
 *   2. A trailing pill appears when the user has filtered to a
 *      specific tag (via clicking a row chip). The pill shows
 *      `#tag ×` and clearing it returns to `All`.
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
  const segmentValue: "all" | "untagged" | null =
    value.kind === "tag" ? null : value.kind;
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
              onClick={() =>
                onChange(
                  opt.id === "all" ? { kind: "all" } : { kind: "untagged" },
                )
              }
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
      {value.kind === "tag" && (
        <button
          type="button"
          onClick={() => onChange({ kind: "all" })}
          className="inline-flex items-center gap-1 rounded-full border px-2 py-0.5 font-mono text-[10px] tracking-[0.04em] transition-colors"
          style={{
            background: "var(--color-accent)",
            color: "var(--color-paper)",
            borderColor: "var(--color-accent)",
          }}
          title="Clear tag filter"
        >
          <span>#{value.value}</span>
          <span aria-hidden="true">×</span>
          <span className="sr-only">Clear tag filter</span>
        </button>
      )}
    </div>
  );
}
