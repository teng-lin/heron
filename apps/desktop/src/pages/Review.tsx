/**
 * Review route — `/review/:sessionId`.
 *
 * UI revamp PR 4: tabbed layout (Summary / Notes / Transcript / Actions
 * / Raw / Diagnostics) atop the unchanged save / re-summarize /
 * backup / playback plumbing. The shell-level RootLayout owns the
 * TitleBar + Sidebar; this page renders only its main content.
 *
 * Tab semantics:
 *  - Summary: read-only `react-markdown` rendering of the vault
 *    note. Same bytes as Notes; just prose mode for skimming.
 *  - Notes: editable TipTap NoteEditor.
 *  - Transcript: read-only TranscriptView, click-to-seek.
 *  - Actions: action-items section extracted from the markdown.
 *  - Raw: <pre> dump of the markdown source.
 *  - Diagnostics: existing DiagnosticsPanel.
 *
 * Summary vs Notes: in v1 the vault note IS the canonical document
 * (LLM-generated summary the user can edit in place). Splitting them
 * here is a UX affordance — read mode for skimming without the
 * editor chrome — not a data split. When Athena lands and the
 * daemon publishes a separate `Summary` artifact, this tab will
 * switch to that source.
 *
 * All existing behaviors (save-on-blur, ⌘S, re-summarize + diff
 * modal, .md.bak restore pill, click-transcript-to-seek, sticky
 * PlaybackBar, Diagnostics tab) are preserved verbatim — wrapped,
 * not rewritten, per the plan.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { toast } from "sonner";
import { RotateCcw } from "lucide-react";
import ReactMarkdown from "react-markdown";

import { DaemonDownBanner } from "../components/DaemonDownBanner";
import { NoteEditor, type NoteEditorHandle } from "../components/NoteEditor";
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

type EditorKey = string;

function formatBackupTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(d);
}

/**
 * Pull the bullet list under `## Action Items` (or `## Actions`)
 * out of the markdown. Pragmatic regex — the v1 LLM template emits
 * the heading verbatim. Returns `[]` when no section exists.
 */
function extractActionItems(markdown: string): string[] {
  const re = /^##\s+(?:Action items|Actions)\s*$/im;
  const match = markdown.match(re);
  if (!match || match.index === undefined) return [];
  const tail = markdown.slice(match.index + match[0].length);
  // Stop at the next `## ` heading (or EOF). Leading whitespace +
  // dash bullets are normalized to plain strings.
  const nextHeading = tail.match(/^##\s+/m);
  const section = nextHeading
    ? tail.slice(0, nextHeading.index)
    : tail;
  return section
    .split("\n")
    .map((line) => line.match(/^\s*[-*]\s+(.*)$/))
    .filter((m): m is RegExpMatchArray => m !== null)
    .map((m) => m[1].trim())
    .filter((s) => s.length > 0);
}

export default function Review() {
  const { sessionId } = useParams<{ sessionId: string }>();
  const [searchParams] = useSearchParams();
  const initialTab = searchParams.get("tab") === "diagnostics" ? "diagnostics" : "summary";
  const settings = useSettingsStore((s) => s.settings);
  const ensureLoaded = useSettingsStore((s) => s.ensureLoaded);
  const settingsLoading = useSettingsStore((s) => s.loading);
  const settingsError = useSettingsStore((s) => s.error);

  const [load, setLoad] = useState<LoadState>({ kind: "idle" });
  const [liveMarkdown, setLiveMarkdown] = useState<string>("");
  const [savedKey, setSavedKey] = useState(0);
  const editorRef = useRef<NoteEditorHandle | null>(null);
  const playbackRef = useRef<PlaybackBarHandle | null>(null);
  const lastSavedRef = useRef<string | null>(null);
  const saveGenRef = useRef(0);
  const [editorContentKey, setEditorContentKey] = useState<EditorKey>("v0");
  const [cacheRoot, setCacheRoot] = useState<string | null>(null);
  const [backup, setBackup] = useState<BackupInfo | null>(null);
  const [resummarizeOpen, setResummarizeOpen] = useState(false);
  const [previewInFlight, setPreviewInFlight] = useState(false);
  const [applyInFlight, setApplyInFlight] = useState(false);
  const [diffOpen, setDiffOpen] = useState(false);
  const [diffPreview, setDiffPreview] = useState<string | null>(null);
  const [diffCurrent, setDiffCurrent] = useState<string | null>(null);
  const previewAbortRef = useRef<AbortController | null>(null);
  const resummarizeBusy =
    resummarizeOpen || diffOpen || previewInFlight || applyInFlight;

  useEffect(() => {
    void ensureLoaded();
  }, [ensureLoaded]);

  useEffect(() => {
    return () => {
      previewAbortRef.current?.abort();
    };
  }, []);

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

  const settingsReady = settings !== null;
  const vaultRoot = settings?.vault_root ?? "";

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
      setBackup(null);
    }
  }, [vaultRoot, sessionId]);

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

  const onConfirmResummarize = useCallback(async () => {
    if (!vaultRoot || !sessionId) {
      toast.error("No vault configured — cannot re-summarize.");
      return;
    }
    previewAbortRef.current?.abort();
    const controller = new AbortController();
    previewAbortRef.current = controller;

    setPreviewInFlight(true);
    setDiffPreview(null);
    setDiffCurrent(null);
    try {
      const live = editorRef.current?.getMarkdown();
      if (live !== undefined) {
        await save(live);
      }
      setResummarizeOpen(false);
      setDiffOpen(true);
      const [currentBody, previewBody] = await Promise.all([
        invoke("heron_read_note", { vaultPath: vaultRoot, sessionId }),
        invoke("heron_resummarize_preview", {
          vaultPath: vaultRoot,
          sessionId,
        }),
      ]);
      if (controller.signal.aborted) return;
      setDiffCurrent(currentBody);
      setDiffPreview(previewBody);
    } catch (err) {
      if (controller.signal.aborted) return;
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Re-summarize failed: ${message}`);
      setDiffOpen(false);
    } finally {
      if (previewAbortRef.current === controller) {
        setPreviewInFlight(false);
      }
    }
  }, [vaultRoot, sessionId, save]);

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
      setEditorContentKey(`resummarize:${Date.now()}`);
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

  const editorKey = useMemo(
    () => `${vaultRoot}/${sessionId}#${editorContentKey}`,
    [vaultRoot, sessionId, editorContentKey],
  );

  const actionItems = useMemo(
    () => extractActionItems(liveMarkdown),
    [liveMarkdown],
  );

  return (
    <>
      <DaemonDownBanner />
      <div className="flex h-full flex-col">
        <div className="flex-1 overflow-y-auto">
          <div className="mx-auto max-w-4xl px-8 py-8">
            <header className="mb-6 flex items-end justify-between gap-3">
              <div>
                <p
                  className="font-mono text-xs uppercase tracking-[0.12em]"
                  style={{ color: "var(--color-ink-3)" }}
                >
                  Meeting
                </p>
                <h1
                  className="mt-1 truncate font-serif text-[24px] leading-tight"
                  style={{
                    color: "var(--color-ink)",
                    letterSpacing: "-0.01em",
                  }}
                  title={sessionId}
                >
                  {sessionId ?? "(no session)"}
                </h1>
              </div>
              <nav className="flex items-center gap-2 text-xs">
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
              <div className="space-y-2 text-sm text-muted-foreground">
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
                Pick a session from Home.
              </div>
            )}

            {backup !== null && vaultRoot && sessionId && (
              <div
                className="mb-4 flex items-center justify-between gap-2 rounded border px-3 py-2 text-xs"
                style={{
                  background: "var(--color-paper-2)",
                  borderColor: "var(--color-warn)",
                  color: "var(--color-ink-2)",
                }}
              >
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
                  <TabsTrigger value="summary">Summary</TabsTrigger>
                  <TabsTrigger value="notes">Notes</TabsTrigger>
                  <TabsTrigger value="transcript">Transcript</TabsTrigger>
                  <TabsTrigger value="actions">Actions</TabsTrigger>
                  <TabsTrigger value="raw">Raw</TabsTrigger>
                  <TabsTrigger value="diagnostics">Diagnostics</TabsTrigger>
                </TabsList>

                <TabsContent value="summary" className="mt-4">
                  {load.kind === "loading" && (
                    <div className="text-sm text-muted-foreground">
                      Loading summary…
                    </div>
                  )}
                  {load.kind === "error" && (
                    <div className="text-sm text-destructive">
                      Failed to load: {load.message}
                    </div>
                  )}
                  {load.kind === "ready" && (
                    <article className="prose prose-sm max-w-none">
                      <ReactMarkdown>{liveMarkdown}</ReactMarkdown>
                    </article>
                  )}
                </TabsContent>

                <TabsContent value="notes" className="mt-4">
                  {load.kind === "loading" && (
                    <div className="text-sm text-muted-foreground">
                      Loading note…
                    </div>
                  )}
                  {load.kind === "error" && (
                    <div className="text-sm text-destructive">
                      Failed to load: {load.message}
                    </div>
                  )}
                  {load.kind === "ready" && (
                    <NoteEditor
                      key={editorKey}
                      ref={editorRef}
                      initialMarkdown={load.markdown}
                      onUpdate={setLiveMarkdown}
                      onBlurSave={(md) => {
                        setLiveMarkdown(md);
                        void save(md);
                      }}
                    />
                  )}
                </TabsContent>

                <TabsContent value="transcript" className="mt-4">
                  <TranscriptView
                    markdown={liveMarkdown}
                    onSeek={onTranscriptSeek}
                  />
                </TabsContent>

                <TabsContent value="actions" className="mt-4">
                  {actionItems.length === 0 ? (
                    <p className="text-sm text-muted-foreground">
                      No action items extracted from this note.
                    </p>
                  ) : (
                    <ul className="list-disc space-y-2 pl-5 text-sm">
                      {actionItems.map((item, i) => (
                        <li key={i}>{item}</li>
                      ))}
                    </ul>
                  )}
                </TabsContent>

                <TabsContent value="raw" className="mt-4">
                  <pre
                    className="max-h-[60vh] overflow-auto rounded border p-3 font-mono text-xs"
                    style={{
                      background: "var(--color-paper-2)",
                      borderColor: "var(--color-rule)",
                      color: "var(--color-ink-2)",
                    }}
                  >
                    {liveMarkdown}
                  </pre>
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
        </div>
        {vaultRoot && sessionId && cacheRoot !== null ? (
          <PlaybackBar
            ref={playbackRef}
            vaultRoot={vaultRoot}
            cacheRoot={cacheRoot}
            sessionId={sessionId}
          />
        ) : (
          <div
            className="h-12 border-t"
            style={{
              background: "var(--color-paper-2)",
              borderColor: "var(--color-rule)",
            }}
          />
        )}
      </div>
      <ResummarizeDialog
        open={resummarizeOpen}
        onOpenChange={setResummarizeOpen}
        onConfirm={onConfirmResummarize}
        loading={previewInFlight}
      />
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
    </>
  );
}
