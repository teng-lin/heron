import {
  BookOpen,
  Home,
  Mic,
  Speaker,
  Square,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { useLocation, useNavigate } from "react-router-dom";

import { setActiveMode, useActiveMode } from "../../store/mode";
import { useMeetingsStore } from "../../store/meetings";
import { useRecordingStore } from "../../store/recording";
import { Avatar } from "../ui/avatar";
import { cn } from "../../lib/cn";

interface NavItemProps {
  to: string;
  icon: LucideIcon;
  label: string;
  count?: string | number | null;
  selected: boolean;
  onClick: () => void;
}

function NavItem({
  icon: Icon,
  label,
  count,
  selected,
  onClick,
}: NavItemProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "mx-2 flex w-[calc(100%-1rem)] items-center gap-2.5 rounded px-2.5 py-1.5 text-left text-[12.5px] transition-colors",
      )}
      style={{
        background: selected ? "var(--color-paper-3)" : "transparent",
        color: selected ? "var(--color-ink)" : "var(--color-ink-2)",
        fontFamily: "var(--font-sans)",
        fontWeight: selected ? 500 : 400,
      }}
    >
      <span
        className="inline-flex"
        style={{
          color: selected ? "var(--color-accent)" : "var(--color-ink-3)",
        }}
      >
        <Icon size={13} />
      </span>
      <span className="flex-1">{label}</span>
      {count != null && (
        <span
          className="font-mono text-[10.5px]"
          style={{ color: "var(--color-ink-4)" }}
        >
          {count}
        </span>
      )}
    </button>
  );
}

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <div
      className="font-mono text-[10.5px] uppercase tracking-[0.12em]"
      style={{
        padding: "16px 18px 6px",
        color: "var(--color-ink-3)",
      }}
    >
      {children}
    </div>
  );
}

/**
 * Left chrome.
 *
 * In PR 2 the meeting/people data sources do not exist yet. The plan
 * mandates that we render `—` for All-meetings/Drafts placeholders and
 * hide the People section entirely until the data substrate lands
 * (PR 3 introduces `useMeetingsStore`, which then unblocks the live
 * counts; the People aggregate is deferred past PR 6).
 */
export function Sidebar() {
  const navigate = useNavigate();
  const location = useLocation();
  const recordingStart = useRecordingStore((s) => s.recordingStart);
  const mode = useActiveMode();
  const meetings = useMeetingsStore((s) => s.items);
  const daemonDown = useMeetingsStore((s) => s.daemonDown);

  const allMeetingsCount: string | number = daemonDown ? "—" : meetings.length;
  const draftsCount: string | number = daemonDown
    ? "—"
    : meetings.filter((m) => m.status === "detected").length;

  const isOn = (path: string) => location.pathname.startsWith(path);

  return (
    <aside
      className="flex shrink-0 flex-col overflow-hidden border-r"
      style={{
        width: 222,
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
      }}
    >
      <div className="flex items-center gap-2 px-3.5 pt-3 pb-1.5">
        <RecordButton recording={recordingStart !== null} />
      </div>

      <SectionLabel>Library</SectionLabel>
      <NavItem
        to="/home"
        icon={Home}
        label="All meetings"
        count={allMeetingsCount}
        selected={isOn("/home")}
        onClick={() => navigate("/home")}
      />
      <NavItem
        to="/recording"
        icon={Mic}
        label="In progress"
        count={recordingStart !== null ? 1 : 0}
        selected={isOn("/recording")}
        onClick={() => navigate("/recording")}
      />
      <NavItem
        to="/home"
        icon={BookOpen}
        label="Drafts"
        count={draftsCount}
        selected={false}
        onClick={() => navigate("/home")}
      />

      <SectionLabel>Modes</SectionLabel>
      <NavItem
        to="/athena"
        icon={Zap}
        label={mode === "athena" ? "Athena live" : "Athena"}
        selected={isOn("/athena")}
        onClick={() => {
          void setActiveMode("athena");
          navigate("/athena");
        }}
      />
      <NavItem
        to="/pollux"
        icon={Speaker}
        label={mode === "pollux" ? "Pollux setup" : "Pollux"}
        selected={isOn("/pollux")}
        onClick={() => {
          void setActiveMode("pollux");
          navigate("/pollux");
        }}
      />

      {/*
       * People section deferred — no Person aggregate exists yet.
       * PR 6+ ships this once Athena's identity model lands.
       * TODO(athena): wire to a `usePeopleStore` once the daemon
       * exposes participant aggregation.
       */}
      <div className="flex-1" />

      <div
        className="flex items-center gap-2 border-t px-3.5 py-2.5"
        style={{
          background: "var(--color-paper)",
          borderColor: "var(--color-rule)",
        }}
      >
        <Avatar name="You" size={22} />
        <div className="min-w-0 flex-1">
          <div
            className="text-xs font-medium"
            style={{ color: "var(--color-ink)" }}
          >
            you@local
          </div>
          <div
            className="font-mono text-[10px] truncate"
            style={{ color: "var(--color-ink-3)" }}
          >
            local-only
          </div>
        </div>
        <span
          className="inline-flex items-center gap-1 rounded-full border px-1.5 py-0.5 font-mono text-[9px] uppercase tracking-[0.08em]"
          style={{
            color: "var(--color-ink-3)",
            borderColor: "var(--color-rule-2)",
            background: "var(--color-paper)",
          }}
        >
          <span
            className="inline-block"
            style={{
              width: 5,
              height: 5,
              borderRadius: "50%",
              background: "var(--color-ok)",
            }}
          />
          local
        </span>
      </div>
    </aside>
  );
}

function RecordButton({ recording }: { recording: boolean }) {
  return (
    <button
      type="button"
      className="inline-flex w-full items-center gap-1.5 rounded px-2.5 py-1.5 text-xs text-white"
      style={{
        background: recording ? "var(--color-ink-2)" : "var(--color-rec)",
      }}
      disabled
      title="Recording is started from Home (PR 3+)"
    >
      {recording ? (
        <Square size={11} />
      ) : (
        <span
          className="inline-block"
          style={{
            width: 8,
            height: 8,
            borderRadius: "50%",
            background: "white",
          }}
        />
      )}
      <span>{recording ? "Stop" : "Start recording"}</span>
      <span
        className="ml-auto font-mono text-[9.5px] tracking-[0.06em]"
        style={{ opacity: 0.7 }}
      >
        ⌘⇧R
      </span>
    </button>
  );
}
