/**
 * Crash-recovery salvage list (`/salvage`).
 *
 * Phase 69 (PR-η). Lists every cached session whose `heron_session.json`
 * does NOT carry `"status": "finalized"` — i.e., a session whose
 * orchestrator never ran the finalize path because of a SIGKILL /
 * panic / power-loss. Each row offers two actions:
 *
 *   - **Recover** — confirmation dialog → calls `heron_recover_session`
 *     → on success navigates to `/review/<id>`. PR-η ships this command
 *     as a placeholder that returns a clear "not yet wired" error;
 *     the UI surfaces that as a toast and leaves the row in place so
 *     the user can fall back to Purge.
 *   - **Purge** — confirmation dialog ("permanent") → calls
 *     `heron_purge_session` → row is removed from the list on success.
 *
 * The dialogs use Radix Dialog (same pattern as `ConsentGate.tsx`)
 * and Sonner toasts mirror the Settings pane's success/error
 * conventions. There is no batch-recovery surface, no diff modal —
 * those are deliberately out of scope per the brief.
 */

import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import * as Dialog from "@radix-ui/react-dialog";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "../components/ui/button";
import {
  invoke,
  type Settings,
  type UnfinalizedSession,
} from "../lib/invoke";

/** Pretty-print a byte count using IEC 1024-based units. */
function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB"];
  const exponent = Math.min(
    Math.floor(Math.log(bytes) / Math.log(1024)),
    units.length - 1,
  );
  const value = bytes / Math.pow(1024, exponent);
  // One decimal place once we're past bytes; integers below.
  return exponent === 0
    ? `${value.toFixed(0)} ${units[exponent]}`
    : `${value.toFixed(1)} ${units[exponent]}`;
}

/** Format an ISO 8601 timestamp for the row header. */
function formatStarted(iso: string): string {
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) {
    // Garbage-in-garbage-out: surface the raw string so the user can
    // still tell two rows apart. Very-old-mtime fallbacks (epoch 0)
    // also land here as a parseable date and render as "1970-01-01".
    return iso;
  }
  return t.toLocaleString();
}

type ConfirmKind = "recover" | "purge";

interface ConfirmTarget {
  kind: ConfirmKind;
  session: UnfinalizedSession;
}

export default function Salvage() {
  const navigate = useNavigate();
  const [sessions, setSessions] = useState<UnfinalizedSession[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<ConfirmTarget | null>(null);
  // Track *every* in-flight operation by session id so two concurrent
  // recover/purge calls on different rows don't clobber each other's
  // disabled state. A `Set` keeps the per-row check O(1).
  const [busyIds, setBusyIds] = useState<Set<string>>(() => new Set());

  function markBusy(id: string) {
    setBusyIds((prev) => {
      const next = new Set(prev);
      next.add(id);
      return next;
    });
  }
  function clearBusy(id: string) {
    setBusyIds((prev) => {
      if (!prev.has(id)) return prev;
      const next = new Set(prev);
      next.delete(id);
      return next;
    });
  }

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const result = await invoke("heron_scan_unfinalized");
        if (cancelled) return;
        setSessions(result);
        setLoadError(null);
      } catch (err) {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        setLoadError(message);
        setSessions([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  async function runRecover(session: UnfinalizedSession) {
    if (busyIds.has(session.session_id)) {
      // Belt-and-braces — the Recover button is already disabled
      // when the row is busy, but a confirmation dialog opened from
      // the same row before the click registered would otherwise be
      // racy.
      return;
    }
    markBusy(session.session_id);
    try {
      // The Rust side reads the configured vault from settings.json.
      // We pass it through here as well so a future Settings rev
      // (e.g., per-session vault override) doesn't require an IPC
      // contract change.
      const settingsPath = await invoke("heron_default_settings_path");
      const settings: Settings = await invoke("heron_read_settings", {
        settingsPath,
      });
      const vaultPath = settings.vault_root;
      await invoke("heron_recover_session", {
        sessionId: session.session_id,
        vaultPath,
      });
      // Success → navigate to the review page. The Rust side returns
      // the `.md` path; we don't render it directly, but we DO want
      // the row removed from the list so a refresh-less navigate-and-
      // back doesn't show a now-finalized row.
      setSessions((prev) =>
        (prev ?? []).filter((s) => s.session_id !== session.session_id),
      );
      toast.success("Session recovered");
      navigate(`/review/${encodeURIComponent(session.session_id)}`);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Recovery failed: ${message}`);
    } finally {
      clearBusy(session.session_id);
    }
  }

  async function runPurge(session: UnfinalizedSession) {
    if (busyIds.has(session.session_id)) {
      return;
    }
    markBusy(session.session_id);
    try {
      await invoke("heron_purge_session", { sessionId: session.session_id });
      setSessions((prev) =>
        (prev ?? []).filter((s) => s.session_id !== session.session_id),
      );
      toast.success("Session purged");
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Purge failed: ${message}`);
    } finally {
      clearBusy(session.session_id);
    }
  }

  if (sessions === null) {
    return (
      <main className="p-6">
        <div className="flex items-center gap-2 text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
          Scanning cache for unfinalized sessions…
        </div>
      </main>
    );
  }

  return (
    <main className="p-6 space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold">Salvage</h1>
        <Link to="/home" className="text-sm underline text-muted-foreground">
          Back to home
        </Link>
      </div>

      <p className="text-sm text-muted-foreground">
        Sessions whose recordings finished without a final summary. Pick
        Recover to re-run the orchestrator on the cached audio +
        transcript, or Purge to delete the cache directory permanently.
      </p>

      {loadError && (
        <p className="text-sm text-destructive">
          Could not scan cache: {loadError}
        </p>
      )}

      {sessions.length === 0 ? (
        <p className="text-muted-foreground italic">Nothing to recover.</p>
      ) : (
        <ul className="space-y-3">
          {sessions.map((session) => (
            <li
              key={session.session_id}
              className="flex items-center justify-between gap-4 rounded-md border border-border p-3"
            >
              <div className="min-w-0 flex-1">
                <p className="font-mono text-sm truncate">
                  {session.session_id}
                </p>
                <p className="text-xs text-muted-foreground">
                  Started {formatStarted(session.started_at)} ·{" "}
                  {formatBytes(session.audio_bytes)} audio
                  {session.has_partial_transcript
                    ? " · partial transcript"
                    : ""}
                </p>
              </div>
              <div className="flex shrink-0 gap-2">
                <Button
                  onClick={() =>
                    setConfirm({ kind: "recover", session })
                  }
                  disabled={busyIds.has(session.session_id)}
                >
                  Recover
                </Button>
                <Button
                  variant="destructive"
                  onClick={() => setConfirm({ kind: "purge", session })}
                  disabled={busyIds.has(session.session_id)}
                >
                  Purge
                </Button>
              </div>
            </li>
          ))}
        </ul>
      )}

      <ConfirmDialog
        target={confirm}
        busy={confirm !== null && busyIds.has(confirm.session.session_id)}
        onCancel={() => setConfirm(null)}
        onConfirm={async (target) => {
          setConfirm(null);
          if (target.kind === "recover") {
            await runRecover(target.session);
          } else {
            await runPurge(target.session);
          }
        }}
      />
    </main>
  );
}

interface ConfirmDialogProps {
  target: ConfirmTarget | null;
  busy: boolean;
  onCancel: () => void;
  onConfirm: (target: ConfirmTarget) => Promise<void>;
}

function ConfirmDialog({
  target,
  busy,
  onCancel,
  onConfirm,
}: ConfirmDialogProps) {
  const open = target !== null;
  // Headline + body copy diverges per action so the user knows what's
  // about to happen — Recover is reversible (re-running finalize is
  // idempotent), Purge is not.
  const headline =
    target?.kind === "purge" ? "Purge this session?" : "Recover this session?";
  const description =
    target?.kind === "purge"
      ? "This deletes the audio + cached transcripts permanently. There is no undo."
      : "Heron will re-run the summarizer against the cached audio + transcript and write the markdown into your vault.";
  const confirmLabel = target?.kind === "purge" ? "Purge" : "Recover";

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next && !busy) {
          onCancel();
        }
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/40 backdrop-blur-sm" />
        <Dialog.Content
          className={
            "fixed left-1/2 top-1/2 w-[min(440px,90vw)] -translate-x-1/2 " +
            "-translate-y-1/2 rounded-lg bg-background p-6 shadow-xl " +
            "border border-border space-y-4"
          }
          aria-describedby="salvage-confirm-description"
        >
          <Dialog.Title className="text-lg font-semibold">
            {headline}
          </Dialog.Title>
          <Dialog.Description
            id="salvage-confirm-description"
            className="text-sm text-muted-foreground"
          >
            {description}
          </Dialog.Description>
          {target && (
            <p className="font-mono text-xs break-all text-muted-foreground">
              {target.session.session_id}
            </p>
          )}
          <div className="flex justify-end gap-2 pt-2">
            <Button variant="ghost" onClick={onCancel} disabled={busy}>
              Cancel
            </Button>
            <Button
              variant={target?.kind === "purge" ? "destructive" : "default"}
              disabled={busy || target === null}
              onClick={() => {
                if (target) {
                  void onConfirm(target);
                }
              }}
            >
              {busy ? (
                <>
                  <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
                  Working…
                </>
              ) : (
                confirmLabel
              )}
            </Button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
