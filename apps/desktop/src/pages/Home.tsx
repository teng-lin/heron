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

import { useEffect, useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { Search } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { DaemonDownBanner } from "../components/DaemonDownBanner";
import { MeetingsTable, type StatusFilter } from "../components/home/meetings-table";
import { Button } from "../components/ui/button";
import {
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../components/ui/dialog";
import { invoke } from "../lib/invoke";
import { cn } from "../lib/cn";
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
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<StatusFilter>("all");

  useEffect(() => {
    void loadMeetings();
  }, [loadMeetings]);

  async function proceedToConsent() {
    const decision = await requestConsent();
    if (decision !== "confirmed") {
      return;
    }
    // Gap #7: ask the daemon to actually start a capture before we
    // navigate. Pre-PR the button only flipped local recording-store
    // state; now Start = `POST /v1/meetings`. The platform default is
    // Zoom — same escape-hatch behaviour as `heron-cli`. A future
    // patch can preselect from the most recent `meeting.detected`
    // event or surface a picker; for v1 the most common case (a
    // running Zoom call) is the right default.
    let outcome;
    try {
      outcome = await invoke("heron_start_capture", { platform: "zoom" });
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

  async function onStart() {
    try {
      const settingsPath = await invoke("heron_default_settings_path");
      const outcome = await invoke("heron_check_disk_for_recording", {
        settingsPath,
      });
      if (outcome.kind === "below_threshold") {
        setDiskWarning({
          freeMib: outcome.free_mib,
          thresholdMib: outcome.threshold_mib,
        });
        return;
      }
    } catch (err) {
      // Pre-flight check is non-blocking on IPC failure.
      // eslint-disable-next-line no-console
      console.warn("[heron] disk pre-flight failed at start:", err);
    }
    await proceedToConsent();
  }

  async function continueAnyway() {
    setDiskWarning(null);
    await proceedToConsent();
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
            <Button onClick={() => void onStart()}>Start recording</Button>
          </div>
        </header>

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
        </div>

        <MeetingsTable query={search} filter={filter} />
      </main>

      <Dialog.Root
        open={diskWarning !== null}
        onOpenChange={(next) => {
          if (!next) setDiskWarning(null);
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
            <Button variant="ghost" onClick={() => setDiskWarning(null)}>
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
