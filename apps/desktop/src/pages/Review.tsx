/**
 * Review route — `/review/:sessionId`.
 *
 * Phase 65 (PR-γ) shipped: sessions sidebar, TipTap markdown editor,
 * read-only transcript view, save-on-blur + ⌘S.
 *
 * Phase 67 (PR-ε) layers on:
 * - Sticky audio playback bar (PR-γ′ deferred-out item).
 * - Click-transcript-to-jump: clicking a row's clock seeks playback.
 * - Diagnostics tab (Radix Tabs around the existing Note view).
 * - Re-summarize button + confirmation dialog.
 * - `.md.bak` rollback pill — visible only when a backup is on disk.
 *
 * Phase 76 (PR-ξ) brings the diff-view-before-accepting checkbox
 * forward from §15 v1.1: the Re-summarize flow now confirms, fetches
 * the post-merge preview via `heron_resummarize_preview` (read-only),
 * shows a side-by-side diff modal, and only writes when the user
 * clicks Apply — which fires `heron_resummarize` (rotate + write).
 * Cancel discards the preview without touching disk.
 *
 * Out of scope (deferred per plan.md §15):
 * - Edit history beyond a single `.md.bak`.
 * - Live audio playback while recording (only finalized files).
 * - Inline (non-split) diff toggle, ignore-whitespace toggle.
 * - Three-way diff against `.md.bak`.
 *
 * The vault path comes from `useSettingsStore`; the cache root is
 * resolved once via `heron_default_cache_root` and cached on the
 * component. We don't promote it to a Zustand store yet — the cache
 * root never changes during the app's lifetime, so a single resolve
 * is enough.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { toast } from "sonner";
import { RotateCcw } from "lucide-react";

import { NoteEditor, type NoteEditorHandle } from "../components/NoteEditor";
import { SessionsSidebar } from "../components/SessionsSidebar";
import { TranscriptView } from "../components/TranscriptView";
import { PlaybackBar, type PlaybackBarHandle } from "../components/PlaybackBar";
import { DiagnosticsPanel } from "../components/DiagnosticsPanel";
import { ResummarizeDialog } from "../components/ResummarizeDialog";
import { ResummarizeDiffModal } from "../components/ResummarizeDiffModal";
import { Button } from "../components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../components/ui/tabs";
import { invoke, type BackupInfo } from "../lib/invoke";
import { useSettingsStore } from "../store/settings";

type LoadState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; markdown: string }
  | { kind: "error"; message: string };

/**
 * The `<id>.md` content key the TipTap editor remounts against. We
 * bump it on a successful re-summarize / restore so the editor
 * resets cleanly to the new content; without the bump TipTap would
 * keep the user's pre-resummarize doc state in memory.
 */
type EditorKey = string;

/**
 * Format an ISO/RFC3339 timestamp for the `.md.bak` pill. Falls back
 * to the raw string if `Intl.DateTimeFormat` rejects the input.
 */
function formatBackupTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(d);
}

export default function Review() {
  const { sessionId } = useParams<{ sessionId: string }>();
  // PR-λ (phase 73): the tray-degraded toast's "View diagnostics"
  // action navigates to `/review/<id>?tab=diagnostics`. Read the
  // query param so the route lands on the right Tabs section instead
  // of the default Note view. Unknown / missing values fall through
  // to "note".
  const [searchParams] = useSearchParams();
  const initialTab = searchParams.get("tab") === "diagnostics" ? "diagnostics" : "note";
  const settings = useSettingsStore((s) => s.settings);
  const ensureLoaded = useSettingsStore((s) => s.ensureLoaded);
  const settingsLoading = useSettingsStore((s) => s.loading);
  const settingsError = useSettingsStore((s) => s.error);

  const [load, setLoad] = useState<LoadState>({ kind: "idle" });
  // Live mirror of the editor's markdown so the transcript view
  // updates as the user edits. Updated by the editor's `onUpdate`,
  // separately from the file-system save (which only fires on blur
  // or ⌘S).
  const [liveMarkdown, setLiveMarkdown] = useState<string>("");
  const [savedKey, setSavedKey] = useState(0);
  const editorRef = useRef<NoteEditorHandle | null>(null);
  const playbackRef = useRef<PlaybackBarHandle | null>(null);
  // Last successfully-saved markdown for this notePath. We compare
  // against this on save attempts so blurring an unchanged document
  // doesn't churn the disk or chime "Saved" repeatedly.
  const lastSavedRef = useRef<string | null>(null);
  // Monotonic save token. A save started later wins over an older
  // save still in flight — even if their POSIX renames land in the
  // wrong order, the older one's success path is gated on its token
  // still matching the latest.
  const saveGenRef = useRef(0);

  // Editor-content nonce: bumping this changes the editor's `key`,
  // forcing TipTap to remount with the freshly-loaded `markdown`.
  // Used after re-summarize / restore so the editor doesn't keep
  // the prior body's state. The string includes the load source so
  // a normal reload also resets cleanly.
  const [editorContentKey, setEditorContentKey] = useState<EditorKey>("v0");

  // Cache root for the diagnostics + playback paths. Resolved once
  // and held — it doesn't change during the app's lifetime.
  const [cacheRoot, setCacheRoot] = useState<string | null>(null);

  // `.md.bak` backup state for the Restore pill.
  const [backup, setBackup] = useState<BackupInfo | null>(null);
  // PR-ξ (phase 76) Re-summarize flow has three open-states:
  //   1. `resummarizeOpen`  — initial confirmation dialog.
  //   2. `diffOpen`         — diff modal (spinner → preview → Apply).
  //   3. neither            — idle.
  // And two in-flight flags that disable buttons during invokes:
  //   - `previewInFlight`   — `heron_resummarize_preview` running.
  //   - `applyInFlight`     — `heron_resummarize` running.
  // Splitting them lets the diff modal show the spinner during
  // preview-fetch and disable buttons during apply, without each
  // phase having to know about the other's invoke state.
  const [resummarizeOpen, setResummarizeOpen] = useState(false);
  const [previewInFlight, setPreviewInFlight] = useState(false);
  const [applyInFlight, setApplyInFlight] = useState(false);
  // `diffPreview` is `null` while the preview invoke is running (the
  // modal renders a spinner) and the rendered post-merge string once
  // it resolves. `diffCurrent` is the body the editor was last
  // mounted against — captured at preview time so the comparison is
  // against the version the user actually sees, not whatever a
  // concurrent save might have written between Confirm and the modal
  // opening.
  const [diffOpen, setDiffOpen] = useState(false);
  const [diffPreview, setDiffPreview] = useState<string | null>(null);
  const [diffCurrent, setDiffCurrent] = useState<string | null>(null);
  // AbortController for the in-flight preview invoke. Cancel during
  // the loading window aborts the underlying invoke promise so a
  // wasted summarizer call doesn't keep running after the user
  // bailed — and a stale preview can't land on the modal after the
  // user closed it (we drop the result if `signal.aborted`).
  const previewAbortRef = useRef<AbortController | null>(null);
  // Aggregate flag the toolbar Re-summarize button reads to disable
  // itself across the whole flow (open the confirmation dialog,
  // preview fetch, diff modal, apply commit). Computed rather than
  // stored so it can never drift out of sync with the underlying flags.
  const resummarizeBusy =
    resummarizeOpen || diffOpen || previewInFlight || applyInFlight;

  useEffect(() => {
    void ensureLoaded();
  }, [ensureLoaded]);

  // PR-ξ (phase 76): abort any in-flight preview invoke on unmount.
  // Without this, navigating away from `/review/<id>` while the
  // summarizer is still running leaves the controller in the ref and
  // the eventual promise resolution updates state on a dead
  // component. The abort signals "drop the result on the floor"
  // (the underlying Tauri invoke can't truly cancel — see
  // `onCancelDiff`'s comment for the rationale).
  useEffect(() => {
    return () => {
      previewAbortRef.current?.abort();
    };
  }, []);

  // Resolve the cache root once on mount. The fallback to `""` on
  // failure means the playback bar and diagnostics panel both render
  // their empty/error states rather than firing a malformed path.
  useEffect(() => {
    let cancelled = false;
    invoke("heron_default_cache_root")
      .then((path) => {
        if (cancelled) return;
        setCacheRoot(path);
      })
      .catch(() => {
        if (cancelled) return;
        setCacheRoot("");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Settings has loaded successfully but the user hasn't picked a
  // vault yet (`vault_root` is the empty string). Distinct from
  // `settings === null && settingsLoading`, which is the initial
  // pre-load tick.
  const settingsReady = settings !== null;
  const vaultRoot = settings?.vault_root ?? "";

  // Re-fetch the backup state whenever vault/session changes or a
  // re-summarize/restore lands. Bumping `savedKey` here would also
  // trigger this — the explicit nonce is what makes the pill
  // refresh after a successful re-summarize.
  const refreshBackup = useCallback(async () => {
    if (!vaultRoot || !sessionId) {
      setBackup(null);
      return;
    }
    try {
      const info = await invoke("heron_check_backup", {
        vaultPath: vaultRoot,
        sessionId,
      });
      setBackup(info);
    } catch {
      // A traversal-rejection here would mean a misrouted session id —
      // not a user-actionable error. Hide the pill silently rather
      // than toasting a confusing message.
      setBackup(null);
    }
  }, [vaultRoot, sessionId]);

  // Load the current note whenever the session or vault changes.
  useEffect(() => {
    let cancelled = false;
    if (!vaultRoot || !sessionId) {
      setLoad({ kind: "idle" });
      setLiveMarkdown("");
      lastSavedRef.current = null;
      setBackup(null);
      return () => {
        cancelled = true;
      };
    }
    setLoad({ kind: "loading" });
    setEditorContentKey(`load:${vaultRoot}:${sessionId}`);
    invoke("heron_read_note", { vaultPath: vaultRoot, sessionId })
      .then((markdown) => {
        if (cancelled) return;
        setLoad({ kind: "ready", markdown });
        setLiveMarkdown(markdown);
        lastSavedRef.current = markdown;
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        setLoad({ kind: "error", message });
      });
    void refreshBackup();
    return () => {
      cancelled = true;
    };
  }, [vaultRoot, sessionId, refreshBackup]);

  const save = useCallback(
    async (markdown: string) => {
      if (!vaultRoot || !sessionId) {
        toast.error("No vault configured — cannot save.");
        return;
      }
      // Skip no-op saves so blurring an unedited note doesn't chime
      // a Saved toast every time.
      if (lastSavedRef.current !== null && markdown === lastSavedRef.current) {
        return;
      }
      const generation = ++saveGenRef.current;
      try {
        await invoke("heron_write_note_atomic", {
          vaultPath: vaultRoot,
          sessionId,
          contents: markdown,
        });
        if (generation !== saveGenRef.current) {
          // A newer save started after we awaited. Its result is
          // the canonical one — don't toast a stale success.
          return;
        }
        lastSavedRef.current = markdown;
        toast.success("Saved");
        setSavedKey((k) => k + 1);
      } catch (err) {
        if (generation !== saveGenRef.current) return;
        const message = err instanceof Error ? err.message : String(err);
        toast.error(`Save failed: ${message}`);
      }
    },
    [vaultRoot, sessionId],
  );

  // ⌘S / Ctrl+S to save. Only the editor (via ref) holds the live
  // doc — `load.markdown` is the on-mount snapshot and would lose
  // every keystroke since. The handler depends only on `save`, not
  // on `load`, so the listener doesn't churn on every load-state
  // transition.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const isSave =
        (e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "s";
      if (!isSave) return;
      e.preventDefault();
      const md = editorRef.current?.getMarkdown();
      if (md === undefined) return;
      void save(md);
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [save]);

  // PR-ξ (phase 76) Confirm-button handler. Replaces the previous
  // direct `heron_resummarize` call with a two-step flow:
  //   1. Flush any unsaved edits to disk.
  //   2. Read the current note + fetch the post-merge preview in
  //      parallel.
  //   3. Open the diff modal and let the user click Apply or Cancel.
  // The actual write happens in `onApplyResummarize` below.
  const onConfirmResummarize = useCallback(async () => {
    if (!vaultRoot || !sessionId) {
      toast.error("No vault configured — cannot re-summarize.");
      return;
    }
    // If a previous preview was somehow still in flight (e.g. user
    // hit Cancel and re-opened fast), abort it so its result can't
    // race the new one onto the modal.
    previewAbortRef.current?.abort();
    const controller = new AbortController();
    previewAbortRef.current = controller;

    setPreviewInFlight(true);
    // Reset modal state up-front so a re-open after a previous
    // Cancel doesn't briefly flash the stale preview.
    setDiffPreview(null);
    setDiffCurrent(null);
    try {
      // Flush any unsaved editor edits to disk first. The Rust side
      // reads `<id>.md` from disk to seed the merge — without this
      // step a user with pending edits would lose them when the
      // post-merge body remounted the editor. `save` is a no-op
      // when the markdown matches the last-saved snapshot, so this
      // is cheap on the steady-state path.
      const live = editorRef.current?.getMarkdown();
      if (live !== undefined) {
        await save(live);
      }
      // Close the confirmation dialog and open the diff modal with
      // the spinner state — the preview fetch then resolves into it.
      setResummarizeOpen(false);
      setDiffOpen(true);
      // Fetch current body + preview in parallel. The current-body
      // fetch is local + fast; the preview can take 5–30s. Doing
      // them in parallel rather than sequentially shaves the local
      // read time off the perceived latency.
      const [currentBody, previewBody] = await Promise.all([
        invoke("heron_read_note", { vaultPath: vaultRoot, sessionId }),
        invoke("heron_resummarize_preview", {
          vaultPath: vaultRoot,
          sessionId,
        }),
      ]);
      // Drop the result if the user already clicked Cancel — the
      // modal is closed and updating state would flash stale data
      // if the user re-opens later.
      if (controller.signal.aborted) return;
      setDiffCurrent(currentBody);
      setDiffPreview(previewBody);
    } catch (err) {
      if (controller.signal.aborted) return;
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Re-summarize failed: ${message}`);
      // Close the diff modal on error — there's nothing to show.
      setDiffOpen(false);
    } finally {
      // Only clear the flag if THIS controller is still the active
      // one. If the user cancelled and started a fresh preview, a
      // new controller has already taken over — clearing here would
      // falsely claim that fresh fetch is no longer in flight.
      if (previewAbortRef.current === controller) {
        setPreviewInFlight(false);
      }
    }
  }, [vaultRoot, sessionId, save]);

  // PR-ξ (phase 76) Apply-button handler. Fires `heron_resummarize`
  // (which rotates `.md.bak` and writes the merged body), updates
  // the editor, and closes the diff modal. The diff library renders
  // a strict view of two strings — the user has already approved
  // the bytes; this just commits them.
  const onApplyResummarize = useCallback(async () => {
    if (!vaultRoot || !sessionId) {
      toast.error("No vault configured — cannot re-summarize.");
      return;
    }
    setApplyInFlight(true);
    try {
      const newBody = await invoke("heron_resummarize", {
        vaultPath: vaultRoot,
        sessionId,
      });
      setLoad({ kind: "ready", markdown: newBody });
      setLiveMarkdown(newBody);
      lastSavedRef.current = newBody;
      // Force the editor to remount so TipTap picks up the new body.
      setEditorContentKey(`resummarize:${Date.now()}`);
      // Refresh sidebar (no-op for content but keeps the pattern
      // consistent) and the backup pill — the writer just rotated
      // a `.md.bak` into existence.
      setSavedKey((k) => k + 1);
      await refreshBackup();
      toast.success("Re-summarized");
      setDiffOpen(false);
      setDiffPreview(null);
      setDiffCurrent(null);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Re-summarize failed: ${message}`);
    } finally {
      setApplyInFlight(false);
    }
  }, [vaultRoot, sessionId, refreshBackup]);

  // Cancel handler for the diff modal: drops the preview result and
  // closes the modal. We can't truly cancel an in-flight Tauri
  // `invoke` (the underlying summarizer call keeps running on the
  // Rust side until it completes), but the AbortController acts as
  // a "drop result on the floor" signal — the controller's
  // `aborted` flag prevents the late-resolving promise from
  // updating React state into a closed modal.
  //
  // We also clear `previewInFlight` immediately so the toolbar
  // Re-summarize button re-enables. Letting the user start a fresh
  // preview while the cancelled one is still grinding away is the
  // intended UX — the cancelled call's eventual result is discarded
  // by the aborted-check above.
  const onCancelDiff = useCallback(() => {
    previewAbortRef.current?.abort();
    previewAbortRef.current = null;
    setPreviewInFlight(false);
    setDiffOpen(false);
    setDiffPreview(null);
    setDiffCurrent(null);
  }, []);

  const onRestoreBackup = useCallback(async () => {
    if (!vaultRoot || !sessionId) return;
    try {
      const restored = await invoke("heron_restore_backup", {
        vaultPath: vaultRoot,
        sessionId,
      });
      setLoad({ kind: "ready", markdown: restored });
      setLiveMarkdown(restored);
      lastSavedRef.current = restored;
      setEditorContentKey(`restore:${Date.now()}`);
      setSavedKey((k) => k + 1);
      await refreshBackup();
      toast.success("Restored");
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Restore failed: ${message}`);
    }
  }, [vaultRoot, sessionId, refreshBackup]);

  const onTranscriptSeek = useCallback((seconds: number) => {
    playbackRef.current?.seekTo(seconds);
  }, []);

  // Editor content key includes the per-session prefix so a session
  // switch always remounts. We split this out as a memo so a stable
  // string is passed to TipTap when nothing reload-relevant changed.
  const editorKey = useMemo(
    () => `${vaultRoot}/${sessionId}#${editorContentKey}`,
    [vaultRoot, sessionId, editorContentKey],
  );

  return (
    <div className="h-screen flex flex-col">
      <div className="flex-1 flex min-h-0">
        <SessionsSidebar
          activeSessionId={sessionId}
          refreshKey={savedKey}
        />
        <main className="flex-1 overflow-y-auto">
          <div className="max-w-prose mx-auto px-6 py-6 space-y-4">
            <header className="flex items-center justify-between gap-3">
              <h1 className="text-xl font-semibold truncate" title={sessionId}>
                {sessionId ?? "(no session)"}
              </h1>
              <nav className="flex gap-2 items-center text-xs">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => setResummarizeOpen(true)}
                  disabled={!vaultRoot || !sessionId || resummarizeBusy}
                  title="Re-summarize this note (current body backs up to .md.bak)"
                >
                  <RotateCcw className="h-3.5 w-3.5" aria-hidden="true" />
                  Re-summarize
                </Button>
                <Link
                  to="/home"
                  className="px-2 py-1 rounded-md text-muted-foreground hover:underline"
                >
                  Home
                </Link>
              </nav>
            </header>

            {settingsError && (
              <div className="text-sm text-destructive">
                Settings load failed: {settingsError}
              </div>
            )}

            {!settingsReady && settingsLoading && (
              <div className="text-sm text-muted-foreground">
                Loading settings…
              </div>
            )}

            {settingsReady && !vaultRoot && !settingsError && (
              <div className="text-sm text-muted-foreground space-y-2">
                <p>No vault configured.</p>
                <p>
                  <Link to="/settings" className="underline">
                    Set one in Settings
                  </Link>{" "}
                  and return to this page.
                </p>
              </div>
            )}

            {settingsReady && vaultRoot && !sessionId && (
              <div className="text-sm text-muted-foreground">
                Pick a session from the sidebar.
              </div>
            )}

            {/* `.md.bak` Restore pill. Sits above the editor so it's
                visible without the user scrolling. Only renders when
                the backup actually exists. */}
            {backup !== null && vaultRoot && sessionId && (
              <div className="flex items-center justify-between gap-2 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-900">
                <span>
                  Backup from{" "}
                  <span className="font-mono">
                    {formatBackupTime(backup.created_at)}
                  </span>
                </span>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={onRestoreBackup}
                >
                  Restore
                </Button>
              </div>
            )}

            {vaultRoot && sessionId && (
              <Tabs defaultValue={initialTab}>
                <TabsList>
                  <TabsTrigger value="note">Note</TabsTrigger>
                  <TabsTrigger value="diagnostics">Diagnostics</TabsTrigger>
                </TabsList>
                <TabsContent value="note" className="space-y-6 mt-4">
                  {load.kind === "loading" && (
                    <div className="text-sm text-muted-foreground">
                      Loading note…
                    </div>
                  )}
                  {load.kind === "error" && (
                    <div className="text-sm text-destructive">
                      Failed to load note: {load.message}
                    </div>
                  )}
                  {load.kind === "ready" && (
                    <>
                      <section>
                        <NoteEditor
                          // Re-mount the editor on session change AND
                          // after re-summarize / restore so TipTap's
                          // internal state resets to the fresh body.
                          key={editorKey}
                          ref={editorRef}
                          initialMarkdown={load.markdown}
                          onUpdate={setLiveMarkdown}
                          onBlurSave={(md) => {
                            setLiveMarkdown(md);
                            void save(md);
                          }}
                        />
                      </section>
                      <section className="border-t border-border pt-6 space-y-3">
                        <h2 className="text-sm font-semibold text-muted-foreground uppercase tracking-wide">
                          Transcript
                        </h2>
                        <TranscriptView
                          markdown={liveMarkdown}
                          onSeek={onTranscriptSeek}
                        />
                      </section>
                    </>
                  )}
                </TabsContent>
                <TabsContent value="diagnostics" className="mt-4">
                  {cacheRoot === null ? (
                    <div className="text-sm text-muted-foreground">
                      Loading diagnostics…
                    </div>
                  ) : (
                    <DiagnosticsPanel
                      cacheRoot={cacheRoot}
                      sessionId={sessionId}
                      refreshKey={savedKey}
                    />
                  )}
                </TabsContent>
              </Tabs>
            )}
          </div>
        </main>
      </div>
      {/* Sticky playback bar at the bottom. Mounted whenever vault +
          session + cache root are all known so the bar can resolve
          its asset; otherwise we keep the strip empty so the layout
          doesn't shift when the resolve completes. */}
      {vaultRoot && sessionId && cacheRoot !== null ? (
        <PlaybackBar
          ref={playbackRef}
          vaultRoot={vaultRoot}
          cacheRoot={cacheRoot}
          sessionId={sessionId}
        />
      ) : (
        <div className="h-12 bg-muted/40 border-t border-border" />
      )}
      <ResummarizeDialog
        open={resummarizeOpen}
        onOpenChange={setResummarizeOpen}
        onConfirm={onConfirmResummarize}
        loading={previewInFlight}
      />
      {/* PR-ξ (phase 76) diff modal. The `open` -> `setDiffOpen(false)`
          path goes through `onCancelDiff` so an Escape / overlay
          click also aborts any in-flight preview invoke. */}
      <ResummarizeDiffModal
        open={diffOpen}
        onOpenChange={(next) => {
          if (next) {
            setDiffOpen(true);
          } else {
            onCancelDiff();
          }
        }}
        currentBody={diffCurrent}
        preview={diffPreview}
        applying={applyInFlight}
        onApply={onApplyResummarize}
      />
    </div>
  );
}
