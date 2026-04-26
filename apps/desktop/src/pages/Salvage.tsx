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
 * Phase 75 (PR-ν) extends the page with a **Purge all** affordance —
 * a top-right button gated behind a two-step confirmation modal that
 * iterates the list calling `heron_purge_session` per session_id with
 * `Promise.allSettled` (so a single failure doesn't abort the rest).
 * The store-tracked unfinalized count is kept in sync so the
 * `<SalvageBanner />` in the app shell disappears as soon as the list
 * empties.
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
import { useSalvagePromptStore } from "../store/salvage";

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

/** Pluralise the noun "session" against an integer count. */
function sessionsNoun(count: number): string {
  return count === 1 ? "session" : "sessions";
}

/**
 * Compact summary of a failure list for a Sonner toast description.
 * The toast is single-line by default and a 30-row purge with full
 * error messages would push other toasts off-screen; we cap to the
 * first three ids and append "+N more" when truncated. Full details
 * land in `console.warn` for power-user diagnosis.
 */
function summariseFailedIds(ids: readonly string[]): string {
  const MAX_INLINE = 3;
  if (ids.length <= MAX_INLINE) {
    return ids.join(", ");
  }
  const head = ids.slice(0, MAX_INLINE).join(", ");
  return `${head} (+${ids.length - MAX_INLINE} more)`;
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
  const setUnfinalizedCount = useSalvagePromptStore(
    (s) => s.setUnfinalizedCount,
  );
  const [sessions, setSessions] = useState<UnfinalizedSession[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<ConfirmTarget | null>(null);
  // Two-step "Purge all" confirmation gate (PR-ν / phase 75). `null`
  // means the modal is closed; `true` means the user clicked "Purge
  // all" and is now looking at the destructive-confirm pane.
  const [purgeAllOpen, setPurgeAllOpen] = useState(false);
  // Track a "batch in progress" flag so per-row buttons disable while
  // the bulk purge is running — interleaved per-row + batch operations
  // would race the `setSessions` writes below.
  const [batchInFlight, setBatchInFlight] = useState(false);
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

  /**
   * Drop the named session ids from the visible list. The companion
   * effect below mirrors the new length into the salvage-prompt store
   * so the app-shell `<SalvageBanner />` stays in sync without any
   * caller having to remember the second call.
   *
   * Pulled out so the per-row + batch handlers all share one
   * mutation point and the effect's `sessions` dep covers every
   * removal path (initial scan, single-row purge, single-row recover,
   * batch purge).
   */
  function removeSessions(ids: ReadonlySet<string>) {
    setSessions((prev) => (prev ?? []).filter((s) => !ids.has(s.session_id)));
  }

  // Mirror the visible-list length into the prompt store so a
  // `<SalvageBanner />` mounted in the app shell sees the same count
  // we render here. Skipping the write while `sessions === null` (the
  // pre-scan state) avoids stomping on the count the App-mount scan
  // wrote — the banner shouldn't blink to zero just because `/salvage`
  // is mid-fetch.
  useEffect(() => {
    if (sessions === null) {
      return;
    }
    setUnfinalizedCount(sessions.length);
  }, [sessions, setUnfinalizedCount]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const result = await invoke("heron_scan_unfinalized");
        if (cancelled) return;
        setSessions(result);
        setLoadError(null);
        // The companion effect on `sessions` mirrors the new length
        // into the prompt store, so navigating here from the tray
        // and finding zero sessions drops the banner without an
        // explicit second write here.
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
      removeSessions(new Set([session.session_id]));
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
      removeSessions(new Set([session.session_id]));
      toast.success("Session purged");
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Purge failed: ${message}`);
    } finally {
      clearBusy(session.session_id);
    }
  }

  /**
   * Bulk-purge every row currently in the list (PR-ν / phase 75).
   *
   * Uses `Promise.allSettled` rather than `Promise.all` so a single
   * `heron_purge_session` rejection (e.g. EBUSY on a still-mmapped
   * file) doesn't strand the rest. Each successful row is removed
   * from the list once the whole batch has settled; failures are
   * collected and surfaced in a single error toast at the end.
   *
   * The function snapshots `sessions` up-front: if the user races a
   * per-row Purge button against the batch, the per-row click is
   * already disabled by `batchInFlight`, but the snapshot guarantees
   * we don't see a `null` mid-iteration if React batches a state
   * update behind us.
   *
   * **Concurrency note.** All purges fire in parallel via
   * `Promise.allSettled`. Tauri's IPC thread pool (~4 threads by
   * default) bounds the actual concurrency on the Rust side, so
   * dispatching N promises here is not the same as N concurrent
   * filesystem operations. For realistic N (a handful of crashed
   * sessions), parallel dispatch is the right shape; if a future
   * pathological user accumulates hundreds of unfinalized sessions,
   * we should chunk via a worker pool — but that's not the steady
   * state the brief targets.
   */
  async function runPurgeAll() {
    setPurgeAllOpen(false);
    const snapshot = sessions ?? [];
    if (snapshot.length === 0) {
      return;
    }
    setBatchInFlight(true);
    const total = snapshot.length;
    // Mark every row busy so the per-row spinners render. We clear
    // them all at the end in a single sweep rather than per-resolve;
    // that keeps the UI consistent with the "batch is one operation"
    // mental model and avoids a flicker when a fast row resolves
    // before its sibling.
    setBusyIds((prev) => {
      const next = new Set(prev);
      for (const s of snapshot) {
        next.add(s.session_id);
      }
      return next;
    });

    try {
      const outcomes = await Promise.allSettled(
        snapshot.map((s) =>
          invoke("heron_purge_session", { sessionId: s.session_id }).then(
            () => s.session_id,
          ),
        ),
      );

      const purged = new Set<string>();
      const failures: { sessionId: string; message: string }[] = [];
      outcomes.forEach((outcome, index) => {
        const sessionId = snapshot[index]?.session_id;
        if (sessionId === undefined) {
          // Defensive: `forEach` over `outcomes` mirrors `snapshot`
          // 1:1, so this branch is unreachable in practice. Keeps the
          // index access type-safe under `noUncheckedIndexedAccess`.
          return;
        }
        if (outcome.status === "fulfilled") {
          purged.add(sessionId);
        } else {
          const reason = outcome.reason;
          const message =
            reason instanceof Error ? reason.message : String(reason);
          failures.push({ sessionId, message });
        }
      });

      removeSessions(purged);

      if (failures.length === 0) {
        toast.success(
          `Purged ${purged.size} of ${total} ${sessionsNoun(total)}`,
        );
      } else if (purged.size === 0) {
        // Every row failed — show a generic error rather than a
        // per-row dump (which could push other toasts off-screen).
        toast.error(`Purge failed for all ${total} ${sessionsNoun(total)}`, {
          description: summariseFailedIds(failures.map((f) => f.sessionId)),
        });
      } else {
        toast.error(
          `Purged ${purged.size} of ${total}; ${failures.length} failed`,
          {
            description: summariseFailedIds(failures.map((f) => f.sessionId)),
          },
        );
      }
      if (failures.length > 0) {
        // Belt-and-braces: also log each failure to the console so a
        // developer / power user can pull the full list out of devtools
        // when the toast description is truncated.
        // eslint-disable-next-line no-console
        console.warn("[heron] purge failures:", failures);
      }
    } finally {
      // Clear the busy flags for every row that was in the snapshot —
      // some of those ids may already be gone from the list (if the
      // call succeeded), but `clearBusy`-style filtering on a missing
      // entry is a no-op.
      setBusyIds((prev) => {
        const next = new Set(prev);
        for (const s of snapshot) {
          next.delete(s.session_id);
        }
        return next;
      });
      setBatchInFlight(false);
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

  const hasRows = sessions.length > 0;

  return (
    <main className="p-6 space-y-4">
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-2xl font-semibold">Salvage</h1>
        <div className="flex items-center gap-3">
          <Button
            variant="destructive"
            size="sm"
            // Disabled when:
            //   - the list is empty (nothing to purge),
            //   - a batch is already running (re-clicking would
            //     race the in-flight `Promise.allSettled`), or
            //   - any per-row operation is in flight (a stuck
            //     spinner on a row would race the snapshot-based
            //     batch path).
            disabled={!hasRows || batchInFlight || busyIds.size > 0}
            onClick={() => setPurgeAllOpen(true)}
          >
            {batchInFlight ? (
              <>
                <Loader2
                  className="h-4 w-4 animate-spin"
                  aria-hidden="true"
                />
                Purging…
              </>
            ) : (
              "Purge all"
            )}
          </Button>
          <Link to="/home" className="text-sm underline text-muted-foreground">
            Back to home
          </Link>
        </div>
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

      {!hasRows ? (
        <p className="text-muted-foreground italic">Nothing to recover.</p>
      ) : (
        <ul className="space-y-3">
          {sessions.map((session) => {
            const rowBusy = busyIds.has(session.session_id);
            return (
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
                <div className="flex shrink-0 items-center gap-2">
                  {rowBusy && (
                    // The disabled Recover/Purge buttons next to the
                    // spinner already convey "operation in flight" to
                    // screen readers; matching the rest of the file's
                    // `aria-hidden="true"` spinners avoids a redundant
                    // announcement next to a non-interactive element.
                    <Loader2
                      className="h-4 w-4 animate-spin text-muted-foreground"
                      aria-hidden="true"
                    />
                  )}
                  <Button
                    onClick={() =>
                      setConfirm({ kind: "recover", session })
                    }
                    disabled={rowBusy || batchInFlight}
                  >
                    Recover
                  </Button>
                  <Button
                    variant="destructive"
                    onClick={() => setConfirm({ kind: "purge", session })}
                    disabled={rowBusy || batchInFlight}
                  >
                    Purge
                  </Button>
                </div>
              </li>
            );
          })}
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

      <PurgeAllDialog
        open={purgeAllOpen}
        count={sessions.length}
        busy={batchInFlight}
        onCancel={() => setPurgeAllOpen(false)}
        onConfirm={() => {
          void runPurgeAll();
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

interface PurgeAllDialogProps {
  open: boolean;
  count: number;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}

/**
 * Two-step confirmation modal for "Purge all" (PR-ν / phase 75).
 *
 * Bulk destructive operations get the same dialog primitive as the
 * per-row Purge, but with copy that names the count + the irreversible
 * nature explicitly. We deliberately do NOT add a "type the count to
 * confirm" interlock — the count is shown in the headline and the
 * destructive button colour, which is the same level of friction the
 * per-row Purge applies for a single session.
 */
function PurgeAllDialog({
  open,
  count,
  busy,
  onCancel,
  onConfirm,
}: PurgeAllDialogProps) {
  const sessionsLabel = sessionsNoun(count);
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
            "fixed left-1/2 top-1/2 w-[min(480px,90vw)] -translate-x-1/2 " +
            "-translate-y-1/2 rounded-lg bg-background p-6 shadow-xl " +
            "border border-border space-y-4"
          }
          aria-describedby="salvage-purge-all-description"
        >
          <Dialog.Title className="text-lg font-semibold">
            Purge all {count} unfinalized {sessionsLabel}?
          </Dialog.Title>
          <Dialog.Description
            id="salvage-purge-all-description"
            className="text-sm text-muted-foreground"
          >
            Audio + cached transcripts will be permanently deleted. This
            cannot be undone.
          </Dialog.Description>
          <div className="flex justify-end gap-2 pt-2">
            <Button variant="ghost" onClick={onCancel} disabled={busy}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={busy || count === 0}
              onClick={onConfirm}
            >
              {busy ? (
                <>
                  <Loader2
                    className="h-4 w-4 animate-spin"
                    aria-hidden="true"
                  />
                  Purging…
                </>
              ) : (
                `Purge all ${count}`
              )}
            </Button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
