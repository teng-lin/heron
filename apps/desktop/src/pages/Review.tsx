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
 *  - Actions: action-items rows — prefers `Meeting.action_items`
 *    (Tier 0 #3, structured rows with owner + due) and falls back
 *    to a regex bullet extractor against the markdown body for
 *    legacy notes. Read-only — write-back is Day 8–10.
 *  - Raw: <pre> dump of the markdown source.
 *  - Diagnostics: existing DiagnosticsPanel.
 *
 * On wide viewports the tabs share the column with a right-rail
 * `ProcessingRail` rendering `Meeting.processing` (Tier 0 #2:
 * model + token counts + cost). Omitted entirely when `processing`
 * is `undefined` — pre-summarize meetings shouldn't render `—`
 * placeholders. `Transcribed by` is intentionally absent because
 * `Frontmatter.stt_model` does not exist yet (separate backend
 * workstream, not in this PR's scope).
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

import { ActionItemsEditor } from "../components/ActionItemsEditor";
import { DaemonDownBanner } from "../components/DaemonDownBanner";
import { TagChip } from "../components/home/meetings-table";
import { NoteEditor, type NoteEditorHandle } from "../components/NoteEditor";
import { TranscriptView } from "../components/TranscriptView";
import { PlaybackBar, type PlaybackBarHandle } from "../components/PlaybackBar";
import { DiagnosticsPanel } from "../components/DiagnosticsPanel";
import { ResummarizeDialog } from "../components/ResummarizeDialog";
import { ResummarizeDiffModal } from "../components/ResummarizeDiffModal";
import { Button } from "../components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../components/ui/tabs";
import { invoke, type BackupInfo } from "../lib/invoke";
import type {
  ActionItem,
  Meeting,
  MeetingProcessing,
  Transcript,
} from "../lib/types";
import { useSettingsStore } from "../store/settings";

type LoadState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; markdown: string }
  | { kind: "error"; message: string };

type DaemonLoadState<T> =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; data: T }
  | { kind: "unavailable"; message: string };

const REVIEW_TABS = new Set([
  "summary",
  "notes",
  "transcript",
  "actions",
  "raw",
  "diagnostics",
]);

type EditorKey = string;

function formatBackupTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(d);
}

const ISO_DATE_RE = /^(\d{4})-(\d{2})-(\d{2})$/;
const ACTION_ITEM_DUE_FORMATTER = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  year: "numeric",
});

/**
 * `Frontmatter.action_items[].due` is `YYYY-MM-DD` (a calendar date,
 * not a timestamp). Parsing it through `new Date(iso)` would treat
 * the string as midnight UTC, which can drift to the prior calendar
 * day in negative-offset timezones. Pin the parts manually so the
 * formatted output matches the date the LLM emitted.
 *
 * Falls back to the raw string when the input doesn't match the
 * expected `YYYY-MM-DD` shape (defensive — a future LLM template
 * change shouldn't render `Invalid Date`).
 */
export function formatActionItemDue(iso: string): string {
  const match = ISO_DATE_RE.exec(iso);
  if (!match) return iso;
  const [, y, m, d] = match;
  const yi = Number(y);
  const mi = Number(m);
  const di = Number(d);
  const date = new Date(yi, mi - 1, di);
  // The `Date` constructor rolls invalid components silently —
  // `2026-02-31` becomes `Mar 3, 2026`, `2026-13-01` becomes
  // `Jan 1, 2027`. Reject anything where the round-trip doesn't
  // match the input so a buggy LLM template surfaces as raw text
  // instead of a confidently-wrong calendar date.
  if (
    date.getFullYear() !== yi ||
    date.getMonth() !== mi - 1 ||
    date.getDate() !== di
  ) {
    return iso;
  }
  return ACTION_ITEM_DUE_FORMATTER.format(date);
}

/**
 * Format `MeetingProcessing.summary_usd` for the right-rail. The
 * summarizer can emit very small amounts (a $0.00004 prompt-cache hit
 * shouldn't render as `$0.00`), so step the precision based on
 * magnitude rather than pinning two decimals. `Intl.NumberFormat`
 * with `maximumFractionDigits` doesn't hit this on its own — it would
 * collapse `0.00004` to `0` in the default `currency` style.
 */
export function formatProcessingCost(usd: number): string {
  if (!Number.isFinite(usd)) return "—";
  const abs = Math.abs(usd);
  // Bucket on the *post-rounding* magnitude so adjacent inputs across
  // a threshold render at consistent precision: `0.0009999` and
  // `0.001` both display as "$0.0010" instead of one rounding up
  // into the next bucket. Standard currency precision (2 digits)
  // applies once a value rounds to >= $0.01.
  let digits: number;
  if (abs === 0 || abs >= 0.005) {
    digits = 2;
  } else if (abs >= 0.00005) {
    digits = 4;
  } else {
    digits = 6;
  }
  return new Intl.NumberFormat(undefined, {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  }).format(usd);
}

const TOKEN_COUNT_FORMATTER = new Intl.NumberFormat(undefined);

/**
 * Pull the bullet list under `## Action Items` (or `## Actions`)
 * out of the markdown. Pragmatic regex — the v1 LLM template emits
 * the heading verbatim. Returns `[]` when no section exists.
 *
 * Tier 0 #3 of the UX redesign moves the canonical source for action
 * items off the markdown body and onto the `Meeting.action_items`
 * wire field. This regex extractor stays as a fallback for vault
 * notes that pre-date the structured emission (or for daemons that
 * haven't been upgraded yet) — see `selectActionItems`.
 *
 * Exported for unit-test consumption; not part of the public app
 * surface.
 */
export function extractActionItems(markdown: string): string[] {
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

/**
 * Uniform shape the Actions tab renders. `id` is stable for typed
 * rows (Tier 0 #3) and synthesized (`fallback:<index>`) for
 * regex-extracted bullets so React keys stay distinct.
 */
export interface ActionItemRow {
  id: string;
  text: string;
  owner: string | null;
  due: string | null;
  /**
   * Day 8–10 (action-item write-back). Mirrors `ActionItem.done` from
   * the wire. Always `false` for `structured: false` rows because the
   * regex-fallback path can't recover the flag — and the editor hides
   * the checkbox on those rows anyway, so the value is just a default
   * that satisfies the type.
   */
  done: boolean;
  /**
   * `true` when this row came from the structured
   * `Meeting.action_items` wire field (Tier 0 #3); `false` when it
   * was reconstructed from the markdown body via the legacy regex
   * extractor. The Actions tab uses this to gate the assignee / due
   * pill rendering — the regex path can't recover those.
   */
  structured: boolean;
}

/**
 * Tier 0 #3: prefer the structured `Meeting.action_items` wire
 * field, fall back to regex-extracted bullets when the field is
 * absent or empty. Empty / absent structured field on a finalized
 * note is the legacy-vault signal: pre-Tier-0-#3 frontmatter wrote
 * action items only into the markdown body, so the wire field stays
 * empty and we have to recover them from prose.
 *
 * Exported for testability — the precedence rule is the load-bearing
 * piece of this PR.
 */
export function selectActionItems(
  meeting: Meeting | null,
  markdown: string,
): ActionItemRow[] {
  const structured = meeting?.action_items ?? [];
  if (structured.length > 0) {
    return structured.map((item: ActionItem, idx: number) => ({
      // `id` is optional on the wire (back-compat with pre-Tier-0
      // daemons), so we synthesize a stable React key from the index
      // when it's missing rather than collapsing all rows onto the
      // same key.
      id: item.id ?? `legacy:${idx}`,
      text: item.text,
      owner: item.owner,
      due: item.due,
      // Day 8–10: `done` is required on the wire post-write-back.
      // Coalesce missing for back-compat with daemons that haven't
      // shipped the field yet — the read path treats it as `false`.
      done: item.done ?? false,
      structured: true,
    }));
  }
  return extractActionItems(markdown).map((text, idx) => ({
    id: `fallback:${idx}`,
    text,
    owner: null,
    due: null,
    done: false,
    structured: false,
  }));
}

function ProcessingRail({ processing }: { processing: MeetingProcessing }) {
  return (
    <aside
      aria-label="Processing"
      className="rounded border p-4"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
        color: "var(--color-ink-2)",
      }}
    >
      <h2
        className="mb-3 font-mono text-[10px] uppercase tracking-[0.12em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        Processing
      </h2>
      <dl className="grid grid-cols-[8rem_1fr] gap-y-2 text-xs">
        <dt style={{ color: "var(--color-ink-3)" }}>Summarized by</dt>
        <dd className="font-mono break-all" style={{ color: "var(--color-ink)" }}>
          {processing.model}
        </dd>
        <dt style={{ color: "var(--color-ink-3)" }}>Tokens in</dt>
        <dd className="font-mono" style={{ color: "var(--color-ink)" }}>
          {TOKEN_COUNT_FORMATTER.format(processing.tokens_in)}
        </dd>
        <dt style={{ color: "var(--color-ink-3)" }}>Tokens out</dt>
        <dd className="font-mono" style={{ color: "var(--color-ink)" }}>
          {TOKEN_COUNT_FORMATTER.format(processing.tokens_out)}
        </dd>
        <dt style={{ color: "var(--color-ink-3)" }}>Cost</dt>
        <dd className="font-mono" style={{ color: "var(--color-ink)" }}>
          {formatProcessingCost(processing.summary_usd)}
        </dd>
      </dl>
    </aside>
  );
}

export default function Review() {
  const { sessionId } = useParams<{ sessionId: string }>();
  const [searchParams, setSearchParams] = useSearchParams();
  // Controlled, not uncontrolled — when the user is already on
  // /review/{id} and the tray's "View diagnostics" toast pushes
  // ?tab=diagnostics, the route doesn't remount, so a defaultValue
  // would leave the visible tab stuck on "summary".
  const tabParam = searchParams.get("tab");
  const activeTab =
    tabParam !== null && REVIEW_TABS.has(tabParam) ? tabParam : "summary";
  const onTabChange = useCallback(
    (next: string) => {
      const params = new URLSearchParams(searchParams);
      if (next === "summary") {
        params.delete("tab");
      } else {
        params.set("tab", next);
      }
      setSearchParams(params, { replace: true });
    },
    [searchParams, setSearchParams],
  );
  const settings = useSettingsStore((s) => s.settings);
  const ensureLoaded = useSettingsStore((s) => s.ensureLoaded);
  const settingsLoading = useSettingsStore((s) => s.loading);
  const settingsError = useSettingsStore((s) => s.error);

  const [load, setLoad] = useState<LoadState>({ kind: "idle" });
  const [meetingLoad, setMeetingLoad] = useState<DaemonLoadState<Meeting>>({
    kind: "idle",
  });
  const [transcriptLoad, setTranscriptLoad] =
    useState<DaemonLoadState<Transcript>>({ kind: "idle" });
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

  useEffect(() => {
    let cancelled = false;
    if (!sessionId) {
      setMeetingLoad({ kind: "idle" });
      setTranscriptLoad({ kind: "idle" });
      return () => {
        cancelled = true;
      };
    }
    setMeetingLoad({ kind: "loading" });
    setTranscriptLoad({ kind: "loading" });

    invoke("heron_get_meeting", { meetingId: sessionId })
      .then((result) => {
        if (cancelled) return;
        if (result.kind === "ok") {
          setMeetingLoad({ kind: "ready", data: result.data });
        } else {
          setMeetingLoad({ kind: "unavailable", message: result.detail });
        }
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        setMeetingLoad({ kind: "unavailable", message });
      });

    invoke("heron_meeting_transcript", { meetingId: sessionId })
      .then((result) => {
        if (cancelled) return;
        if (result.kind === "ok") {
          setTranscriptLoad({ kind: "ready", data: result.data });
        } else {
          setTranscriptLoad({ kind: "unavailable", message: result.detail });
        }
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        setTranscriptLoad({ kind: "unavailable", message });
      });

    return () => {
      cancelled = true;
    };
  }, [sessionId]);

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

  const meeting = meetingLoad.kind === "ready" ? meetingLoad.data : null;
  const actionItems = useMemo(
    () => selectActionItems(meeting, liveMarkdown),
    [meeting, liveMarkdown],
  );
  const title = meeting?.title ?? sessionId ?? "(no session)";
  const subtitle = meeting
    ? `${meeting.platform.replace(/_/g, " ")} · ${meeting.status}`
    : "Meeting";

  return (
    <>
      <DaemonDownBanner />
      <div className="flex h-full flex-col">
        <div className="flex-1 overflow-y-auto">
          <div className="mx-auto max-w-6xl px-8 py-8">
            <header className="mb-6 flex items-end justify-between gap-3">
              <div>
                <p
                  className="font-mono text-xs uppercase tracking-[0.12em]"
                  style={{ color: "var(--color-ink-3)" }}
                >
                  {subtitle}
                </p>
                <h1
                  className="mt-1 truncate font-serif text-[24px] leading-tight"
                  style={{
                    color: "var(--color-ink)",
                    letterSpacing: "-0.01em",
                  }}
                  title={title}
                >
                  {title}
                </h1>
                {/*
                  LLM-emitted topic tags from the note's frontmatter
                  (Tier 0 #1). Read-only here — Home owns the
                  tag-as-filter UX; on Review they're decorative
                  metadata next to the title. Coalesce optional
                  `tags` (back-compat with pre-Tier-0-#1 daemons).
                */}
                {meeting && (meeting.tags ?? []).length > 0 && (
                  <div className="mt-2 flex flex-wrap items-center gap-1">
                    {/*
                      `${tag}-${index}` (rather than `tag` alone) so a
                      duplicate tag emitted by the LLM summarizer
                      doesn't collide on React's reconciliation key —
                      same shape used by the Home meetings-table chip
                      strip.
                    */}
                    {(meeting.tags ?? []).map((tag, index) => (
                      <TagChip key={`${tag}-${index}`} tag={tag} />
                    ))}
                  </div>
                )}
                {meetingLoad.kind === "unavailable" && (
                  <p className="mt-1 text-xs text-muted-foreground">
                    Metadata unavailable: {meetingLoad.message}
                  </p>
                )}
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
              <div className="lg:grid lg:grid-cols-[minmax(0,1fr)_18rem] lg:gap-6">
                <div className="min-w-0">
                  <Tabs value={activeTab} onValueChange={onTabChange}>
                    <TabsList>
                      <TabsTrigger value="summary">Summary</TabsTrigger>
                      <TabsTrigger value="notes">Notes</TabsTrigger>
                      <TabsTrigger value="transcript">Transcript</TabsTrigger>
                      <TabsTrigger value="actions">Actions</TabsTrigger>
                      <TabsTrigger value="raw">Raw</TabsTrigger>
                      <TabsTrigger value="diagnostics">
                        Diagnostics
                      </TabsTrigger>
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
                      {transcriptLoad.kind === "loading" ? (
                        <div className="text-sm text-muted-foreground">
                          Loading transcript…
                        </div>
                      ) : transcriptLoad.kind === "ready" &&
                        transcriptLoad.data.segments.length > 0 ? (
                        <TranscriptView
                          segments={transcriptLoad.data.segments}
                          onSeek={onTranscriptSeek}
                        />
                      ) : (
                        <>
                          {transcriptLoad.kind === "unavailable" && (
                            <p className="mb-3 text-xs text-muted-foreground">
                              Daemon transcript unavailable:{" "}
                              {transcriptLoad.message}. Showing transcript
                              parsed from the note.
                            </p>
                          )}
                          <TranscriptView
                            markdown={liveMarkdown}
                            onSeek={onTranscriptSeek}
                          />
                        </>
                      )}
                    </TabsContent>

                    <TabsContent value="actions" className="mt-4">
                      {load.kind === "loading" && (
                        <div className="text-sm text-muted-foreground">
                          Loading action items…
                        </div>
                      )}
                      {load.kind === "error" && (
                        <div className="text-sm text-destructive">
                          Failed to load: {load.message}
                        </div>
                      )}
                      {load.kind === "ready" && actionItems.length === 0 && (
                        <p className="text-sm text-muted-foreground">
                          No action items extracted from this note.
                        </p>
                      )}
                      {load.kind === "ready" && actionItems.length > 0 && (
                        <ActionItemsEditor
                          rows={actionItems}
                          vaultPath={vaultRoot}
                          meetingId={sessionId}
                          onError={(message) => toast.error(message)}
                        />
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
                </div>
                {meeting?.processing !== undefined && (
                  <div className="mt-6 lg:mt-0">
                    <ProcessingRail processing={meeting.processing} />
                  </div>
                )}
              </div>
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
