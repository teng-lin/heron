/**
 * Re-summarize side-by-side diff modal — PR-ξ (phase 76).
 *
 * Lifts the "diff view before accepting" item from `plan.md` §15
 * deferred-to-v1.1 list forward. The flow:
 *
 *   1. User clicks Re-summarize on `/review/:sessionId`.
 *   2. {@link ResummarizeDialog} renders the existing confirmation
 *      ("current body backs up to .md.bak"). On Confirm, the parent
 *      calls `heron_read_note` + `heron_resummarize_preview` in
 *      parallel — the preview command runs the summarizer + §10.3
 *      merge but never writes to disk.
 *   3. THIS COMPONENT renders the side-by-side diff. Left pane:
 *      current body. Right pane: post-merge preview.
 *   4. On Apply: parent calls `heron_resummarize` (rotate + write).
 *   5. On Cancel: parent drops the preview; nothing changes on disk.
 *
 * Why a separate modal vs. an inline diff under the editor: the
 * §10.3 merge can produce body changes that span the whole note, and
 * the existing editor + transcript + sidebar layout would compress a
 * useful side-by-side comparison to a column too narrow to read. The
 * 90vw / 80vh modal lets two columns of text breathe.
 *
 * Diff library: `react-diff-viewer-continued` (MIT). Picked over
 * `diff-match-patch` for word-level highlighting and a built-in
 * split view so we don't reinvent line-by-line rendering. The
 * package is JS-only — `cargo deny check` doesn't see it; the MIT
 * license is documented in the PR description.
 *
 * Loading state: the summarizer can take 5–30s on a real LLM, so the
 * modal renders a spinner overlay while {@link ResummarizeDiffModalProps.preview}
 * is `null`. The Cancel button stays clickable during that window so
 * the user can bail out — the parent's AbortController short-circuits
 * the in-flight invoke promise.
 *
 * Empty-summary edge case: if the summarizer returns an empty (or
 * whitespace-only) string, we collapse to a single "Cancel" view
 * with an explanatory message. There's nothing useful to apply.
 */

import { useEffect, useRef } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import ReactDiffViewer, { DiffMethod } from "react-diff-viewer-continued";

import { Button } from "./ui/button";

export interface ResummarizeDiffModalProps {
  /** Controls modal visibility. Parent owns this. */
  open: boolean;
  /** Closes the modal. Fired by Escape, overlay click, or Cancel. */
  onOpenChange: (open: boolean) => void;
  /**
   * Current `<id>.md` body the renderer fetched via `heron_read_note`.
   * Parent passes the same string the editor is mounted against so
   * the left pane shows what the user is about to lose.
   */
  currentBody: string | null;
  /**
   * Post-merge preview from `heron_resummarize_preview`. `null` while
   * the preview invoke is in flight; the modal renders a spinner
   * overlay until this resolves.
   */
  preview: string | null;
  /**
   * `true` once the user clicks Apply and the parent fires
   * `heron_resummarize`. Disables the buttons so a double-click
   * doesn't queue a second commit while the writer is rotating
   * `.md.bak`.
   */
  applying: boolean;
  /**
   * Apply button click handler. The parent fires `heron_resummarize`
   * (which writes), then closes the modal on success.
   */
  onApply: () => void;
}

/**
 * Whitespace-only check for the empty-summary branch. Trim once and
 * test for length so a preview consisting entirely of `\n` and spaces
 * (the v0 LLM prompt occasionally returned this on a malformed
 * transcript) collapses to the empty-result UI rather than rendering
 * an all-blank diff.
 */
function isBlank(s: string): boolean {
  return s.trim().length === 0;
}

export function ResummarizeDiffModal({
  open,
  onOpenChange,
  currentBody,
  preview,
  applying,
  onApply,
}: ResummarizeDiffModalProps) {
  // While a commit is in flight, block dismissal events (Escape /
  // overlay click) so the user can't close the modal mid-write and
  // queue a second Re-summarize that races the first's `.md.bak`
  // rotation. Mirrors the same guard ResummarizeDialog uses.
  const guardClose = (next: boolean) => {
    if (applying && !next) return;
    onOpenChange(next);
  };

  // Focus the Cancel button on open so Enter doesn't accidentally
  // commit when the user just wants to escape — Apply requires an
  // explicit click. We use a ref + an effect rather than `autoFocus`
  // so the focus moves the moment the modal mounts (Radix's portal
  // can otherwise hand focus to the close-X icon by default).
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    if (!open) return;
    // Run after Radix's own focus trap settles. A microtask is
    // enough; we don't need a full setTimeout.
    queueMicrotask(() => cancelRef.current?.focus());
  }, [open]);

  // Three mutually-exclusive view states the body of the modal renders:
  //   - `loading`: preview hasn't resolved yet → spinner.
  //   - `empty`:   summarizer returned blank → explanatory message.
  //   - `ready`:   we have both bodies → render the diff. The
  //                discriminated `kind` lets the JSX use TypeScript
  //                narrowing without re-stating the null checks.
  type ViewState =
    | { kind: "loading" }
    | { kind: "empty" }
    | { kind: "ready"; current: string; preview: string };
  const view: ViewState =
    preview === null
      ? { kind: "loading" }
      : isBlank(preview)
        ? { kind: "empty" }
        : currentBody === null
          ? { kind: "loading" } // current still racing; show spinner
          : { kind: "ready", current: currentBody, preview };
  const ready = view.kind === "ready";
  const previewEmpty = view.kind === "empty";

  return (
    <Dialog.Root open={open} onOpenChange={guardClose}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/40 backdrop-blur-sm" />
        <Dialog.Content
          className={
            "fixed left-1/2 top-1/2 max-w-5xl w-[90vw] h-[80vh] " +
            "-translate-x-1/2 -translate-y-1/2 rounded-lg bg-background " +
            "shadow-xl border border-border flex flex-col overflow-hidden"
          }
          aria-describedby="resummarize-diff-description"
        >
          {/* Top bar: title + Apply/Cancel. Sits above the diff body
              so the buttons are reachable without scrolling, even on
              long notes. */}
          <header className="flex items-center justify-between gap-3 px-4 py-3 border-b border-border">
            <div>
              <Dialog.Title className="text-base font-semibold">
                Re-summarize preview
              </Dialog.Title>
              <Dialog.Description
                id="resummarize-diff-description"
                className="text-xs text-muted-foreground"
              >
                Compare the current note (left) against the proposed
                re-summarized version (right). Apply rotates the
                current body to <code className="font-mono">.md.bak</code>{" "}
                and overwrites the note.
              </Dialog.Description>
            </div>
            <div className="flex gap-2 shrink-0">
              <Button
                ref={cancelRef}
                variant="ghost"
                onClick={() => onOpenChange(false)}
                disabled={applying}
              >
                Cancel
              </Button>
              <Button
                onClick={onApply}
                disabled={!ready || applying}
                title={
                  previewEmpty
                    ? "Summarizer returned an empty result"
                    : undefined
                }
              >
                {applying ? "Applying…" : "Apply"}
              </Button>
            </div>
          </header>

          {/* Body: the diff itself, or one of three placeholder states.
              The wrapping `<div>` owns the scroll so the header stays
              pinned. `min-h-0` is needed because flex children default
              to `min-height: auto` and would otherwise prevent the
              child's own scrollbar from appearing. */}
          <div className="flex-1 min-h-0 overflow-auto">
            {view.kind === "loading" && (
              <div
                className="flex h-full items-center justify-center"
                role="status"
                aria-live="polite"
              >
                <div className="flex flex-col items-center gap-3 text-sm text-muted-foreground">
                  <Spinner />
                  <span>Computing preview…</span>
                </div>
              </div>
            )}
            {view.kind === "empty" && (
              <div className="flex h-full items-center justify-center px-6">
                <div className="text-center text-sm text-muted-foreground max-w-prose">
                  Summarizer returned an empty result — nothing to
                  apply. Cancel and try again, or check your LLM
                  backend in Settings.
                </div>
              </div>
            )}
            {view.kind === "ready" && (
              <ReactDiffViewer
                oldValue={view.current}
                newValue={view.preview}
                splitView
                // Word-level diff so a small wording change doesn't
                // light up entire paragraphs as "changed". The
                // `WORDS` method ignores whitespace-only deltas,
                // matching how the user reads the doc.
                compareMethod={DiffMethod.WORDS}
                leftTitle="Current note"
                rightTitle="Proposed re-summarized version"
                // The default light theme reads fine on the app's
                // dark surfaces too — the diff bg colors come from
                // the library and stay legible. We don't toggle
                // `useDarkTheme` because the app doesn't yet have a
                // true dark-mode signal wired through (the Tailwind
                // `dark:` classes are the only signal, and the
                // library doesn't introspect that).
              />
            )}
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/**
 * Tiny inline spinner. We don't pull in a separate icon component for
 * one use site; the Tailwind `animate-spin` keyframe + an SVG circle
 * is the same primitive `lucide-react`'s `Loader2` would render under
 * the hood.
 */
function Spinner() {
  return (
    <svg
      className="animate-spin h-6 w-6 text-muted-foreground"
      xmlns="http://www.w3.org/2000/svg"
      fill="none"
      viewBox="0 0 24 24"
      aria-hidden="true"
    >
      <circle
        className="opacity-25"
        cx="12"
        cy="12"
        r="10"
        stroke="currentColor"
        strokeWidth="4"
      />
      <path
        className="opacity-75"
        fill="currentColor"
        d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z"
      />
    </svg>
  );
}
