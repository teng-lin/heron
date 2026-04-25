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
 * Active session is highlighted by comparing the route param to each
 * basename in the list.
 */

import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";

import { invoke } from "../lib/invoke";
import { cn } from "../lib/cn";
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
          <p className="text-xs text-muted-foreground p-2">
            No sessions yet.
          </p>
        )}
        {vault &&
          sessions?.map((id) => {
            const active = id === activeSessionId;
            return (
              <button
                key={id}
                type="button"
                onClick={() => navigate(`/review/${encodeURIComponent(id)}`)}
                className={cn(
                  "w-full text-left text-sm px-2 py-1.5 rounded-md truncate",
                  active
                    ? "bg-primary text-primary-foreground"
                    : "hover:bg-muted text-foreground",
                )}
                title={id}
              >
                {id}
              </button>
            );
          })}
      </div>
    </aside>
  );
}
