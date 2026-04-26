/**
 * Left sidebar for the Review route — lists `.md` sessions in the
 * vault and lets the user click into one.
 *
 * Reads the vault path from `useSettingsStore` (PR-γ does not own
 * Settings.tsx; PR-δ ships the editor for `vault_root`). If the
 * vault is unconfigured or missing, we render an empty state pointing
 * the user at the Settings route — the rest of the Review page also
 * renders an empty state so the user is never staring at a blank
 * canvas wondering what to do.
 *
 * PR-κ (phase 72) groups sessions into Today / Yesterday / This week /
 * Older buckets driven by the `YYYY-MM-DD-HHMM <slug>` filename prefix
 * written by `crates/heron-vault`. Parsing the filename is offline,
 * IPC-free, and keeps the sidebar render path identical between cold
 * mount and post-save refresh.
 *
 * Active session is highlighted by comparing the route param to each
 * basename in the list.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { ChevronDown, ChevronRight } from "lucide-react";

import { invoke } from "../lib/invoke";
import { cn } from "../lib/cn";
import {
  DEFAULT_EXPANDED_BUCKETS,
  formatSessionLabel,
  groupSessionsByBucket,
  localDayKey,
  type ParsedSessionName,
  SESSION_BUCKET_LABELS,
  SESSION_BUCKET_ORDER,
  type SessionBucket,
} from "../lib/sessions";
import { useSettingsStore } from "../store/settings";

interface SessionsSidebarProps {
  /** Currently-active session basename (no `.md` suffix). */
  activeSessionId?: string;
  /**
   * Bumped after a successful save so the list re-fetches. Optional —
   * sidebar still works without it (will just be a one-shot fetch on
   * mount).
   */
  refreshKey?: number;
}

export function SessionsSidebar({
  activeSessionId,
  refreshKey,
}: SessionsSidebarProps) {
  const settings = useSettingsStore((s) => s.settings);
  const ensureLoaded = useSettingsStore((s) => s.ensureLoaded);
  const settingsLoading = useSettingsStore((s) => s.loading);
  const settingsError = useSettingsStore((s) => s.error);

  const [sessions, setSessions] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // `dayKey` triggers a re-bucket when the local day rolls over
  // (otherwise a long-lived window would freeze "Today" on yesterday's
  // sessions until the next save / refetch). We bump it via a timeout
  // aimed at the next local midnight rather than polling.
  const [dayKey, setDayKey] = useState(() => localDayKey(new Date()));
  useEffect(() => {
    const now = new Date();
    const nextMidnight = new Date(
      now.getFullYear(),
      now.getMonth(),
      now.getDate() + 1,
      0,
      0,
      0,
      // 1ms cushion past midnight so the timeout fires *into* the new
      // day, not at the prior day's last instant.
      1,
    );
    const ms = nextMidnight.getTime() - now.getTime();
    const handle = setTimeout(() => setDayKey(localDayKey(new Date())), ms);
    // Sleep/wake + background-throttling fallback: macOS suspends
    // `setTimeout` while the laptop is closed, so a window opened at
    // 23:55 and reopened the next morning would otherwise stay frozen
    // on yesterday's bucketing until the next save/refetch. Re-syncing
    // on visibility/focus catches that case without polling.
    const recheck = () => {
      const k = localDayKey(new Date());
      setDayKey((prev) => (prev === k ? prev : k));
    };
    document.addEventListener("visibilitychange", recheck);
    window.addEventListener("focus", recheck);
    return () => {
      clearTimeout(handle);
      document.removeEventListener("visibilitychange", recheck);
      window.removeEventListener("focus", recheck);
    };
  }, [dayKey]);
  // Per-bucket expanded state. Today + Yesterday default open; older
  // buckets default closed so a long-running vault doesn't dominate
  // the sidebar.
  //
  // We build the record with an explicit literal so TypeScript catches
  // any new `SessionBucket` member at compile time — `Object.fromEntries`
  // would have erased the key set into `string`.
  const [expanded, setExpanded] = useState<Record<SessionBucket, boolean>>(
    () => ({
      today: DEFAULT_EXPANDED_BUCKETS.has("today"),
      yesterday: DEFAULT_EXPANDED_BUCKETS.has("yesterday"),
      thisWeek: DEFAULT_EXPANDED_BUCKETS.has("thisWeek"),
      older: DEFAULT_EXPANDED_BUCKETS.has("older"),
    }),
  );
  const navigate = useNavigate();
  const settingsReady = settings !== null;

  useEffect(() => {
    void ensureLoaded();
  }, [ensureLoaded]);

  const vault = settings?.vault_root ?? "";

  useEffect(() => {
    let cancelled = false;
    if (!vault) {
      setSessions(null);
      setError(null);
      return () => {
        cancelled = true;
      };
    }
    invoke("heron_list_sessions", { vaultPath: vault })
      .then((list) => {
        if (cancelled) return;
        setSessions(list);
        setError(null);
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        setSessions([]);
        setError(message);
      });
    return () => {
      cancelled = true;
    };
  }, [vault, refreshKey]);

  // Memoize on the basename array + `dayKey` so we re-bucket only when
  // the session list changes or the local day rolls over —
  // `groupSessionsByBucket` parses each filename and would be wasteful
  // on every render. The `dayKey` dep covers the long-lived-window
  // case where Today's sessions should slide into Yesterday at
  // midnight.
  const grouped = useMemo(
    () => (sessions ? groupSessionsByBucket(sessions) : null),
    // `dayKey` looks unused inside the body but is the trigger for
    // recomputation across midnight: `groupSessionsByBucket` reads
    // `new Date()` internally, so we just need any dep that flips
    // when the day rolls over.
    [sessions, dayKey],
  );

  // Auto-expand the bucket containing the active session, but only
  // once per session id — if the user later collapses the bucket we
  // don't fight them on a save-triggered refresh. The ref tracks
  // the last id we auto-expanded for; we explicitly do NOT reset it
  // on `refreshKey` so a re-fetched list with the same active session
  // leaves the user's manual collapse alone.
  const lastAutoExpandedRef = useRef<string | null>(null);
  useEffect(() => {
    if (!grouped || !activeSessionId) return;
    if (lastAutoExpandedRef.current === activeSessionId) return;
    for (const key of SESSION_BUCKET_ORDER) {
      if (grouped[key].some((s) => s.id === activeSessionId)) {
        lastAutoExpandedRef.current = activeSessionId;
        setExpanded((prev) => (prev[key] ? prev : { ...prev, [key]: true }));
        return;
      }
    }
  }, [grouped, activeSessionId]);

  return (
    <aside className="w-[232px] shrink-0 border-r border-border bg-muted/30 flex flex-col">
      <div className="p-3 border-b border-border flex items-center justify-between">
        <span className="text-sm font-semibold">Sessions</span>
        <Link
          to="/home"
          className="text-xs text-muted-foreground hover:underline"
        >
          Home
        </Link>
      </div>
      <div className="flex-1 overflow-y-auto p-2 space-y-1">
        {settingsError && (
          <p className="text-xs text-destructive p-2">
            Settings load failed: {settingsError}
          </p>
        )}
        {!settingsReady && settingsLoading && (
          <p className="text-xs text-muted-foreground p-2">Loading…</p>
        )}
        {settingsReady && !vault && !settingsError && (
          <div className="text-xs text-muted-foreground p-2 space-y-2">
            <p>No vault configured.</p>
            <p>
              <Link to="/settings" className="underline">
                Set one in Settings
              </Link>
              .
            </p>
          </div>
        )}
        {vault && sessions === null && !error && (
          <p className="text-xs text-muted-foreground p-2">Loading…</p>
        )}
        {vault && error && (
          <p className="text-xs text-destructive p-2">{error}</p>
        )}
        {vault && sessions && sessions.length === 0 && !error && (
          <p className="text-xs text-muted-foreground p-2">No sessions yet.</p>
        )}
        {vault &&
          grouped &&
          SESSION_BUCKET_ORDER.map((key) => {
            const items = grouped[key];
            // Skip empty buckets entirely per the PR-κ scope — no
            // "no sessions today" placeholder.
            if (items.length === 0) return null;
            return (
              <SessionBucketDisclosure
                key={key}
                bucket={key}
                items={items}
                isOpen={expanded[key]}
                onToggle={() =>
                  setExpanded((prev) => ({ ...prev, [key]: !prev[key] }))
                }
                activeSessionId={activeSessionId}
                onSelect={(id) =>
                  navigate(`/review/${encodeURIComponent(id)}`)
                }
              />
            );
          })}
      </div>
    </aside>
  );
}

interface SessionBucketDisclosureProps {
  bucket: SessionBucket;
  items: ParsedSessionName[];
  isOpen: boolean;
  onToggle: () => void;
  activeSessionId?: string;
  onSelect: (id: string) => void;
}

/**
 * Radix-Disclosure-style collapsible group. We don't depend on
 * `@radix-ui/react-accordion` here because the section is a plain
 * `aria-expanded` button + a region that's either rendered or not —
 * importing the accordion package for one toggle would pull in extra
 * focus-management overhead the sidebar doesn't need.
 */
function SessionBucketDisclosure({
  bucket,
  items,
  isOpen,
  onToggle,
  activeSessionId,
  onSelect,
}: SessionBucketDisclosureProps) {
  const headerId = `sessions-bucket-${bucket}-header`;
  const regionId = `sessions-bucket-${bucket}-region`;
  const Chevron = isOpen ? ChevronDown : ChevronRight;
  return (
    <div className="space-y-0.5">
      <button
        id={headerId}
        type="button"
        onClick={onToggle}
        aria-expanded={isOpen}
        aria-controls={regionId}
        // Compose the accessible name explicitly so screen readers
        // announce e.g. "Today, 3 sessions, expanded" instead of just
        // the visual label. Using `aria-label` rather than relying on
        // a visually-hidden span keeps the markup simple — the count
        // is the only auxiliary info we'd surface anyway.
        aria-label={`${SESSION_BUCKET_LABELS[bucket]}, ${items.length} ${
          items.length === 1 ? "session" : "sessions"
        }`}
        className="w-full flex items-center gap-1 px-2 py-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground hover:text-foreground rounded-md focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
      >
        <Chevron className="w-3.5 h-3.5 shrink-0" aria-hidden="true" />
        <span aria-hidden="true">{SESSION_BUCKET_LABELS[bucket]}</span>
        <span
          className="ml-auto text-[10px] font-normal text-muted-foreground/80"
          aria-hidden="true"
        >
          {items.length}
        </span>
      </button>
      {isOpen && (
        // `role="region"` belongs on a wrapper rather than the `<ul>`
        // itself so screen readers still announce the list semantics
        // (item count, "list of N") — putting `role="region"` on the
        // `<ul>` would override its native role.
        <div id={regionId} role="region" aria-labelledby={headerId}>
          <ul className="space-y-0.5">
            {items.map((parsed) => {
              const active = parsed.id === activeSessionId;
              const label = formatSessionLabel(parsed);
              return (
                <li key={parsed.id}>
                  <button
                    type="button"
                    onClick={() => onSelect(parsed.id)}
                    className={cn(
                      "w-full text-left text-sm px-2 py-1.5 rounded-md truncate",
                      active
                        ? "bg-primary text-primary-foreground"
                        : "hover:bg-muted text-foreground",
                    )}
                    title={parsed.id}
                  >
                    {label}
                  </button>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </div>
  );
}
