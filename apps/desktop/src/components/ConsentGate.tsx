/**
 * Consent gate — modal that asks "Did you tell the room?" before any
 * recording starts.
 *
 * Phase 64 (PR-β). The disposition lives in `store/consent.ts`; this
 * component is purely presentational + dispatches actions to the
 * store.
 *
 * Mounted once at the App level (in `App.tsx`) so the same modal
 * instance services every `useConsentStore.requestConsent()` call —
 * including the snooze auto-reopen, which would otherwise need a
 * dedicated portal. Radix Dialog handles the focus trap, scroll lock,
 * and Escape-to-close — we override `onOpenChange` so dismissal
 * (Escape, click outside) routes to `cancel()` rather than dropping
 * the promise on the floor.
 */

import * as Dialog from "@radix-ui/react-dialog";

import { Button } from "./ui/button";
import { useConsentStore } from "../store/consent";

export default function ConsentGate() {
  // One subscription per primitive so unrelated store changes (the
  // disposition flipping snoozed→pending under the auto-reopen, for
  // instance) don't trigger spurious re-renders of the whole modal.
  const open = useConsentStore((s) => s.open);
  const confirm = useConsentStore((s) => s.confirm);
  const snooze = useConsentStore((s) => s.snooze);
  const cancel = useConsentStore((s) => s.cancel);

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        // Radix only fires `onOpenChange(false)` on user-driven close
        // (Esc, overlay click) — programmatic `open={false}` updates
        // do *not* call back, so we don't need to guard against
        // double-cancel here. Treat any user-driven dismiss as Cancel
        // so the awaiting `requestConsent()` promise resolves.
        if (!next) {
          cancel();
        }
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/40 backdrop-blur-sm" />
        <Dialog.Content
          className={
            "fixed left-1/2 top-1/2 w-[min(420px,90vw)] -translate-x-1/2 " +
            "-translate-y-1/2 rounded-lg bg-background p-6 shadow-xl " +
            "border border-border space-y-4"
          }
          aria-describedby="consent-gate-description"
        >
          <Dialog.Title className="text-lg font-semibold">
            Did you tell the room?
          </Dialog.Title>
          <Dialog.Description
            id="consent-gate-description"
            className="text-sm text-muted-foreground"
          >
            heron records meetings on your device. Make sure everyone in
            the call has consented to being recorded before you start.
          </Dialog.Description>
          {/* Radix Dialog auto-focuses the first focusable child on
              open, which is "Yes, go" — that's also the visual primary
              action, so we don't need an explicit `autoFocus`. */}
          <div className="flex flex-col gap-2 pt-2">
            <Button onClick={confirm} aria-label="Yes, start recording">
              Yes, go
            </Button>
            <Button variant="outline" onClick={snooze}>
              Remind me in 30s
            </Button>
            <Button variant="ghost" onClick={cancel}>
              Cancel
            </Button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
