import { useMemo } from "react";
import { useNavigate } from "react-router-dom";

import { Avatar } from "../ui/avatar";
import { useMeetingsStore } from "../../store/meetings";
import type { Meeting, MeetingStatus, Platform } from "../../lib/types";

interface MeetingsTableProps {
  query: string;
  filter: StatusFilter;
  /**
   * Tag filter compounds with `filter` (status) using AND semantics.
   * See `TagFilter` for the discriminated-union shape.
   *
   * Held in Home-page state so the chip click on a row can toggle it
   * without lifting the whole filter machinery into a store.
   */
  tagFilter: TagFilter;
  /**
   * Click-handler for a row-level tag chip. Optional so callers that
   * don't want chip-click filtering (e.g. Review page, where tags are
   * decorative) can omit it. When provided, clicking the chip stops
   * row navigation and hands the tag string up.
   */
  onTagClick?: (tag: string) => void;
}

export type StatusFilter = "all" | "active" | "done";

/**
 * Tag-axis filter state. A discriminated union (rather than the
 * naive `null | "untagged" | string` shape we shipped originally)
 * so an LLM-emitted tag literally named `"untagged"` cannot collide
 * with the "no-tags" sentinel — a user clicking a `#untagged` row
 * chip yields `{ kind: "tag", value: "untagged" }`, NOT the
 * `{ kind: "untagged" }` magic that hides every tagged meeting.
 *
 * - `{ kind: "all" }` — no tag constraint (default).
 * - `{ kind: "untagged" }` — show only meetings whose `tags` is
 *   empty / missing.
 * - `{ kind: "tag"; value }` — show only meetings whose `tags`
 *   contains `value` (case-insensitive exact match).
 */
export type TagFilter =
  | { kind: "all" }
  | { kind: "untagged" }
  | { kind: "tag"; value: string };

/**
 * Pure predicate used by `MeetingsTable` to filter the rendered list.
 *
 * Extracted so the status × tag × free-text matrix can be unit-pinned
 * without spinning up a React renderer (the rest of the file's tests
 * follow the same pure-helper pattern as `Review.test.ts`).
 *
 * Semantics:
 *
 *   - `filter` (status):
 *       - `"all"` — no status constraint.
 *       - `"active"` — drop `done` / `failed`.
 *       - `"done"` — keep only `done`.
 *   - `tagFilter` (discriminated union — see `TagFilter`):
 *       - `{ kind: "all" }` — no tag constraint.
 *       - `{ kind: "untagged" }` — keep only meetings whose `tags` is
 *         empty / missing.
 *       - `{ kind: "tag"; value }` — keep only meetings whose `tags`
 *         contains `value` (case-insensitive exact match).
 *   - `query` (free text): empty → no constraint. Otherwise the
 *     lowercased substring is searched against title, platform label
 *     (`"Google Meet"`) and wire value (`"google_meet"`), participant
 *     names, and tag strings (a typed query of `"react"` matches a
 *     meeting tagged `"react"`).
 *
 * All three axes compose with AND.
 */
export function filterMeetings(
  items: Meeting[],
  query: string,
  filter: StatusFilter,
  tagFilter: TagFilter,
): Meeting[] {
  const q = query.trim().toLowerCase();
  const normalizedTag =
    tagFilter.kind === "tag" ? tagFilter.value.toLowerCase() : null;
  return items.filter((m) => {
    if (filter === "active" && (m.status === "done" || m.status === "failed")) {
      return false;
    }
    if (filter === "done" && m.status !== "done") {
      return false;
    }
    // Tag filter is independent of status filter — they compose AND.
    // `tags` is optional on the wire (back-compat with pre-Tier-0-#1
    // daemons), so coalesce to `[]` before reading.
    const tags = m.tags ?? [];
    if (tagFilter.kind === "untagged" && tags.length > 0) {
      return false;
    }
    if (
      normalizedTag !== null &&
      !tags.some((t) => t.toLowerCase() === normalizedTag)
    ) {
      return false;
    }
    if (q.length === 0) return true;
    const title = (m.title ?? "").toLowerCase();
    const matchesTitle = title.includes(q);
    // Match against the rendered label ("Google Meet") as well as
    // the wire value ("google_meet") so a query like "google meet"
    // hits the row the user is actually looking at. The label
    // lookup is defensive against platforms the daemon emits ahead
    // of a frontend update — we fall back to the wire value rather
    // than crashing the whole table.
    const platformLabel = (
      PLATFORM_LABEL[m.platform] ?? m.platform
    ).toLowerCase();
    const matchesPlatform =
      platformLabel.includes(q) || m.platform.includes(q);
    const matchesParticipant = m.participants.some((p) =>
      p.display_name.toLowerCase().includes(q),
    );
    const matchesTag = tags.some((t) => t.toLowerCase().includes(q));
    return matchesTitle || matchesPlatform || matchesParticipant || matchesTag;
  });
}

/**
 * Tag chips render with a leading `#` to mirror the social-style tag
 * vocabulary the LLM summarizer emits and to disambiguate them from
 * platform/status badges (which are uppercase, no prefix).
 */
export function MeetingsTable({
  query,
  filter,
  tagFilter,
  onTagClick,
}: MeetingsTableProps) {
  const items = useMeetingsStore((s) => s.items);
  const loading = useMeetingsStore((s) => s.loading);

  const rows = useMemo(
    () => filterMeetings(items, query, filter, tagFilter),
    [items, query, filter, tagFilter],
  );

  if (loading && items.length === 0) {
    return <SkeletonRows />;
  }

  if (rows.length === 0) {
    return (
      <div
        className="rounded border px-6 py-12 text-center"
        style={{
          background: "var(--color-paper-2)",
          borderColor: "var(--color-rule)",
          color: "var(--color-ink-3)",
        }}
      >
        <p className="font-serif text-lg" style={{ color: "var(--color-ink-2)" }}>
          {items.length === 0
            ? "No meetings yet"
            : "Nothing matches your filter"}
        </p>
        <p className="mt-1 text-xs">
          {items.length === 0
            ? "Start your first recording from the tray, ⌘⇧R, or the button above."
            : "Adjust the search or filter to see more rows."}
        </p>
      </div>
    );
  }

  return (
    <div
      className="overflow-hidden rounded border"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
      }}
    >
      <table className="w-full text-left text-sm">
        <thead>
          <tr
            className="font-mono text-[10px] uppercase tracking-[0.12em]"
            style={{
              background: "var(--color-paper-2)",
              color: "var(--color-ink-3)",
            }}
          >
            <th className="px-4 py-2 font-normal">Meeting</th>
            <th className="px-4 py-2 font-normal">Platform</th>
            <th className="px-4 py-2 font-normal">When</th>
            <th className="px-4 py-2 font-normal">Length</th>
            <th className="px-4 py-2 font-normal">Status</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((m) => (
            <Row key={m.id} meeting={m} onTagClick={onTagClick} />
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Row({
  meeting,
  onTagClick,
}: {
  meeting: Meeting;
  onTagClick?: (tag: string) => void;
}) {
  const navigate = useNavigate();
  const fetchSummary = useMeetingsStore((s) => s.fetchSummary);
  const summary = useMeetingsStore((s) => s.summaries[meeting.id]);
  // Coalesce optional `tags` (back-compat with pre-Tier-0-#1 daemons,
  // see `Meeting.tags` doc).
  const tags = meeting.tags ?? [];

  const previewText =
    summary && summary !== "unavailable"
      ? summary.text.replace(/[#*_`>]/g, "").slice(0, 140).trim()
      : null;

  const onHover = () => {
    if (summary === undefined) {
      void fetchSummary(meeting.id);
    }
  };

  const onClick = () => {
    navigate(`/review/${encodeURIComponent(meeting.id)}`);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTableRowElement>) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onClick();
    }
  };

  return (
    <tr
      role="link"
      aria-label={`Open ${meeting.title ?? "untitled meeting"}`}
      onMouseEnter={onHover}
      onFocus={onHover}
      onClick={onClick}
      onKeyDown={onKeyDown}
      tabIndex={0}
      className="cursor-pointer border-t transition-colors hover:bg-paper-2"
      style={{ borderColor: "var(--color-rule)" }}
    >
      <td className="px-4 py-3 align-top">
        <div
          className="font-serif text-[14px] leading-tight"
          style={{ color: "var(--color-ink)" }}
        >
          {meeting.title ?? "Untitled meeting"}
        </div>
        {previewText && (
          <div
            className="mt-1 line-clamp-2 text-xs"
            style={{ color: "var(--color-ink-3)" }}
          >
            {previewText}
          </div>
        )}
        {meeting.participants.length > 0 && (
          <div className="mt-1.5 flex items-center gap-1">
            {meeting.participants.slice(0, 4).map((p) => (
              <Avatar key={p.display_name} name={p.display_name} size={16} />
            ))}
            {meeting.participants.length > 4 && (
              <span
                className="font-mono text-[10px]"
                style={{ color: "var(--color-ink-4)" }}
              >
                +{meeting.participants.length - 4}
              </span>
            )}
          </div>
        )}
        {tags.length > 0 && (
          <div className="mt-1.5 flex flex-wrap items-center gap-1">
            {/*
              Key includes `index` because the LLM summarizer can in
              theory emit duplicate tag strings — `key={tag}` alone
              would collapse two `#react` chips onto the same React
              key and trigger the "two children with the same key"
              warning + lose chip-local state on reorder.
            */}
            {tags.map((tag, index) => (
              <TagChip
                key={`${tag}-${index}`}
                tag={tag}
                onClick={
                  onTagClick
                    ? (e) => {
                        // Stop the row's onClick from also firing —
                        // chip click should filter, not navigate.
                        e.stopPropagation();
                        onTagClick(tag);
                      }
                    : undefined
                }
              />
            ))}
          </div>
        )}
      </td>
      <td className="px-4 py-3 align-top">
        <PlatformBadge platform={meeting.platform} />
      </td>
      <td
        className="px-4 py-3 align-top text-xs"
        style={{ color: "var(--color-ink-3)" }}
      >
        {formatRelative(meeting.started_at)}
      </td>
      <td
        className="px-4 py-3 align-top font-mono text-xs"
        style={{ color: "var(--color-ink-3)" }}
      >
        {formatDuration(meeting.duration_secs)}
      </td>
      <td className="px-4 py-3 align-top">
        <StatusBadge status={meeting.status} />
      </td>
    </tr>
  );
}

const PLATFORM_LABEL: Record<Platform, string> = {
  zoom: "Zoom",
  google_meet: "Google Meet",
  microsoft_teams: "Teams",
  webex: "Webex",
};

/**
 * Render an LLM-emitted topic tag as a pill chip with a leading `#`.
 *
 * Same chip skeleton as `PlatformBadge` (rounded-full border, mono
 * 10px) so the row's badge vocabulary stays visually consistent. When
 * `onClick` is supplied, the chip becomes a button and stops both
 * mouse-click AND keydown propagation. The parent `<tr>` in the
 * meetings table is a row-link to /review/{id} that listens for
 * `click` (to navigate) and `keydown` of Enter/Space (the same
 * navigation, for keyboard users) — without `stopPropagation` on the
 * keydown branch, pressing Enter on a focused chip would BOTH apply
 * the tag filter AND navigate to the meeting, leaving the user
 * stranded on the wrong page with the filter silently applied.
 *
 * Exported so the Review page header can render the same shape
 * without re-deriving the tailwind/CSS-var combo.
 */
export function TagChip({
  tag,
  onClick,
}: {
  tag: string;
  onClick?: (event: React.MouseEvent<HTMLElement>) => void;
}) {
  const className =
    "inline-flex items-center rounded-full border px-2 py-0.5 font-mono text-[10px] tracking-[0.04em]";
  const style = {
    color: "var(--color-ink-3)",
    borderColor: "var(--color-rule-2)",
    background: "var(--color-paper-2)",
  } as const;
  const label = `#${tag}`;
  if (onClick) {
    return (
      <button
        type="button"
        onClick={onClick}
        onKeyDown={(e) => {
          // Match the row's keyboard contract — Enter / Space activate
          // a focusable element. Stop propagation BEFORE the row's
          // own `onKeyDown` sees the event and navigates. Without this
          // both handlers fire (button-click via browser default,
          // navigate via row keydown) and the user lands on /review
          // with an unintended filter applied.
          if (e.key === "Enter" || e.key === " ") {
            e.stopPropagation();
          }
        }}
        className={`${className} cursor-pointer transition-colors hover:bg-paper-3`}
        style={style}
        title={`Filter by ${label}`}
      >
        {label}
      </button>
    );
  }
  return (
    <span className={className} style={style}>
      {label}
    </span>
  );
}

function PlatformBadge({ platform }: { platform: Platform }) {
  return (
    <span
      className="inline-flex items-center rounded-full border px-2 py-0.5 font-mono text-[10px] uppercase tracking-[0.08em]"
      style={{
        color: "var(--color-ink-3)",
        borderColor: "var(--color-rule-2)",
        background: "var(--color-paper-2)",
      }}
    >
      {PLATFORM_LABEL[platform] ?? platform}
    </span>
  );
}

const STATUS_TONE: Record<MeetingStatus, { color: string; bg: string }> = {
  detected: { color: "var(--color-warn)", bg: "var(--color-paper-2)" },
  armed: { color: "var(--color-warn)", bg: "var(--color-paper-2)" },
  recording: { color: "var(--color-rec)", bg: "var(--color-paper-2)" },
  ended: { color: "var(--color-ink-3)", bg: "var(--color-paper-2)" },
  done: { color: "var(--color-ok)", bg: "var(--color-paper-2)" },
  failed: { color: "var(--color-rec)", bg: "var(--color-paper-2)" },
};

function StatusBadge({ status }: { status: MeetingStatus }) {
  const tone = STATUS_TONE[status] ?? {
    color: "var(--color-ink-3)",
    bg: "var(--color-paper-2)",
  };
  return (
    <span
      className="inline-flex items-center rounded-full border px-2 py-0.5 font-mono text-[10px] uppercase tracking-[0.08em]"
      style={{
        color: tone.color,
        borderColor: tone.color,
        background: tone.bg,
      }}
    >
      {status}
    </span>
  );
}

function SkeletonRows() {
  return (
    <div
      className="space-y-2 rounded border p-4"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
      }}
    >
      {[0, 1, 2].map((i) => (
        <div
          key={i}
          className="h-12 animate-pulse rounded"
          style={{ background: "var(--color-paper-3)" }}
        />
      ))}
    </div>
  );
}

function formatRelative(rfc3339: string): string {
  const t = Date.parse(rfc3339);
  if (Number.isNaN(t)) return "—";
  const diffMs = Date.now() - t;
  const minute = 60_000;
  const hour = 60 * minute;
  const day = 24 * hour;
  if (diffMs < minute) return "just now";
  if (diffMs < hour) return `${Math.floor(diffMs / minute)}m ago`;
  if (diffMs < day) return `${Math.floor(diffMs / hour)}h ago`;
  if (diffMs < 7 * day) return `${Math.floor(diffMs / day)}d ago`;
  return new Date(t).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

function formatDuration(secs: number | null): string {
  if (secs === null || secs === 0) return "—";
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}
