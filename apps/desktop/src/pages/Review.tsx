/**
 * Review route — `/review/:sessionId`.
 *
 * Shows a left sidebar (sessions list), the current session's `.md`
 * loaded into a TipTap editor, a transcript section beneath the
 * editor, and a placeholder playback strip at the bottom. Save
 * happens on editor blur and on ⌘S; both surface a Sonner toast.
 *
 * Out of scope (deferred per PR-γ scope doc):
 * - Audio playback strip (PR-γ′)
 * - Diagnostics tab (PR-γ″)
 * - Re-summarize button
 * - `.md.bak` rollback affordance
 * - Live WebSocket transcript ticks
 *
 * The vault path comes from `useSettingsStore` (read-only here; PR-δ
 * owns Settings.tsx). When no vault is configured we show a single
 * empty state instead of attempting to read a `.md` from `<empty>`.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { toast } from "sonner";

import { NoteEditor, type NoteEditorHandle } from "../components/NoteEditor";
import { SessionsSidebar } from "../components/SessionsSidebar";
import { TranscriptView } from "../components/TranscriptView";
import { invoke } from "../lib/invoke";
import { useSettingsStore } from "../store/settings";

type LoadState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; markdown: string }
  | { kind: "error"; message: string };

export default function Review() {
  const { sessionId } = useParams<{ sessionId: string }>();
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
  // Last successfully-saved markdown for this notePath. We compare
  // against this on save attempts so blurring an unchanged document
  // doesn't churn the disk or chime "Saved" repeatedly.
  const lastSavedRef = useRef<string | null>(null);
  // Monotonic save token. A save started later wins over an older
  // save still in flight — even if their POSIX renames land in the
  // wrong order, the older one's success path is gated on its token
  // still matching the latest.
  const saveGenRef = useRef(0);

  useEffect(() => {
    void ensureLoaded();
  }, [ensureLoaded]);

  // Settings has loaded successfully but the user hasn't picked a
  // vault yet (`vault_root` is the empty string). Distinct from
  // `settings === null && settingsLoading`, which is the initial
  // pre-load tick.
  const settingsReady = settings !== null;
  const vaultRoot = settings?.vault_root ?? "";

  // Load the current note whenever the session or vault changes.
  useEffect(() => {
    let cancelled = false;
    if (!vaultRoot || !sessionId) {
      setLoad({ kind: "idle" });
      setLiveMarkdown("");
      lastSavedRef.current = null;
      return () => {
        cancelled = true;
      };
    }
    setLoad({ kind: "loading" });
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
    return () => {
      cancelled = true;
    };
  }, [vaultRoot, sessionId]);

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

  return (
    <div className="h-screen flex flex-col">
      <div className="flex-1 flex min-h-0">
        <SessionsSidebar
          activeSessionId={sessionId}
          refreshKey={savedKey}
        />
        <main className="flex-1 overflow-y-auto">
          <div className="max-w-prose mx-auto px-6 py-6 space-y-8">
            <header className="flex items-center justify-between">
              <h1 className="text-xl font-semibold truncate" title={sessionId}>
                {sessionId ?? "(no session)"}
              </h1>
              <nav className="flex gap-2 text-xs">
                <button
                  type="button"
                  className="px-2 py-1 rounded-md bg-muted text-muted-foreground"
                  disabled
                  title="Diagnostics ships in PR-γ″"
                >
                  Diagnostics
                </button>
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

            {vaultRoot && sessionId && load.kind === "loading" && (
              <div className="text-sm text-muted-foreground">Loading note…</div>
            )}

            {vaultRoot && sessionId && load.kind === "error" && (
              <div className="text-sm text-destructive">
                Failed to load note: {load.message}
              </div>
            )}

            {vaultRoot && sessionId && load.kind === "ready" && (
              <>
                <section>
                  <NoteEditor
                    // Re-mount the editor when the session changes
                    // so TipTap's internal state resets cleanly.
                    key={`${vaultRoot}/${sessionId}`}
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
                  <TranscriptView markdown={liveMarkdown} />
                </section>
              </>
            )}
          </div>
        </main>
      </div>
      <div className="h-12 bg-muted/40 text-center text-sm flex items-center justify-center text-muted-foreground border-t border-border">
        Playback (γ′)
      </div>
    </div>
  );
}
