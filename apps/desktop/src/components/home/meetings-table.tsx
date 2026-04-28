import { useMemo } from "react";
import { useNavigate } from "react-router-dom";

import { Avatar } from "../ui/avatar";
import { useMeetingsStore } from "../../store/meetings";
import type { Meeting, MeetingStatus, Platform } from "../../lib/types";

interface MeetingsTableProps {
  query: string;
  filter: StatusFilter;
}

export type StatusFilter = "all" | "active" | "done";

export function MeetingsTable({ query, filter }: MeetingsTableProps) {
  const items = useMeetingsStore((s) => s.items);
  const loading = useMeetingsStore((s) => s.loading);

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return items.filter((m) => {
      if (filter === "active" && (m.status === "done" || m.status === "failed")) {
        return false;
      }
      if (filter === "done" && m.status !== "done") {
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
      return matchesTitle || matchesPlatform || matchesParticipant;
    });
  }, [items, query, filter]);

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
            <Row key={m.id} meeting={m} />
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Row({ meeting }: { meeting: Meeting }) {
  const navigate = useNavigate();
  const fetchSummary = useMeetingsStore((s) => s.fetchSummary);
  const summary = useMeetingsStore((s) => s.summaries[meeting.id]);

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
