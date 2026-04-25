/**
 * Re-summarize confirmation dialog — PR-ε (phase 67).
 *
 * Wraps Radix Dialog with the project's button styling. The actual
 * `heron_resummarize` invocation lives in the parent (`Review.tsx`)
 * so the dialog stays presentational; we accept an `onConfirm`
 * callback the parent fires when the primary button clicks.
 *
 * `loading` is parent-controlled rather than internal because the
 * parent also renders a Sonner toast during the call — keeping a
 * single source of truth for the in-flight state means the user
 * can't double-click the confirm button while the toast is
 * dismissing.
 */

import * as Dialog from "@radix-ui/react-dialog";

import { Button } from "./ui/button";

interface ResummarizeDialogProps {
  open: boolean;
  /** Called when the user closes via Escape or clicks outside. */
  onOpenChange: (open: boolean) => void;
  /** Primary-button click handler. The parent owns the actual call. */
  onConfirm: () => void;
  /** When `true`, the buttons are disabled — set by the parent for
   * the duration of the in-flight call. */
  loading: boolean;
}

export function ResummarizeDialog({
  open,
  onOpenChange,
  onConfirm,
  loading,
}: ResummarizeDialogProps) {
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        // Block Escape / overlay-click dismissal while a re-summarize
        // is in flight. Without this, the user could close the dialog
        // mid-call, click the toolbar button again, and queue a
        // concurrent re-summarize (the second would race the first's
        // `.md.bak` rotation and clobber it). The button-disabled
        // state isn't enough — Radix lets dismissal events through
        // even when the buttons are disabled.
        if (loading && !next) return;
        onOpenChange(next);
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
          aria-describedby="resummarize-dialog-description"
        >
          <Dialog.Title className="text-lg font-semibold">
            Re-summarize this note?
          </Dialog.Title>
          <Dialog.Description
            id="resummarize-dialog-description"
            className="text-sm text-muted-foreground"
          >
            The current body will be backed up to{" "}
            <code className="font-mono text-xs">&lt;id&gt;.md.bak</code> so you
            can restore it from this page.
          </Dialog.Description>
          <div className="flex justify-end gap-2 pt-2">
            <Button
              variant="ghost"
              onClick={() => onOpenChange(false)}
              disabled={loading}
            >
              Cancel
            </Button>
            <Button onClick={onConfirm} disabled={loading}>
              {loading ? "Summarizing…" : "Re-summarize"}
            </Button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
